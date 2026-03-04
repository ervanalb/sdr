use rayon::prelude::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::ops::{DerefMut, Range};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use num_complex::Complex;

use crate::band_info::{BandsInfo, ChannelGroupInfo, ChannelInfo};
use crate::dsp::{Fft, Ifft, OverlapExpand, OverlapReduce, Rechunker, hann_window};
use crate::hardware::{
    HardwareHandler, ReceiveStreamDescriptor, ReceiveStreamMessage, StreamHandler,
};
use crate::id_factory::IdFactory;

pub type ReceiveStreamId = usize;
pub type ChannelId = usize;

const STREAM_TARGET_BIN_SIZE: f64 = 2_500.0; // 2.5 KHz
const STREAM_TARGET_OUTPUT_PERIOD: f64 = 0.01; // 100 chunks per second
const STREAM_MIN_MAX_TIME_CONSTANT: f64 = 1.;
const STREAM_OFFSET_REJECT_TIME_CONSTANT: f64 = 0.1;

pub trait ProcessedDataHandler {
    fn add_waterfall_row(
        &mut self,
        stream_id: ReceiveStreamId,
        time: Instant,
        spectrum: &[f32],
        min: f32,
        max: f32,
    );

    fn add_channel_iq(
        &mut self,
        stream_id: ReceiveStreamId,
        channel_id: ChannelId,
        time: Instant,
        iq_data: &[f32],
    );
}

pub struct Processor<H: ProcessedDataHandler> {
    bands_info: Arc<Mutex<BandsInfo>>, // TODO maybe Rc<RefCell> is sufficient?
    pub handler: H,
    receive_stream_id_factory: IdFactory,
}

impl<H: ProcessedDataHandler> Processor<H> {
    pub fn new(bands_info: Arc<Mutex<BandsInfo>>, handler: H) -> Processor<H> {
        Processor {
            bands_info,
            handler,
            receive_stream_id_factory: IdFactory::default(),
        }
    }
}

impl<H: ProcessedDataHandler + Clone> HardwareHandler for Processor<H> {
    type StreamHandler = (
        ReceiveStreamId,
        Rechunker<Complex<f32>>,
        StreamChunkProcessor,
        H,
    );

    fn new_stream_handler(
        &mut self,
        descriptor: ReceiveStreamDescriptor,
    ) -> (
        ReceiveStreamId,
        Rechunker<Complex<f32>>,
        StreamChunkProcessor,
        H,
    ) {
        let stream_id = self.receive_stream_id_factory.create();
        let channels = { &self.bands_info.lock().unwrap().channels };
        let processor = StreamChunkProcessor::new(
            descriptor.frequency,
            descriptor.sample_rate,
            STREAM_TARGET_BIN_SIZE,
            STREAM_TARGET_OUTPUT_PERIOD,
            STREAM_MIN_MAX_TIME_CONSTANT,
            STREAM_OFFSET_REJECT_TIME_CONSTANT,
            descriptor.start_time,
            channels,
        );
        (
            stream_id,
            Rechunker::new(processor.chunk_size),
            processor,
            self.handler.clone(),
        )
    }
}

pub struct StreamChunkProcessor {
    chunk_size: usize,
    overlap_expand: OverlapExpand<Complex<f32>>,
    hann_window: Vec<f32>,
    fft: Fft,
    counter: u32,
    period: f64,
    spectrum: Vec<f32>,
    min: f32,
    max: f32,
    min_max_alpha: f32,
    offset: Complex<f32>,
    offset_reject_alpha: f32,
    channels: Vec<Channel>,
    stream_last_t: Instant,
    channel_last_t: Instant,
    channel_id_factory: IdFactory,
}

struct ChannelGroup {
    ifft: Ifft,
}

pub struct Channel {
    id: ChannelId,
    pub descriptor: ChannelDescriptor,
    pub iq_data: Vec<Complex<f32>>,
    bins: Range<usize>,
    phasors: [f32; 2],
    ifft: Ifft,
    overlap_reduce: OverlapReduce<Complex<f32>>,
}

#[derive(Debug, Clone)]
pub struct ChannelDescriptor {
    pub sample_rate: f64,
    pub name: String,
    pub center_frequency: f64,
    pub bandwidth: f64,
    pub tuning_error: f64,
    pub start_time: Instant,
}

impl StreamChunkProcessor {
    pub fn new(
        frequency: f64,
        sample_rate: f64,
        target_bin_size: f64,
        target_output_period: f64,
        min_max_time_constant: f64,
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
        let offset_reject_alpha =
            (output_period / (offset_reject_time_constant + output_period)) as f32;

        // Build channels from ChannelGroupInfo
        let min_freq = frequency - 0.5 * sample_rate;
        let max_freq = frequency + 0.5 * sample_rate;

        // This refcell is very silly,
        // why don't the lifetimes work?
        let channel_id_factory = RefCell::new(IdFactory::default());
        let channels = channel_group_info
            .iter()
            .filter(|channel_group_info| {
                channel_group_info.max > min_freq && channel_group_info.min < max_freq
            })
            .flat_map(|channel_group_info| {
                let mut channel_group = None;
                let fft = &fft;
                let channel_id_factory_ref = &channel_id_factory;
                channel_group_info.iter().filter_map(move |channel_info| {
                    Self::maybe_create_channel(
                        &mut channel_group,
                        &channel_info,
                        &channel_group_info,
                        frequency,
                        sample_rate,
                        fft,
                        time,
                        channel_id_factory_ref.borrow_mut(),
                    )
                })
            })
            .collect();

        Self {
            chunk_size,
            overlap_expand,
            hann_window: hann_window(fft_size),
            fft,
            counter: 0,
            period: output_period,
            spectrum: vec![0.; fft_size],
            min: std::f32::NAN,
            max: std::f32::NAN,
            min_max_alpha,
            offset: Default::default(),
            offset_reject_alpha,
            channels,
            stream_last_t: time,
            channel_last_t: time,
            channel_id_factory: channel_id_factory.into_inner(),
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
        mut channel_id_factory: impl DerefMut<Target = IdFactory>,
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
        let channel_group = channel_group.get_or_insert_with(|| {
            // Compute channel width, in bins
            let width_bins = fft.size() as f64 * channel_group_info.bandwidth / sample_rate;
            let width_bins = (width_bins / 2.).max(1.).ceil() as usize * 2; // Round up to even size of at least 2
            let ifft = Ifft::new(width_bins);

            ChannelGroup { ifft }
        });

        let center_bin =
            fft.freq2bin((channel_info.center_frequency - stream_center_frequency) / sample_rate);
        // If left bin < 0, skip this channel
        let left_bin = center_bin.checked_sub(channel_group.ifft.dc_bin())?;
        let right_bin = left_bin + channel_group.ifft.size();
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

        let output_sample_rate = sample_rate * channel_group.ifft.size() as f64 / fft.size() as f64;

        let overlap_reduce = OverlapReduce::new(channel_group.ifft.size() / 2);

        Some(Channel {
            id: channel_id_factory.create(),
            descriptor: ChannelDescriptor {
                sample_rate: output_sample_rate,
                name: channel_info.name.clone(),
                center_frequency: channel_info.center_frequency,
                bandwidth: channel_group_info.bandwidth,
                tuning_error,
                start_time: time,
            },
            bins,
            phasors,
            ifft: channel_group.ifft.clone(),
            overlap_reduce,
            iq_data: vec![],
        })
    }

    pub fn period(&self) -> f64 {
        self.period
    }

    pub fn process_chunk(&mut self, samples: &[Complex<f32>], time: Instant) {
        assert_eq!(samples.len(), self.chunk_size);
        // TODO custom error type

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

        // Apply offset correction
        for one_fft in buffer.chunks_exact_mut(self.fft.size()) {
            offset_accumulator += (1. / 3.)
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
        self.channels.par_iter_mut().for_each(|channel| {
            // Aggregate per-channel FFT data in this thread
            let chunk_slice_len = channel.bins.end - channel.bins.start;
            let mut fft_buffer = Vec::with_capacity(chunk_slice_len * fft_count);
            let mut counter = self.counter;
            for one_fft in buffer.chunks_exact(self.fft.size()) {
                // This is a hot loop
                let slice = &one_fft[channel.bins.clone()];
                let start = fft_buffer.len();
                fft_buffer.extend_from_slice(slice);

                // Apply phase correction due to overlap
                let phasor = channel.phasors[counter as usize];
                for sample in fft_buffer[start..start + slice.len()].iter_mut() {
                    *sample *= phasor;
                }
                counter = (counter + 1) % 2;
            }

            // Process this channel
            channel.process(fft_buffer)
        });
        self.channel_last_t = time;

        self.counter = (self.counter + (fft_count % 2) as u32) % 2;
        self.stream_last_t = time;
    }
}

impl<H: ProcessedDataHandler> StreamHandler
    for (
        ReceiveStreamId,
        Rechunker<Complex<f32>>,
        StreamChunkProcessor,
        H,
    )
{
    fn process(&mut self, msg: ReceiveStreamMessage) -> Result<(), String> {
        let (stream_id, rechunker, processor, handler) = self;
        rechunker.process(&msg.iq_data, |chunk| {
            processor.process_chunk(&chunk, msg.time);
            handler.add_waterfall_row(
                *stream_id,
                msg.time,
                &processor.spectrum,
                processor.min,
                processor.max,
            );
        });
        Ok(())
    }
}

impl Channel {
    fn process(&mut self, mut fft_buffer: Vec<Complex<f32>>) {
        self.ifft.process_inplace(&mut fft_buffer);
        let iq_buffer = self.overlap_reduce.process(&fft_buffer);
        self.iq_data = iq_buffer;
    }
}

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    (x * (y / x).powf(a)).max(1e-10)
    //(x * powf_approx(y / x, a)).max(1e-10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_processing() {
        // Parameters from user spec
        let sample_rate = 20_000_000.0; // 20 MSPS
        let center_frequency = 100_000_000.0; // 100 MHz
        let target_bin_size = 2_500.0; // 2.5 KHz
        let target_output_period = 0.01; // 0.01 sec
        let min_max_time_constant = 1.0; // 1s
        let offset_reject_time_constant = 0.1; // 0.1s

        // Channel: 100.7 MHz center freq, 200 KHz bandwidth
        let channel_center_frequency = 88_100_000.0; // 88.1 MHz
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
        }];

        let time = Instant::now();

        let mut processor = StreamChunkProcessor::new(
            center_frequency,
            sample_rate,
            target_bin_size,
            target_output_period,
            min_max_time_constant,
            offset_reject_time_constant,
            time,
            &channel_group_info,
        );

        // Get the chunk size
        let chunk_count = 40;
        println!(
            "Chunk size: {}, chunk count: {}",
            processor.chunk_size, chunk_count
        );

        // Generate signal: e^(j*w*t) where w = channel_center_frequency * 2π
        // and t = sample_index / sample_rate
        let samples: Vec<Complex<f32>> = (0..processor.chunk_size * chunk_count)
            .map(|i| {
                let t = i as f64 / sample_rate;
                let omega = channel_center_frequency * std::f64::consts::TAU;
                Complex::cis(((omega * t) % std::f64::consts::TAU) as f32)
            })
            .collect();

        // Process the data in chunks
        let mut combined_channel_data = vec![];
        for chunk in samples.chunks_exact(processor.chunk_size) {
            processor.process_chunk(chunk, time);
            let mut channels_iq_data = processor.channels.iter().filter_map(|channel| {
                (channel.descriptor.center_frequency == channel_center_frequency)
                    .then(|| &channel.iq_data)
            });
            let iq_data = channels_iq_data.next().expect("Could not find channel!");
            assert!(
                channels_iq_data.next().is_none(),
                "Found multiple channels!"
            );
            combined_channel_data.extend_from_slice(iq_data);
        }

        if combined_channel_data.is_empty() {
            println!("No channel output received");
        } else {
            let channel_output_path = "channel_test_output.raw";
            let channel_bytes: Vec<u8> = combined_channel_data
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
            println!(
                "Total channel IQ data length: {} samples",
                combined_channel_data.len(),
            );
            println!("Channel output saved to {}", channel_output_path);
        }
    }
}
