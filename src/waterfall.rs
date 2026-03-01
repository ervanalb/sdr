use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use num_complex::Complex;

use crate::band_info::{ChannelGroupInfo, ChannelInfo};
use crate::dsp::{Fft, Ifft, OverlapExpand, OverlapReduce, Rechunker, hann_window};
use crate::hardware::IntoComplexF32;
use crate::hardware::{
    ChannelMessage, HardwareDeviceId, ReceiveChannelDescriptor, ReceiveChannelDescriptorPtr,
    ReceiveStreamDescriptor, ReceiveStreamDescriptorPtr, StreamMessage,
}; // TODO Move to this module

pub struct Waterfall {
    receive_stream_descriptor_ptr: ReceiveStreamDescriptorPtr,
    rechunker: Rechunker<Complex<f32>>,
    overlap_expand: OverlapExpand<Complex<f32>>,
    hann_window: Vec<f32>,
    fft: Fft,
    counter: u32,
    output_period: f64,
    min: f32,
    max: f32,
    min_max_alpha: f32,
    offset: Complex<f32>,
    offset_reject_alpha: f32,
    channels: Vec<Channel>,
    stream_last_t: Instant,
    channel_last_t: Instant,
}

impl Waterfall {
    pub fn new(
        device_id: HardwareDeviceId,
        stream_index: usize,
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

        let chunk_size =
            (sample_rate * target_output_period / fft_size as f64).round() as usize * fft_size;
        let rechunker = Rechunker::new(chunk_size);

        let output_period = sample_rate / chunk_size as f64;

        let overlap_expand = OverlapExpand::new(fft_size);
        let fft = Fft::new(fft_size);

        let min_max_alpha = (output_period / (min_max_time_constant + output_period)) as f32;
        let offset_reject_alpha =
            (output_period / (offset_reject_time_constant + output_period)) as f32;

        let receive_stream_descriptor_ptr: ReceiveStreamDescriptorPtr = ReceiveStreamDescriptor {
            device_id,
            stream_index,
            frequency,
            sample_rate,
        }
        .into();

        // Build channels from ChannelGroupInfo
        let min_freq = frequency - 0.5 * sample_rate;
        let max_freq = frequency + 0.5 * sample_rate;

        let channels: Vec<_> = channel_group_info
            .iter()
            .filter(|channel_group_info| {
                channel_group_info.max > min_freq && channel_group_info.min < max_freq
            })
            .flat_map(|channel_group_info| {
                let mut channel_group = None;
                let receive_stream_descriptor_ptr = &receive_stream_descriptor_ptr;
                let fft = &fft;
                channel_group_info.iter().filter_map(move |channel_info| {
                    Self::maybe_create_channel(
                        &mut channel_group,
                        receive_stream_descriptor_ptr,
                        &channel_info,
                        &channel_group_info,
                        frequency,
                        sample_rate,
                        fft,
                    )
                })
            })
            .collect();

        Self {
            receive_stream_descriptor_ptr,
            rechunker,
            overlap_expand,
            hann_window: hann_window(fft_size),
            fft,
            counter: 0,
            output_period,
            min: std::f32::NAN,
            max: std::f32::NAN,
            min_max_alpha,
            offset: Default::default(),
            offset_reject_alpha,
            channels,
            stream_last_t: time,
            channel_last_t: time,
        }
    }

    fn maybe_create_channel(
        channel_group: &mut Option<ChannelGroup>,
        receive_stream_descriptor_ptr: &ReceiveStreamDescriptorPtr,
        channel_info: &ChannelInfo,
        channel_group_info: &ChannelGroupInfo,
        waterfall_center_frequency: f64,
        sample_rate: f64,
        fft: &Fft,
    ) -> Option<Channel> {
        let center_frequency = channel_info.center_frequency;
        let left_freq =
            center_frequency - waterfall_center_frequency - 0.5 * channel_group_info.bandwidth;
        let right_freq =
            center_frequency - waterfall_center_frequency + 0.5 * channel_group_info.bandwidth;

        if left_freq < -0.5 * sample_rate || right_freq > 0.5 * sample_rate {
            return None;
        }

        // This channel is probably within range
        let channel_group = channel_group.get_or_insert_with(|| {
            // Compute channel width, in bins
            let width_bins = fft.size() as f64 * channel_group_info.bandwidth / sample_rate;
            let width_bins = 2 * (0.5 * width_bins).ceil() as usize; // Round up to even size
            let ifft = Ifft::new(width_bins);

            ChannelGroup { ifft }
        });

        let center_bin = fft
            .freq2bin((channel_info.center_frequency - waterfall_center_frequency) / sample_rate);
        let left_bin = center_bin.checked_sub(channel_group.ifft.dc_bin())?; // If left bin < 0,
        // skip this channel
        let right_bin = left_bin + channel_group.ifft.size();
        if right_bin > fft.size() {
            // If right bin > fft.size(),
            // skip this channel
            return None;
        }
        let bins = left_bin..right_bin;

        let tuning_error = fft.bin2freq(center_bin) * sample_rate + waterfall_center_frequency
            - channel_info.center_frequency;

        // Phasor to correct the phase shift caused by overlapping chunks.
        // General form: e^(j * f_shift * 2pi * t_overlap)
        let phasor: f32 = (-1_f32).powi((center_bin % 2) as i32);
        // General form: [phasor^0, phasor^1, phasor^2, ...]
        let phasors = [1., phasor];

        let output_sample_rate = sample_rate * channel_group.ifft.size() as f64 / fft.size() as f64;

        let receive_channel_descriptor = ReceiveChannelDescriptor {
            receive_stream_descriptor_ptr: receive_stream_descriptor_ptr.clone(),
            sample_rate: output_sample_rate,
            name: channel_info.name.clone(),
            center_frequency: channel_info.center_frequency,
            bandwidth: channel_group_info.bandwidth,
            tuning_error,
        };
        let receive_channel_descriptor_ptr = receive_channel_descriptor.into();

        let overlap_reduce = OverlapReduce::new(channel_group.ifft.size());

        Some(Channel {
            receive_channel_descriptor_ptr,
            bins,
            phasors,
            ifft: channel_group.ifft.clone(),
            overlap_reduce,
        })
    }

    pub fn period(&self) -> f64 {
        self.output_period
    }

    pub fn process<T: IntoComplexF32 + Copy + std::fmt::Debug>(
        &mut self,
        samples: &[T],
        time: Instant,
        stream_sender: &SyncSender<StreamMessage>,
        channel_sender: &SyncSender<ChannelMessage>,
    ) -> Result<(), String> {
        // TODO custom error type
        // TODO make this function take Complex<f32> input only
        let mut result = Ok(());

        self.rechunker.process_iter(
            samples.iter().map(|sample| sample.into_complex_f32()),
            |samples| {
                // Clear buffers & accumulators
                let mut chunk_count = 0;
                let mut offset_accumulator = Complex::<f32>::ZERO;
                let mut accumulator = vec![0.; self.fft.size()];

                // Process incoming data into overlapping chunks
                let mut buffer = self.overlap_expand.process(&samples);

                let mut chunks = buffer.chunks_exact_mut(self.fft.size());
                while let Some(chunk) = chunks.next() {
                    // Apply Hann window
                    for (sample, win) in chunk.iter_mut().zip(self.hann_window.iter()) {
                        *sample *= win;
                    }
                }
                assert!(chunks.into_remainder().is_empty());

                // FFT each chunk
                self.fft.process_inplace(&mut buffer);

                // Apply per-chunk processing
                {
                    let mut power_buffer = vec![0.; self.fft.size()];
                    let mut chunks = buffer.chunks_exact_mut(self.fft.size());
                    while let Some(chunk) = chunks.next() {
                        offset_accumulator += (1. / 3.)
                            * (-chunk[self.fft.dc_bin() - 1] + 2. * chunk[self.fft.dc_bin()]
                                - chunk[self.fft.dc_bin() + 1]);
                        chunk[self.fft.dc_bin()] -= self.offset;
                        chunk[self.fft.dc_bin() - 1] += 0.5 * self.offset;
                        chunk[self.fft.dc_bin() + 1] += 0.5 * self.offset;

                        // Compute squared magnitude and add to accumulator
                        for (&sample, power) in chunk.iter().zip(power_buffer.iter_mut()) {
                            *power = sample.re * sample.re + sample.im * sample.im;
                        }
                        for (&power, acc) in power_buffer.iter().zip(accumulator.iter_mut()) {
                            *acc += power;
                        }

                        // Count chunks so we can compute averages
                        chunk_count += 1;

                        // Keep track of the overlap for phase adjustment
                        self.counter += 1;
                        if self.counter >= 2 {
                            self.counter = 0;
                        }
                    }
                    assert!(chunks.into_remainder().is_empty());
                }

                let inv_chunk_count = 1.0 / (chunk_count as f32 as f32);
                let output: Vec<f32> = accumulator
                    .iter()
                    .map(|&power| power * inv_chunk_count)
                    .collect();

                let new_offset = offset_accumulator * inv_chunk_count;

                if self.offset_reject_alpha.is_finite() {
                    self.offset =
                        self.offset + self.offset_reject_alpha * (new_offset - self.offset);
                }

                let new_min = output
                    .iter()
                    .copied()
                    .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                    .unwrap()
                    .max(1e-10);
                let new_max = output
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

                result = stream_sender
                    .try_send(StreamMessage {
                        receive_stream_descriptor_ptr: self.receive_stream_descriptor_ptr.clone(),
                        waterfall_row: output,
                        start_time: self.stream_last_t,
                        end_time: time,
                        min: self.min,
                        max: self.max,
                    })
                    .map_err(|e| e.to_string());

                // Drive each channel
                let num_chunks = buffer.len() / self.fft.size();
                let channel_result = Mutex::new(Ok(()));
                rayon::scope(|s| {
                    let channel_result = &channel_result;
                    for channel in self.channels.iter_mut() {
                        // Aggregate per-channel FFT data in this thread
                        let chunk_slice_len = channel.bins.end - channel.bins.start;
                        let mut fft_buffer = Vec::with_capacity(chunk_slice_len * num_chunks);
                        for chunk in buffer.chunks_exact_mut(self.fft.size()) {
                            // This is a hot loop
                            let slice = &chunk[channel.bins.clone()];
                            let start = fft_buffer.len();
                            fft_buffer.extend_from_slice(slice);

                            // Apply phase correction due to overlap
                            let phasor = channel.phasors[self.counter as usize];
                            for sample in fft_buffer[start..start + slice.len()].iter_mut() {
                                *sample *= phasor;
                            }
                        }

                        // Do remainder of channel processing in the rayon thread pool
                        s.spawn(|_| {
                            let r = channel.process(
                                fft_buffer,
                                self.channel_last_t,
                                time,
                                channel_sender.clone(),
                            );
                            if r.is_err() {
                                let mut channel_result = channel_result.lock().unwrap();
                                *channel_result = r;
                            }
                        });
                    }
                });
                self.channel_last_t = time;

                let channel_result = channel_result.into_inner().unwrap();
                if channel_result.is_err() {
                    result = channel_result;
                }

                self.stream_last_t = time;
            },
        );
        result
    }
}

struct ChannelGroup {
    ifft: Ifft,
}

struct Channel {
    receive_channel_descriptor_ptr: ReceiveChannelDescriptorPtr,
    bins: Range<usize>,
    phasors: [f32; 2],
    ifft: Ifft,
    overlap_reduce: OverlapReduce<Complex<f32>>,
}

impl Channel {
    // Called every period, possibly from a different thread
    fn process(
        &mut self,
        mut fft_buffer: Vec<Complex<f32>>,
        start_time: Instant,
        end_time: Instant,
        channel_sender: SyncSender<ChannelMessage>,
    ) -> Result<(), String> {
        self.ifft.process_inplace(&mut fft_buffer);
        let iq_buffer = self.overlap_reduce.process(&fft_buffer);
        channel_sender
            .try_send(ChannelMessage {
                receive_channel_descriptor_ptr: self.receive_channel_descriptor_ptr.clone(),
                iq_data: iq_buffer,
                start_time,
                end_time,
            })
            .map_err(|e| e.to_string())
    }
}

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    (x * (y / x).powf(a)).max(1e-10)
    //(x * powf_approx(y / x, a)).max(1e-10)
}
