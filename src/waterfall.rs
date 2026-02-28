use std::cmp::Ordering;
use std::ops::Range;
use std::sync::mpsc::SyncSender;
use std::time::Instant;

use num_complex::Complex;

use crate::band_info::{ChannelGroupInfo, ChannelInfo};
use crate::dsp::{Fft, Ifft, Overlap, Reassemble, Rechunker};
use crate::hardware::IntoComplexF32;
use crate::hardware::{
    ChannelMessage, HardwareDeviceId, ReceiveChannelDescriptor, ReceiveChannelDescriptorPtr,
    ReceiveStreamDescriptor, ReceiveStreamDescriptorPtr, StreamMessage,
}; // TODO Move to this module

pub struct Waterfall {
    receive_stream_descriptor_ptr: ReceiveStreamDescriptorPtr,
    overlap: Overlap,
    fft: Fft,
    power_buffer: Vec<f32>,
    accumulator: Vec<f32>,
    offset_accumulator: Complex<f32>,
    sample_rate: f64,
    accumulations_target: usize,
    accumulations_count: usize,
    period: f64,
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
        output_period: f64,
        min_max_time_constant: f64,
        offset_reject_time_constant: f64,
        time: Instant,
        channel_group_info: &[ChannelGroupInfo],
    ) -> Self {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (sample_rate / target_bin_size).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        let overlap = Overlap::new(fft_size);
        let fft = Fft::new(fft_size);

        // Calculate how many FFT results to accumulate before emitting
        // output_rate is how many waterfall rows per second we want
        // Each FFT takes (fft_size / sample_rate) seconds
        // So we need (sample_rate / (fft_size * output_rate)) FFTs per output
        // We have 50% overlap so we need an additional factor of * 2
        let accumulations_target =
            (2. * sample_rate * output_period / (fft_size as f64)).ceil() as usize;
        let accumulations_target = accumulations_target.max(1); // At least 1

        let period = accumulations_target as f64 * fft_size as f64 / sample_rate;

        let min_max_alpha = (period / (min_max_time_constant + period)) as f32;
        let offset_reject_alpha = (period / (offset_reject_time_constant + period)) as f32;

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
                        period,
                    )
                })
            })
            .collect();

        Self {
            receive_stream_descriptor_ptr,
            overlap,
            fft,
            power_buffer: vec![0.0; fft_size],
            accumulator: vec![0.0; fft_size],
            offset_accumulator: Default::default(),
            sample_rate,
            accumulations_target,
            accumulations_count: 0,
            period,
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
        period: f64,
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
        let phasors = [1., phasor];

        /*
        let squelch_threshold_on = 10_f32.powf(
            (probe_params.squelch_threshold_db as f32
                + 0.5 * probe_params.squelch_hysteresis_db as f32)
                / 10.,
        ) * n_bins;
        let squelch_threshold_off = 10_f32.powf(
            (probe_params.squelch_threshold_db as f32
                - 0.5 * probe_params.squelch_hysteresis_db as f32)
                / 10.,
        ) * n_bins;
        let squelch_alpha = (period / (probe_params.squelch_time_constant + period)) as f32;

        let converter_frequency =
            (-(center_frequency - waterfall_center_frequency) / sample_rate) as f32;
        let lpf_impulse_response = windowed_sinc(
            0.5 * convert_params.bandwidth / sample_rate,
            1 + 2
                * (0.5 * AA_FILTER_LENGTH_FACTOR * sample_rate / convert_params.bandwidth).round()
                    as usize,
        );
        */

        let output_sample_rate = sample_rate * channel_group.ifft.size() as f64 / fft.size() as f64;

        let receive_channel_descriptor = ReceiveChannelDescriptor {
            receive_stream_descriptor_ptr: receive_stream_descriptor_ptr.clone(),
            sample_rate: output_sample_rate,
            name: channel_info.name.clone(),
            center_frequency: channel_info.center_frequency,
            tuning_error,
        };
        let receive_channel_descriptor_ptr = receive_channel_descriptor.into();

        let reassemble = Reassemble::new(channel_group.ifft.size());

        let output_chunk_size = (output_sample_rate * period).round() as usize;

        let rechunker = Rechunker::new(output_chunk_size);

        Some(Channel {
            //receive_channel_descriptor,
            receive_channel_descriptor_ptr,
            bins,
            //squelch_threshold_on,
            //squelch_threshold_off,
            //squelch_alpha,
            //probe_level: 1e-10,
            //active: None,
            phasors,
            ifft: channel_group.ifft.clone(),
            reassemble,
            rechunker,
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
        let mut result = Ok(());

        self.overlap.process_iter(
            samples.iter().map(|sample| sample.into_complex_f32()),
            |chunk, counter| {
                if result.is_err() {
                    return;
                }

                {
                    self.fft.apply(chunk);

                    // Add the DC value to the DC accumulator
                    // and remove the DC component for further computation.
                    // Note that the Hann window smears the DC component across 3 FFT terms
                    self.offset_accumulator += (1. / 3.)
                        * (-chunk[self.fft.dc_bin() - 1] + 2. * chunk[self.fft.dc_bin()]
                            - chunk[self.fft.dc_bin() + 1]);
                    chunk[self.fft.dc_bin()] -= self.offset;
                    chunk[self.fft.dc_bin() - 1] += 0.5 * self.offset;
                    chunk[self.fft.dc_bin() + 1] += 0.5 * self.offset;

                    // Compute squared magnitude and add to accumulator
                    for (&sample, power) in chunk.iter().zip(self.power_buffer.iter_mut()) {
                        *power = sample.re * sample.re + sample.im * sample.im;
                    }
                    for (&power, acc) in self.power_buffer.iter().zip(self.accumulator.iter_mut()) {
                        *acc += power;
                    }

                    self.accumulations_count += 1;
                }

                // Check if we should emit a waterfall row
                if self.accumulations_count >= self.accumulations_target {
                    // Normalize and convert to dB
                    let normalization_factor =
                        1.0 / (self.accumulations_count as f32 * self.fft.size() as f32);
                    let output: Vec<f32> = self
                        .accumulator
                        .iter()
                        .map(|&power| power * normalization_factor)
                        .collect();

                    let new_offset = self.offset_accumulator / self.accumulations_count as f32;

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

                    // Check each channel for squelch
                    /*
                    for channel in self.channels.iter_mut() {
                        let probe_threshold_on = self.min * channel.squelch_threshold_on;
                        let probe_threshold_off = self.min * channel.squelch_threshold_off;

                        // Check power within this channel
                        let new_probe_level = output
                            [channel.probe_left_bin as usize..=channel.probe_right_bin as usize]
                            .iter()
                            .sum::<f32>();
                        channel.probe_level = log_mix_f32(
                            channel.probe_level,
                            new_probe_level,
                            channel.squelch_alpha,
                        );

                        match channel.active {
                            None => {
                                if channel.probe_level > probe_threshold_on {
                                    channel.active =
                                        Some(channel.receive_channel_descriptor.clone().into());
                                }
                            }
                            Some(_) => {
                                if channel.probe_level < probe_threshold_off {
                                    channel.active = None;
                                }
                            }
                        }
                    }
                    drop(channels_squelch);
                    */

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
                    self.offset_accumulator = Complex::ZERO;
                    self.accumulations_count = 0;
                    self.stream_last_t = time;
                }

                if result.is_err() {
                    return;
                }

                {
                    //rayon::scope(|s| {
                    for channel in self.channels.iter_mut() {
                        //if channel.active.is_none() {
                        //    continue;
                        //}
                        //        s.spawn(|_| {
                        let r = channel.process(
                            chunk,
                            self.channel_last_t,
                            time,
                            channel_sender.clone(),
                            counter,
                        );
                        if r.is_err() {
                            result = r;
                        }
                        //        });
                    }
                    //});
                    self.channel_last_t = time;
                }
            },
        );
        result
    }
}

struct ChannelGroup {
    ifft: Ifft,
}

struct Channel {
    //receive_channel_descriptor: ReceiveChannelDescriptor,
    receive_channel_descriptor_ptr: ReceiveChannelDescriptorPtr,
    bins: Range<usize>,
    //squelch_threshold_on: f32,
    //squelch_threshold_off: f32,
    //squelch_alpha: f32,
    //probe_level: f32,
    //active: Option<ReceiveChannelDescriptorPtr>, // TODO move more stuff inside this Option
    phasors: [f32; 2],
    ifft: Ifft,
    reassemble: Reassemble,
    rechunker: Rechunker<Complex<f32>>,
}

impl Channel {
    fn process(
        &mut self,
        fft: &[Complex<f32>],
        start_time: Instant,
        end_time: Instant,
        channel_sender: SyncSender<ChannelMessage>,
        counter: u32,
    ) -> Result<(), String> {
        let mut result = Ok(());
        let chunk = self.ifft.process(&fft[self.bins.clone()]);
        // Apply phase correction due to overlap
        let phasor = self.phasors[counter as usize];
        for sample in chunk.iter_mut() {
            *sample *= phasor;
        }
        self.reassemble.process(chunk, |chunk| {
            self.rechunker.process(chunk, |chunk| {
                if result.is_err() {
                    return;
                }
                result = channel_sender
                    .try_send(ChannelMessage {
                        receive_channel_descriptor_ptr: self.receive_channel_descriptor_ptr.clone(),
                        iq_data: chunk.to_vec(),
                        start_time,
                        end_time,
                    })
                    .map_err(|e| e.to_string());
            });
        });
        result
    }
}

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    (x * (y / x).powf(a)).max(1e-10)
    //(x * powf_approx(y / x, a)).max(1e-10)
}
