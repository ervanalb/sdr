use std::cmp::Ordering;
use std::sync::Mutex;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use num_complex::Complex;

use crate::band_info::{ChannelConvertParams, ChannelInfo, ChannelProbeParams, ChannelsInfo};
use crate::dsp::Owner;
use crate::hardware::{
    ChannelMessage, HardwareDeviceId, ReceiveChannelDescriptor, ReceiveChannelDescriptorPtr,
    ReceiveStreamDescriptor, ReceiveStreamDescriptorPtr, StreamMessage,
}; // TODO Move to this module
use crate::{
    dsp::{Converter, Decimator, FirFilter, Rechunker, windowed_sinc},
    hardware::IntoComplexF32,
};

pub struct Waterfall {
    receive_stream_descriptor_ptr: ReceiveStreamDescriptorPtr,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    rechunker: Rechunker<Complex<f32>>,
    buffer: Vec<Complex<f32>>,
    power_buffer: Vec<f32>,
    accumulator: Vec<f32>,
    offset_accumulator: Complex<f32>,
    fft_size: usize,
    sample_rate: f64,
    accumulations_target: usize,
    accumulations_count: usize,
    period: f64,
    min: f64,
    max: f64,
    min_max_alpha: f64,
    offset: Complex<f64>,
    offset_reject_alpha: f64,
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
        output_period: f64,
        min_max_time_constant: f64,
        offset_reject_time_constant: f64,
        time: Instant,
        channels_info: &[ChannelsInfo],
    ) -> Self {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (sample_rate / target_bin_size).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        // Create FFT planner and plan
        let mut planner = rustfft::FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);

        // Calculate how many FFT results to accumulate before emitting
        // output_rate is how many waterfall rows per second we want
        // Each FFT takes (fft_size / sample_rate) seconds
        // So we need (sample_rate / (fft_size * output_rate)) FFTs per output
        let accumulations_target =
            (sample_rate * output_period / (fft_size as f64)).ceil() as usize;
        let accumulations_target = accumulations_target.max(1); // At least 1

        let period = accumulations_target as f64 * fft_size as f64 / sample_rate;

        let min_max_alpha = period / (min_max_time_constant + period);
        let offset_reject_alpha = period / (offset_reject_time_constant + period);

        let receive_stream_descriptor_ptr: ReceiveStreamDescriptorPtr = ReceiveStreamDescriptor {
            device_id,
            stream_index,
            frequency,
            sample_rate,
        }
        .into();

        // Build channels from ChannelsInfo
        let min_freq = frequency - 0.5 * sample_rate;
        let max_freq = frequency + 0.5 * sample_rate;

        let channels: Vec<_> = channels_info
            .iter()
            .filter(|channels_info| channels_info.max > min_freq && channels_info.min < max_freq)
            .flat_map(|channels_info| {
                channels_info.iter().filter_map(|channel_info| {
                    // Check if channel is fully contained within receive range
                    let left_freq =
                        channel_info.center_frequency - 0.5 * channels_info.probe.bandwidth;
                    let right_freq =
                        channel_info.center_frequency + 0.5 * channels_info.probe.bandwidth;

                    if left_freq >= min_freq && right_freq <= max_freq {
                        Self::create_channel(
                            &receive_stream_descriptor_ptr,
                            &channel_info,
                            frequency,
                            sample_rate,
                            fft_size,
                            period,
                            &channels_info.probe,
                            &channels_info.convert,
                        )
                    } else {
                        None
                    }
                })
            })
            .collect();

        Self {
            receive_stream_descriptor_ptr,
            fft,
            rechunker: Rechunker::new(fft_size),
            buffer: vec![Default::default(); fft_size],
            power_buffer: vec![0.0; fft_size],
            accumulator: vec![0.0; fft_size],
            offset_accumulator: Default::default(),
            fft_size,
            sample_rate,
            accumulations_target,
            accumulations_count: 0,
            period,
            min: std::f64::NAN,
            max: std::f64::NAN,
            min_max_alpha,
            offset: Default::default(),
            offset_reject_alpha,
            channels,
            stream_last_t: time,
            channel_last_t: time,
        }
    }

    fn create_channel(
        receive_stream_descriptor_ptr: &ReceiveStreamDescriptorPtr,
        channel_info: &ChannelInfo,
        waterfall_center_frequency: f64,
        sample_rate: f64,
        fft_size: usize,
        period: f64,
        probe_params: &ChannelProbeParams,
        convert_params: &ChannelConvertParams,
    ) -> Option<Channel> {
        let center_frequency = channel_info.center_frequency;
        let left_freq =
            center_frequency - waterfall_center_frequency - 0.5 * probe_params.bandwidth;
        let right_freq =
            center_frequency - waterfall_center_frequency + 0.5 * probe_params.bandwidth;

        if left_freq < -0.5 * sample_rate || right_freq > 0.5 * sample_rate {
            return None;
        }

        let freq_to_bin = |freq: f64| -> u32 {
            let bin = (((freq / sample_rate) + 0.5) * fft_size as f64).round();
            bin.clamp(0., fft_size as f64) as u32
        };

        let left_bin = freq_to_bin(
            center_frequency - waterfall_center_frequency - 0.5 * probe_params.bandwidth,
        );
        let right_bin = freq_to_bin(
            center_frequency - waterfall_center_frequency + 0.5 * probe_params.bandwidth,
        );

        let squelch_alpha = (period / (probe_params.squelch_time_constant + period)) as f32;

        let squelch_threshold_on = 10_f64.powf(
            (probe_params.squelch_threshold_db + 0.5 * probe_params.squelch_hysteresis_db) / 10.,
        );
        let squelch_threshold_off = 10_f64.powf(
            (probe_params.squelch_threshold_db - 0.5 * probe_params.squelch_hysteresis_db) / 10.,
        );

        let converter_frequency = -(center_frequency - waterfall_center_frequency) / sample_rate;
        let lpf_impulse_response = windowed_sinc(
            0.5 * convert_params.bandwidth / sample_rate,
            1 + 2 * (5. * sample_rate / convert_params.bandwidth).round() as usize,
        );

        let fast_chunk_size = (convert_params.target_chunk_period * sample_rate).ceil() as usize;
        let fft_size_for_filter =
            (fast_chunk_size + lpf_impulse_response.len()).next_power_of_two();

        let decimation_factor = (sample_rate / convert_params.target_sample_rate).round() as usize;
        let slow_chunk_size = fast_chunk_size / decimation_factor;

        let receive_channel_descriptor_ptr = ReceiveChannelDescriptor {
            receive_stream_descriptor_ptr: receive_stream_descriptor_ptr.clone(),
            sample_rate: sample_rate / decimation_factor as f64,
            name: channel_info.name.clone(),
            center_frequency: channel_info.center_frequency,
        }
        .into();

        Some(Channel {
            receive_channel_descriptor_ptr,
            probe_left_bin: left_bin,
            probe_right_bin: right_bin,
            squelch_alpha,
            squelch_threshold_on,
            squelch_threshold_off,
            probe_threshold_on: std::f32::INFINITY,
            probe_threshold_off: std::f32::INFINITY,
            probe_level: 1e-10,
            active: false,
            owner: Owner::new(),
            converter: Converter::new(converter_frequency),
            filter: FirFilter::new_from_impulse_response(
                &lpf_impulse_response,
                fft_size_for_filter,
            ),
            decimator: Decimator::new(decimation_factor, slow_chunk_size),
        })
    }

    pub fn period(&self) -> f64 {
        self.period
    }

    pub fn process<T: IntoComplexF32 + Copy + std::fmt::Debug>(
        &mut self,
        samples: &[T],
        time: Instant,
        stream_sender: &SyncSender<StreamMessage>,
        channel_sender: &SyncSender<ChannelMessage>,
    ) -> Result<(), String> {
        // TODO custom error type
        let offset = Complex::<f32> {
            re: self.offset.re as f32,
            im: self.offset.im as f32,
        };

        let mut result = Ok(());

        self.rechunker.process_iter(
            samples
                .iter()
                .map(|sample| sample.into_complex_f32() - offset),
            |incoming_buffer| {
                if result.is_err() {
                    return;
                }

                self.buffer.clone_from_slice(incoming_buffer);
                self.fft.process(&mut self.buffer);

                // Compute squared magnitude and add to accumulator
                let shifted_buffer = self.buffer[self.fft_size / 2..]
                    .iter()
                    .chain(self.buffer[..self.fft_size / 2].iter());
                for (&sample, power) in shifted_buffer.zip(self.power_buffer.iter_mut()) {
                    *power = sample.re * sample.re + sample.im * sample.im;
                }
                for (&power, acc) in self.power_buffer.iter().zip(self.accumulator.iter_mut()) {
                    *acc += power;
                }

                // Add the DC value to the DC accumulator
                self.offset_accumulator += self.buffer[0];

                self.accumulations_count += 1;

                if self.min <= self.max {
                    // Search for active channels
                    for channel in self.channels.iter_mut() {
                        // Check power within this channel
                        let new_level: f32 = self.power_buffer
                            [channel.probe_left_bin as usize..=channel.probe_right_bin as usize]
                            .iter()
                            .sum();
                        channel.probe_level =
                            log_mix_f32(channel.probe_level, new_level, channel.squelch_alpha);
                        channel.update_activation();
                    }
                }

                // Check if we should emit a waterfall row
                if self.accumulations_count >= self.accumulations_target {
                    // Normalize and convert to dB
                    let normalization_factor =
                        1.0 / (self.accumulations_count as f32 * self.fft_size as f32);
                    let output: Vec<f32> = self
                        .accumulator
                        .iter()
                        .map(|&power| power * normalization_factor)
                        .collect();

                    let new_offset = self.offset_accumulator * normalization_factor;
                    let new_offset = Complex::<f64> {
                        re: new_offset.re as f64,
                        im: new_offset.im as f64,
                    };

                    let new_min = output
                        .iter()
                        .copied()
                        .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                        .unwrap()
                        .max(1e-10) as f64;
                    let new_max = output
                        .iter()
                        .copied()
                        .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                        .unwrap()
                        .max(1e-10) as f64;

                    // Compute min/max with LPF
                    if self.min <= self.max {
                        // Normal case:

                        // LPF in log space
                        self.min = log_mix_f64(self.min, new_min, self.min_max_alpha);
                        self.max = log_mix_f64(self.max, new_max, self.min_max_alpha);
                    } else {
                        // On startup, or if something goes wrong:
                        self.min = new_min;
                        self.max = new_max;
                    }

                    // Update relative squelch thresholds for each channel
                    for channel in self.channels.iter_mut() {
                        let normalize = (channel.probe_right_bin - channel.probe_left_bin + 1)
                            as f64
                            * self.fft_size as f64;
                        channel.probe_threshold_on =
                            (self.min * channel.squelch_threshold_on * normalize) as f32;
                        channel.probe_threshold_off =
                            (self.min * channel.squelch_threshold_off * normalize) as f32;
                    }

                    if self.offset_reject_alpha.is_finite() {
                        self.offset =
                            self.offset + self.offset_reject_alpha * (new_offset - self.offset);
                    }

                    result = stream_sender
                        .try_send(StreamMessage {
                            receive_stream_descriptor_ptr: self
                                .receive_stream_descriptor_ptr
                                .clone(),
                            waterfall_row: output,
                            start_time: self.stream_last_t,
                            end_time: time,
                            min: self.min,
                            max: self.max,
                        })
                        .map_err(|e| e.to_string());

                    // Clear accumulator
                    self.accumulator.fill(0.0);
                    self.accumulations_count = 0;
                    self.stream_last_t = time;
                }

                if result.is_err() {
                    return;
                }

                let channel_result = Mutex::new(Ok(()));
                rayon::scope(|s| {
                    for channel in self.channels.iter_mut() {
                        if !channel.active {
                            continue;
                        }
                        s.spawn(|_| {
                            let r = channel.process(
                                incoming_buffer,
                                self.channel_last_t,
                                time,
                                channel_sender.clone(),
                            );
                            if r.is_err() {
                                *(channel_result.lock().unwrap()) = r;
                            }
                        });
                    }
                });
                result = channel_result.into_inner().unwrap();
                self.channel_last_t = time;
            },
        );
        result
    }
}

struct Channel {
    receive_channel_descriptor_ptr: ReceiveChannelDescriptorPtr,
    probe_left_bin: u32,
    probe_right_bin: u32,
    squelch_alpha: f32,
    squelch_threshold_on: f64,
    squelch_threshold_off: f64,
    probe_threshold_on: f32,
    probe_threshold_off: f32,
    probe_level: f32,
    active: bool,
    owner: Owner<Complex<f32>>,
    converter: Converter,
    filter: FirFilter,
    decimator: Decimator<Complex<f32>>,
}

impl Channel {
    fn update_activation(&mut self) {
        match self.active {
            false => {
                if self.probe_level > self.probe_threshold_on {
                    self.active = true;
                }
            }
            true => {
                if self.probe_level < self.probe_threshold_off {
                    self.active = false;
                }
            }
        }
    }

    fn process(
        &mut self,
        samples: &[Complex<f32>],
        start_time: Instant,
        end_time: Instant,
        channel_sender: SyncSender<ChannelMessage>,
    ) -> Result<(), String> {
        let mut result = Ok(());
        if !self.active {
            return result;
        }
        let samples = self.owner.process(samples);
        // Shift frequency of interest to baseband
        self.converter.process(samples);
        // LPF signal to avoid aliasing
        self.filter.process(samples, |samples| {
            // Decimate signal to lower data rate
            self.decimator.process(samples, |samples| {
                if result.is_err() {
                    return;
                }
                result = channel_sender
                    .try_send(ChannelMessage {
                        receive_channel_descriptor_ptr: self.receive_channel_descriptor_ptr.clone(),
                        iq_data: samples.to_vec(),
                        start_time,
                        end_time,
                    })
                    .map_err(|e| e.to_string());
            });
        });
        result
    }
}

fn log_mix_f64(x: f64, y: f64, a: f64) -> f64 {
    (x * (y / x).powf(a)).max(1e-10)
}

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    (x * (y / x).powf(a)).max(1e-10)
}
