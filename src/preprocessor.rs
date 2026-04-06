use crate::{
    document::ClipDescriptor,
    dsp::{Fft, OverlapExpand, hann_window},
    hardware::RawIqSamples,
};
use num_complex::Complex;
use std::mem;

const TARGET_BIN_SIZE: f64 = 2.5e3; // 2.5 KHz

pub struct PreprocessedClipDescriptor {
    pub clip_name: String,
    pub frequency: f64,
    pub sample_rate: f64,
    pub start_time: f64,
    pub chunk_size: usize,
    pub fft_size: usize,
}

pub struct StreamPreprocessor {
    fft_size: usize,
    buffer: Vec<Complex<f32>>,
    overlap_expand: OverlapExpand<Complex<f32>>,
    hann_window: Box<[f32]>,
    fft: Fft,
}

impl StreamPreprocessor {
    pub fn new(
        descriptor: &ClipDescriptor,
    ) -> (StreamPreprocessor, PreprocessedClipDescriptor) {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / target_bin_size`
        let min_fft_size = (descriptor.sample_rate / TARGET_BIN_SIZE).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        let overlap_expand = OverlapExpand::new(fft_size);
        let fft = Fft::new(fft_size);

        let processor = StreamPreprocessor {
            fft_size,
            buffer: vec![],
            overlap_expand,
            hann_window: hann_window(fft_size),
            fft,
        };
        let preprocessed_descriptor = PreprocessedClipDescriptor {
            clip_name: descriptor.name.clone(),
            frequency: descriptor.frequency,
            sample_rate: descriptor.sample_rate,
            start_time: descriptor.start_time,
            chunk_size: descriptor.chunk_size,
            fft_size,
        };
        (processor, preprocessed_descriptor)
    }

    pub fn process(&mut self, data: &RawIqSamples) -> Box<[Complex<f32>]> {
        // Convert data to Complex<f32>
        match data {
            RawIqSamples::CS8(samples) => {
                self.buffer.extend(samples.iter().map(|&sample| {
                    (1. / 127.)
                        * Complex {
                            re: sample.re as f32,
                            im: sample.im as f32,
                        }
                }));
            }
            RawIqSamples::CF32(samples) => {
                self.buffer.extend(samples);
            }
        }

        // Split off an integer number of FFTs
        let split_pt = self.buffer.len() / self.fft_size * self.fft_size;
        let mut samples = self.buffer.split_off(split_pt);
        mem::swap(&mut self.buffer, &mut samples); // split_off works the opposite way from what we want

        // Process incoming data into overlapping chunks
        let mut samples = self.overlap_expand.process(&samples);

        // Apply Hann window
        for one_fft in samples.chunks_exact_mut(self.fft.size()) {
            for (sample, win) in one_fft.iter_mut().zip(self.hann_window.iter()) {
                *sample *= win;
            }
        }

        // FFT
        self.fft.process_inplace(&mut samples);

        // Return result
        samples
    }
}
