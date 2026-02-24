use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufWriter, Write};

use num_complex::Complex;

use crate::dsp::Owner;
use crate::{
    dsp::{Converter, Decimator, FirFilter, Rechunker, windowed_sinc},
    hardware::IntoComplexF32,
};

pub struct Waterfall {
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
}

impl Waterfall {
    pub fn new(
        sample_rate: f64,
        target_bin_size: f64,
        output_period: f64,
        min_max_time_constant: f64,
        offset_reject_time_constant: f64,
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

        Self {
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
            channels: vec![],
        }
    }

    pub fn period(&self) -> f64 {
        self.period
    }

    fn freq_to_bin(&self, freq: f64) -> usize {
        let bin = (((freq / self.sample_rate) + 0.5) * self.fft_size as f64).round();
        bin.clamp(0., self.fft_size as f64) as usize
    }

    pub fn set_channels(
        &mut self,
        center_frequency: f64,
        channel_params: impl Iterator<Item = ChannelParams>,
    ) {
        self.channels = channel_params
            .filter_map(|params| {
                let left_freq =
                    params.probe.center_frequency - center_frequency - 0.5 * params.probe.bandwidth;
                let right_freq =
                    params.probe.center_frequency - center_frequency + 0.5 * params.probe.bandwidth;
                if left_freq < -0.5 * self.sample_rate || right_freq > 0.5 * self.sample_rate {
                    return None;
                }
                let left_bin = self.freq_to_bin(
                    params.probe.center_frequency - center_frequency - 0.5 * params.probe.bandwidth,
                ) as u32;
                let right_bin = self.freq_to_bin(
                    params.probe.center_frequency - center_frequency + 0.5 * params.probe.bandwidth,
                ) as u32;
                let squelch_alpha =
                    (self.period / (params.probe.squelch_time_constant + self.period)) as f32;

                let squelch_threshold_on = 10_f64.powf(
                    (params.probe.squelch_threshold_db + 0.5 * params.probe.squelch_hysteresis_db)
                        / 10.,
                );
                let squelch_threshold_off = 10_f64.powf(
                    (params.probe.squelch_threshold_db - 0.5 * params.probe.squelch_hysteresis_db)
                        / 10.,
                );

                let converter_frequency =
                    -(params.convert.center_frequency - center_frequency) / self.sample_rate;
                let lpf_impulse_response = windowed_sinc(
                    0.5 * params.convert.bandwidth / self.sample_rate,
                    1 + 2 * (5. * self.sample_rate / params.convert.bandwidth).round() as usize,
                );

                let fast_chunk_size =
                    (params.convert.target_chunk_period * self.sample_rate).ceil() as usize;
                let fft_size = (fast_chunk_size + lpf_impulse_response.len()).next_power_of_two();

                let decimation_factor =
                    (self.sample_rate / params.convert.target_sample_rate).round() as usize;
                let slow_chunk_size = fast_chunk_size / decimation_factor;
                println!(
                    "Output sample rate: {}",
                    self.sample_rate / decimation_factor as f64
                );

                // Write LPF impulse response to file
                let file = File::create("iq_samples.raw").expect("Failed to create iq_samples.raw");
                let iq_writer = BufWriter::new(file);

                Some(Channel {
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
                    filter: FirFilter::new_from_impulse_response(&lpf_impulse_response, fft_size),
                    decimator: Decimator::new(decimation_factor, slow_chunk_size),
                    iq_writer,
                })
            })
            .collect();
    }

    pub fn process<T: IntoComplexF32 + Copy + std::fmt::Debug>(
        &mut self,
        samples: &[T],
        mut emit: impl FnMut(Vec<f32>, f64, f64),
    ) {
        let offset = Complex::<f32> {
            re: self.offset.re as f32,
            im: self.offset.im as f32,
        };

        self.rechunker.process_iter(
            samples
                .iter()
                .map(|sample| sample.into_complex_f32() - offset),
            |incoming_buffer| {
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
                        .unwrap() as f64;
                    let new_max = output
                        .iter()
                        .copied()
                        .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                        .unwrap() as f64;

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
                    self.min = self.min.max(1e-10);
                    self.max = self.max.max(1e-10);

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

                    emit(output, self.min, self.max);

                    // Clear accumulator
                    self.accumulator.fill(0.0);
                    self.accumulations_count = 0;
                }

                rayon::scope(|s| {
                    for channel in self.channels.iter_mut() {
                        if !channel.active {
                            continue;
                        }
                        s.spawn(|_| {
                            channel.process(incoming_buffer);
                        });
                    }
                });
            },
        );
    }
}

#[derive(Debug, Clone)]
pub struct ChannelParams {
    pub probe: ChannelProbeParams,
    pub convert: ChannelConvertParams,
}

#[derive(Debug, Clone)]
pub struct ChannelProbeParams {
    pub center_frequency: f64,
    pub bandwidth: f64,
    pub squelch_time_constant: f64,
    pub squelch_threshold_db: f64,
    pub squelch_hysteresis_db: f64,
}

#[derive(Debug, Clone)]
pub struct ChannelConvertParams {
    pub center_frequency: f64,
    pub bandwidth: f64,
    pub target_sample_rate: f64,
    pub target_chunk_period: f64,
}

struct Channel {
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
    iq_writer: BufWriter<File>,
}

impl Channel {
    fn update_activation(&mut self) {
        match self.active {
            false => {
                if self.probe_level > self.probe_threshold_on {
                    println!("TURN ON CHANNEL");
                    self.active = true;
                }
            }
            true => {
                if self.probe_level < self.probe_threshold_off {
                    println!("TURN OFF CHANNEL");
                    self.active = false;
                }
            }
        }
    }

    fn process(&mut self, samples: &[Complex<f32>]) {
        if !self.active {
            return;
        }
        let samples = self.owner.process(samples);
        // Shift frequency of interest to baseband
        self.converter.process(samples);
        // LPF signal to avoid aliasing
        self.filter.process(samples, |samples| {
            // Decimate signal to lower data rate
            self.decimator.process(samples, |samples| {
                // Write samples as raw complex f32 (interleaved real, imag pairs)
                for sample in samples.iter() {
                    self.iq_writer.write_all(&sample.re.to_le_bytes()).ok();
                    self.iq_writer.write_all(&sample.im.to_le_bytes()).ok();
                }
                self.iq_writer.flush().ok();
            });
        });
    }
}

fn log_mix_f64(x: f64, y: f64, a: f64) -> f64 {
    x * (y / x).powf(a)
}

fn log_mix_f32(x: f32, y: f32, a: f32) -> f32 {
    x * (y / x).powf(a)
}
