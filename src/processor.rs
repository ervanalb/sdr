use crate::band_info::{BandsInfo, ChannelGroupInfo, ChannelInfo};
use crate::dsp::{Fft, Ifft, OverlapExpand, Rechunker, hann_window};
use crate::hardware::{HardwareResult, ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId};
use crate::id_factory::IdFactory;
use crate::modulation::{Demodulator, ModulationParameters};
use num_complex::Complex;
use rayon::prelude::*;
use std::any::Any;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::{DerefMut, Range};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STREAM_TARGET_BIN_SIZE: f64 = 2_500.0; // 2.5 KHz
const STREAM_TARGET_OUTPUT_PERIOD: f64 = 0.01; // 100 chunks per second
const STREAM_MIN_MAX_TIME_CONSTANT: f64 = 1.;
const STREAM_PEAK_TIME_CONSTANT: f64 = 1.;
const STREAM_OFFSET_REJECT_TIME_CONSTANT: f64 = 0.1;
const CHANNEL_MARGIN: f64 = 0.05; // Add 5% of channel bandwidth as margin on each side

// IDS //

pub type ChannelId = usize;

// DESCRIPTORS //

#[derive(Debug, Clone)]
pub struct ChannelDescriptor {
    pub sample_rate: f64,
    pub name: String,
    pub center_frequency: f64,
    pub bandwidth: f64,
    pub tuning_error: f64,
    pub start_time: Instant,
    pub modulation: Box<dyn ModulationParameters>,
}

// RESULTS //

#[derive(Debug, Default)]
pub struct ProcessingResult {
    pub receive_streams: BTreeMap<StreamId, StreamProcessingResult>,
}

#[derive(Debug)]
pub struct StreamProcessingResult {
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    pub spectrum_len: usize, // XXX move to descriptor
    pub waterfall_rows: Vec<WaterfallRow>,
    pub channels: BTreeMap<ChannelId, ChannelResult>,
}

pub struct WaterfallRow {
    pub time: Instant,
    pub spectrum: Vec<f32>,
    pub min: f32,
    pub max: f32,
    pub peak: f32,
    pub overload: bool,
}

impl std::fmt::Debug for WaterfallRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaterfallRow")
            .field("time", &self.time)
            .field("spectrum.len()", &self.spectrum.len())
            .field("min", &self.min)
            .field("max", &self.max)
            .field("peak", &self.peak)
            .field("overload", &self.overload)
            .finish()
    }
}

#[derive(Debug)]
pub struct ChannelResult {
    pub descriptor: Arc<ChannelDescriptor>,
    pub demodulation: Vec<Box<dyn Any + Send>>,
}

// PROCESSOR //

pub struct Processor {
    bands_info: Rc<RefCell<BandsInfo>>, // TODO maybe Rc<RefCell> is sufficient?
    streams: BTreeMap<StreamId, StreamProcessor>,
}

impl Processor {
    pub fn new(bands_info: Rc<RefCell<BandsInfo>>) -> Processor {
        Processor {
            bands_info,
            streams: BTreeMap::default(),
        }
    }

    pub fn process(&mut self, data: &HardwareResult) -> ProcessingResult {
        // Remove any streams that are no longer present from the hardware
        self.streams
            .retain(|k, _| data.receive_streams.contains_key(&k));

        // Process each hardware stream, adding it if necessary
        let receive_streams = data
            .receive_streams
            .iter()
            .map(|(stream_id, stream)| {
                let processor = self.streams.entry(*stream_id).or_insert_with(|| {
                    let channels = { &self.bands_info.borrow().channels };
                    StreamProcessor::new(stream.descriptor.clone(), channels)
                });

                (*stream_id, processor.process(&stream.data))
            })
            .collect();

        ProcessingResult { receive_streams }
    }
}

pub struct StreamProcessor {
    descriptor: Arc<ReceiveStreamDescriptor>,
    rechunker: Rechunker<Complex<f32>>,
    processor: StreamChunkProcessor,
}

impl StreamProcessor {
    pub fn new(descriptor: Arc<ReceiveStreamDescriptor>, channels: &[ChannelGroupInfo]) -> Self {
        let processor = StreamChunkProcessor::new(
            descriptor.frequency,
            descriptor.sample_rate,
            STREAM_TARGET_BIN_SIZE,
            STREAM_TARGET_OUTPUT_PERIOD,
            STREAM_MIN_MAX_TIME_CONSTANT,
            STREAM_PEAK_TIME_CONSTANT,
            STREAM_OFFSET_REJECT_TIME_CONSTANT,
            descriptor.start_time,
            channels,
        );

        StreamProcessor {
            descriptor,
            rechunker: Rechunker::new(processor.chunk_size),
            processor,
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.processor.chunk_size
    }

    pub fn process(&mut self, data: &[ReceiveStreamChunk]) -> StreamProcessingResult {
        let mut result = StreamProcessingResult {
            descriptor: self.descriptor.clone(),
            spectrum_len: self.processor.fft.size(),
            waterfall_rows: Vec::default(),
            channels: BTreeMap::default(),
        };
        for msg in data {
            self.rechunker.process(&msg.iq_data, |chunk| {
                let channel_demodulations = self.processor.process_chunk(&chunk, msg.time);
                result.waterfall_rows.push(WaterfallRow {
                    time: msg.time,
                    spectrum: self.processor.spectrum.clone(),
                    min: self.processor.min,
                    max: self.processor.max,
                    peak: self.processor.peak,
                    overload: self.processor.overload,
                });
                for (channel_id, demodulation) in channel_demodulations {
                    let channel = self
                        .processor
                        .channels
                        .iter()
                        .find(|c| c.id == channel_id)
                        .unwrap();
                    let channel_result =
                        result
                            .channels
                            .entry(channel_id)
                            .or_insert_with(|| ChannelResult {
                                descriptor: channel.descriptor.clone(),
                                demodulation: vec![],
                            });
                    channel_result.demodulation.push(demodulation);
                }
            });
        }
        result
    }
}

struct StreamChunkProcessor {
    chunk_size: usize,
    overlap_expand: OverlapExpand<Complex<f32>>,
    hann_window: Vec<f32>,
    fft: Fft,
    counter: u32,
    spectrum: Vec<f32>,
    min: f32,
    max: f32,
    peak: f32,
    overload_t: Instant,
    overload: bool,
    min_max_alpha: f32,
    peak_alpha: f32,
    peak_time_constant: f64,
    offset: Complex<f32>,
    offset_reject_alpha: f32,
    channels: Vec<Channel>,
    _channel_id_factory: IdFactory,
}

impl StreamChunkProcessor {
    pub fn new(
        frequency: f64,
        sample_rate: f64,
        target_bin_size: f64,
        target_output_period: f64,
        min_max_time_constant: f64,
        peak_time_constant: f64,
        offset_reject_time_constant: f64,
        time: Instant,
        channel_group_info: &[ChannelGroupInfo],
    ) -> Self {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (sample_rate / target_bin_size).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        // Pick a chunk size that gets us close to target_output_period
        // but which contains an integer number of FFTs
        let chunk_size =
            (sample_rate * target_output_period / fft_size as f64).round() as usize * fft_size;

        let output_period = chunk_size as f64 / sample_rate;

        let overlap_expand = OverlapExpand::new(fft_size);
        let fft = Fft::new(fft_size);

        let min_max_alpha = (output_period / (min_max_time_constant + output_period)) as f32;
        let peak_alpha = (output_period / (peak_time_constant + output_period)) as f32;
        let offset_reject_alpha =
            (output_period / (offset_reject_time_constant + output_period)) as f32;

        // Build channels from ChannelGroupInfo
        let min_freq = frequency - 0.5 * sample_rate;
        let max_freq = frequency + 0.5 * sample_rate;

        // This refcell is very silly,
        // why don't the lifetimes work?
        let id_factory_refcell = RefCell::new(IdFactory::default());
        let channels: Vec<_> = channel_group_info
            .iter()
            .filter(|channel_group_info| {
                channel_group_info.max > min_freq && channel_group_info.min < max_freq
            })
            .flat_map(|channel_group_info| {
                let mut channel_group = None;
                let fft = &fft;
                let id_factory_refcell = &id_factory_refcell;
                channel_group_info.iter().filter_map(move |channel_info| {
                    Self::maybe_create_channel(
                        &mut channel_group,
                        &channel_info,
                        &channel_group_info,
                        frequency,
                        sample_rate,
                        fft,
                        time,
                        id_factory_refcell.borrow_mut(),
                    )
                })
            })
            .collect();
        let channel_id_factory = id_factory_refcell.into_inner();

        Self {
            chunk_size,
            overlap_expand,
            hann_window: hann_window(fft_size),
            fft,
            counter: 0,
            spectrum: vec![0.; fft_size],
            min: std::f32::NAN,
            max: std::f32::NAN,
            peak: std::f32::NAN,
            overload_t: time - Duration::from_secs_f64(2. * peak_time_constant),
            overload: false,
            min_max_alpha,
            peak_alpha,
            peak_time_constant,
            offset: Default::default(),
            offset_reject_alpha,
            channels,
            _channel_id_factory: channel_id_factory,
        }
    }

    fn maybe_create_channel(
        channel_group: &mut Option<ChannelGroup>,
        channel_info: &ChannelInfo,
        channel_group_info: &ChannelGroupInfo,
        stream_center_frequency: f64,
        sample_rate: f64,
        fft: &Fft,
        time: Instant,
        mut id_factory: impl DerefMut<Target = IdFactory>,
    ) -> Option<Channel> {
        let center_frequency = channel_info.center_frequency;
        let left_freq =
            center_frequency - stream_center_frequency - 0.5 * channel_group_info.bandwidth;
        let right_freq =
            center_frequency - stream_center_frequency + 0.5 * channel_group_info.bandwidth;

        if left_freq < -0.5 * sample_rate || right_freq > 0.5 * sample_rate {
            return None;
        }

        // This channel is probably within range
        let ChannelGroup {
            signal_bin_count,
            margin_bin_count,
            ifft,
        } = channel_group.get_or_insert_with(|| {
            // Compute channel width, in bins
            let signal_bin_count = fft.size() as f64 * channel_group_info.bandwidth / sample_rate;
            let signal_bin_count = (signal_bin_count / 2.).max(1.).ceil() as usize * 2; // Round up to even size of at least 2
            let margin_bin_count =
                fft.size() as f64 * channel_group_info.bandwidth * CHANNEL_MARGIN / sample_rate;
            let margin_bin_count = margin_bin_count.max(1.).ceil() as usize;
            let ifft = Ifft::new(signal_bin_count + 2 * margin_bin_count);

            ChannelGroup {
                signal_bin_count,
                margin_bin_count,
                ifft,
            }
        });

        let center_bin =
            fft.freq2bin((channel_info.center_frequency - stream_center_frequency) / sample_rate);
        // If left bin < 0, skip this channel
        let left_bin = center_bin.checked_sub(ifft.dc_bin() - *margin_bin_count)?;
        let right_bin = left_bin + *signal_bin_count;
        if right_bin > fft.size() {
            // If right bin > fft.size(), skip this channel
            return None;
        }
        let bins = left_bin..right_bin;

        let tuning_error = fft.bin2freq(center_bin) * sample_rate + stream_center_frequency
            - channel_info.center_frequency;

        // Phasor to correct the phase shift caused by overlapping chunks.
        // General form: e^(j * f_shift * 2pi * t_overlap)
        let phasor: f32 = (-1_f32).powi((center_bin % 2) as i32);
        // General form: [phasor^0, phasor^1, phasor^2, ...]
        let phasors = [1., phasor];

        let output_sample_rate = sample_rate * ifft.size() as f64 / fft.size() as f64;

        let descriptor = ChannelDescriptor {
            sample_rate: output_sample_rate,
            name: channel_info.name.clone(),
            center_frequency: channel_info.center_frequency,
            bandwidth: channel_group_info.bandwidth,
            tuning_error,
            start_time: time,
            modulation: channel_group_info.modulation.clone(),
        };

        Some(Channel {
            id: id_factory.create(),
            descriptor: Arc::new(descriptor),
            demodulator: channel_group_info
                .modulation
                .create_demodulator(ifft.size()),
            bins,
            margin_bin_count: *margin_bin_count,
            ifft_size: ifft.size(),
            phasors,
        })
    }

    pub fn process_chunk(
        &mut self,
        samples: &[Complex<f32>],
        time: Instant,
    ) -> BTreeMap<ChannelId, Box<dyn Any + Send>> {
        assert_eq!(samples.len(), self.chunk_size);
        // TODO custom error type

        // Find peak value of data
        let new_peak = samples
            .iter()
            .map(|s| s.re.abs().max(s.im.abs()))
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap();
        // LPF peak
        if new_peak < self.peak {
            self.peak = log_mix_f32(self.peak, new_peak, self.peak_alpha);
        } else {
            self.peak = new_peak;
        }

        // Update overload flag
        if self.peak >= 1. {
            self.overload_t = time;
            self.overload = true;
        } else if time.duration_since(self.overload_t).as_secs_f64() >= self.peak_time_constant {
            self.overload = false;
        }

        let mut offset_accumulator = Complex::<f32>::ZERO;
        self.spectrum.fill(0.);

        // Process incoming data into overlapping chunks
        let mut buffer = self.overlap_expand.process(&samples);

        let fft_count = self.chunk_size / self.fft.size();
        for one_fft in buffer.chunks_exact_mut(self.fft.size()) {
            // Apply Hann window
            for (sample, win) in one_fft.iter_mut().zip(self.hann_window.iter()) {
                *sample *= win;
            }
        }

        // FFT each chunk
        self.fft.process_inplace(&mut buffer);

        // Measure & apply offset correction
        for one_fft in buffer.chunks_exact_mut(self.fft.size()) {
            offset_accumulator += (1. / 6.)
                * (-one_fft[self.fft.dc_bin() - 1] + 2. * one_fft[self.fft.dc_bin()]
                    - one_fft[self.fft.dc_bin() + 1]);
            one_fft[self.fft.dc_bin()] -= self.offset;
            one_fft[self.fft.dc_bin() - 1] += 0.5 * self.offset;
            one_fft[self.fft.dc_bin() + 1] += 0.5 * self.offset;
        }

        // Accumulate power, for waterfall
        for one_fft in buffer.chunks_exact(self.fft.size()) {
            for (&sample, spectrum_sample) in one_fft.iter().zip(self.spectrum.iter_mut()) {
                *spectrum_sample += sample.re * sample.re + sample.im * sample.im;
            }
        }

        let inv_fft_count = 1.0 / (fft_count as f32);
        for sample in self.spectrum.iter_mut() {
            *sample *= inv_fft_count;
        }

        let new_offset = offset_accumulator * inv_fft_count;

        if self.offset_reject_alpha.is_finite() {
            self.offset = self.offset + self.offset_reject_alpha * (new_offset - self.offset);
        }

        let new_min = self
            .spectrum
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap()
            .max(1e-10);
        let new_max = self
            .spectrum
            .iter()
            .copied()
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap()
            .max(1e-10);

        // Compute min/max with LPF
        if self.min <= self.max {
            // Normal case:

            // LPF in log space
            self.min = log_mix_f32(self.min, new_min, self.min_max_alpha);
            self.max = log_mix_f32(self.max, new_max, self.min_max_alpha);
        } else {
            // On startup, or if something goes wrong:
            self.min = new_min;
            self.max = new_max;
        }

        // Drive each channel
        let channel_results: BTreeMap<ChannelId, Box<dyn Any + Send>> = self
            .channels
            .par_iter_mut()
            .filter_map(|channel| {
                // Aggregate per-channel FFT data in this thread
                let chunk_slice_len = channel.bins.end - channel.bins.start;
                let mut fft_buffer = Vec::with_capacity(chunk_slice_len * fft_count);
                let mut counter = self.counter;
                for one_fft in buffer.chunks_exact(self.fft.size()) {
                    // This is a hot loop
                    let start = fft_buffer.len() + channel.margin_bin_count;
                    let end = fft_buffer.len() + channel.ifft_size - channel.margin_bin_count;
                    // Resize to accomodate IFFT, filling margin with zeros
                    fft_buffer.resize(fft_buffer.len() + channel.ifft_size, Complex::ZERO);
                    // Copy in FFT data
                    fft_buffer[start..end].clone_from_slice(&one_fft[channel.bins.clone()]);

                    // Apply phase correction due to overlap
                    let phasor = channel.phasors[counter as usize];
                    for sample in fft_buffer[start..end].iter_mut() {
                        *sample *= phasor;
                    }
                    counter = (counter + 1) % 2;
                }

                // Process this channel
                Some((
                    channel.id,
                    channel.demodulator.process(time, fft_buffer, self.min)?,
                ))
            })
            .collect();

        self.counter = (self.counter + (fft_count % 2) as u32) % 2;

        channel_results
    }
}

struct ChannelGroup {
    signal_bin_count: usize,
    margin_bin_count: usize,
    ifft: Ifft,
}

pub struct Channel {
    id: ChannelId,
    descriptor: Arc<ChannelDescriptor>,
    demodulator: Box<dyn Demodulator>,
    bins: Range<usize>,
    margin_bin_count: usize,
    ifft_size: usize,
    phasors: [f32; 2],
}

// HELPER FUNCTIONS //

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    (x * (y / x).powf(a)).max(1e-10)
    //(x * powf_approx(y / x, a)).max(1e-10)
}

#[cfg(test)]
mod tests {
    use crate::modulation::fm::FmModulationParameters;

    use super::*;

    #[test]
    fn test_channel_processing() {
        // Parameters from user spec
        let sample_rate = 20_000_000.0; // 20 MSPS
        let center_frequency = 100_000_000.0; // 100 MHz

        // Channel: 100.7 MHz center freq, 200 KHz bandwidth
        let channel_center_frequency = 100_700_000.0; // 100.7 MHz
        let channel_bandwidth = 200_000.0; // 200 KHz

        // How many total channels to capture
        let channels = 4;
        let step = 200_000.0;

        let channel_group_info = vec![ChannelGroupInfo {
            name: "Test Channel".to_string(),
            min: channel_center_frequency,
            max: channel_center_frequency + step * (channels - 1) as f64,
            step,
            naming: crate::band_info::NamingConvention::Number,
            bandwidth: channel_bandwidth,
            modulation: Box::new(FmModulationParameters {
                squelch_db: -100.,
                squelch_hysteresis_db: 0.,
            }),
        }];

        let time = Instant::now();

        let mut processor = StreamProcessor::new(
            Arc::new(ReceiveStreamDescriptor {
                device_id: "Test".to_string(),
                stream_index: 0,
                start_time: time,
                frequency: center_frequency,
                sample_rate,
            }),
            &channel_group_info,
        );

        // Get the chunk size
        let chunk_count = 40;
        println!(
            "Chunk size: {}, chunk count: {}",
            processor.chunk_size(),
            chunk_count
        );

        // Generate signal: e^(j*w*t) where w = channel_center_frequency * 2π
        // and t = sample_index / sample_rate
        let iq_data: Vec<Complex<f32>> = (0..processor.chunk_size() * chunk_count)
            .map(|i| {
                let t = i as f64 / sample_rate;
                let omega = channel_center_frequency * std::f64::consts::TAU;
                Complex::cis(((omega * t) % std::f64::consts::TAU) as f32)
            })
            .collect();

        let results = processor.process(&[ReceiveStreamChunk { time, iq_data }]);

        let mut channels_iq_data = results.channels.into_values().filter_map(|channel| {
            (channel.descriptor.center_frequency == channel_center_frequency).then(|| channel.iq)
        });
        let iq_data = channels_iq_data.next().expect("Could not find channel!");
        assert!(
            channels_iq_data.next().is_none(),
            "Found multiple channels!"
        );
        let iq_data: Vec<_> = iq_data
            .into_iter()
            .map(|iq_row| iq_row.data)
            .flatten()
            .collect();

        if iq_data.is_empty() {
            println!("No channel output received");
        } else {
            let channel_output_path = "channel_test_output.raw";
            let channel_bytes: Vec<u8> = iq_data
                .iter()
                .flat_map(|c| {
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(&c.re.to_le_bytes());
                    bytes.extend_from_slice(&c.im.to_le_bytes());
                    bytes
                })
                .collect();
            std::fs::write(channel_output_path, channel_bytes)
                .expect("Failed to write channel output");
            println!("Total channel IQ data length: {} samples", iq_data.len(),);
            println!("Channel output saved to {}", channel_output_path);
        }
    }
}
