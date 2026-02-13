use crate::IntoComplexF32;

pub struct WaterfallMessage {
    pub center_frequency: f64,
    pub width: f64,
    pub waterfall_row: Vec<f32>,
}

pub struct Waterfall {
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    fft_buffer: Vec<num_complex::Complex32>,
    incoming_buffer: Vec<num_complex::Complex32>,
    accumulator: Vec<f32>,
    fft_size: usize,
    accumulations_target: usize,
    accumulations_count: usize,
}

const TARGET_BIN_SIZE: f64 = 20_000.0; // 20 KHz

impl Waterfall {
    pub fn new(sample_rate: f64, output_rate: f64) -> Self {
        // Pick a FFT size that is a power of 2 that is at least `sample_rate / TARGET_BIN_SIZE`
        let min_fft_size = (sample_rate / TARGET_BIN_SIZE).ceil() as usize;
        let fft_size = min_fft_size.next_power_of_two();

        // Create FFT planner and plan
        let mut planner = rustfft::FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);

        // Calculate how many FFT results to accumulate before emitting
        // output_rate is how many waterfall rows per second we want
        // Each FFT takes (fft_size / sample_rate) seconds
        // So we need (sample_rate / (fft_size * output_rate)) FFTs per output
        let accumulations_target = (sample_rate / (fft_size as f64 * output_rate)).ceil() as usize;
        let accumulations_target = accumulations_target.max(1); // At least 1

        Self {
            fft,
            fft_buffer: vec![num_complex::Complex32::new(0.0, 0.0); fft_size],
            incoming_buffer: Vec::with_capacity(fft_size),
            accumulator: vec![0.0; fft_size],
            fft_size,
            accumulations_target,
            accumulations_count: 0,
        }
    }

    pub fn process<T: IntoComplexF32 + Copy>(
        &mut self,
        samples: &[T],
        mut emit: impl FnMut(Vec<f32>),
    ) {
        let mut sample_idx = 0;

        while sample_idx < samples.len() {
            // Fill incoming buffer until full or we run out of samples
            let space_available = self.fft_size - self.incoming_buffer.len();
            let samples_to_copy = (samples.len() - sample_idx).min(space_available);

            for i in 0..samples_to_copy {
                self.incoming_buffer
                    .push(samples[sample_idx + i].into_complex_f32());
            }
            sample_idx += samples_to_copy;

            // If buffer is full, run FFT
            if self.incoming_buffer.len() == self.fft_size {
                self.fft.process(&mut self.incoming_buffer);

                // Compute squared magnitude and add to accumulator
                for (&sample, acc) in self.fft_buffer.iter().zip(self.accumulator.iter_mut()) {
                    let power = sample.re * sample.re + sample.im * sample.im;
                    *acc += power;
                }

                self.accumulations_count += 1;

                // Clear incoming buffer for next FFT
                self.incoming_buffer.clear();

                // Check if we should emit
                if self.accumulations_count >= self.accumulations_target {
                    // Normalize and convert to dB
                    let normalization_factor = 1.0 / (self.accumulations_count as f32);
                    let output: Vec<f32> = self
                        .accumulator
                        .iter()
                        .map(|&power| {
                            let normalized = power * normalization_factor;
                            // TODO(Claude): Break this out into a function power_to_db
                            // Convert to dB: 10 * log10(power)
                            // Add small epsilon to avoid log(0)
                            10.0 * (normalized + 1e-10).log10()
                        })
                        .collect();

                    emit(output);

                    // Clear accumulator
                    self.accumulator.fill(0.0);
                    self.accumulations_count = 0;
                }
            }
        }
    }
}
