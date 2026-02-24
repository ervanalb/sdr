use num_complex::Complex;

pub struct Rechunker<T> {
    chunk_size: usize,
    buffer: Vec<T>,
}

impl<T: Clone> Rechunker<T> {
    pub fn new(chunk_size: usize) -> Self {
        Rechunker {
            chunk_size,
            buffer: Vec::with_capacity(chunk_size),
        }
    }

    pub fn process(&mut self, mut data: &[T], mut emit: impl FnMut(&mut [T])) {
        while !data.is_empty() {
            // Fill buffer until full or we run out of samples
            let space_available = (self.chunk_size - self.buffer.len()).min(data.len());

            self.buffer.extend_from_slice(&data[..space_available]);
            data = &data[space_available..];
            if self.buffer.len() == self.chunk_size {
                emit(&mut self.buffer);
                self.buffer.clear();
            }
        }
    }

    pub fn process_iter(&mut self, data: impl Iterator<Item = T>, mut emit: impl FnMut(&mut [T])) {
        for sample in data {
            self.buffer.push(sample);
            if self.buffer.len() == self.chunk_size {
                emit(&mut self.buffer);
                self.buffer.clear();
            }
        }
    }
}

pub struct Decimator<T> {
    factor: usize,
    chunk_size: usize,
    buffer: Vec<T>,
    counter: usize,
}

impl<T: Clone> Decimator<T> {
    pub fn new(factor: usize, chunk_size: usize) -> Self {
        Decimator {
            factor,
            chunk_size,
            buffer: Vec::with_capacity(chunk_size),
            counter: 0,
        }
    }

    pub fn process(&mut self, data: &[T], mut emit: impl FnMut(&mut [T])) {
        let offset = (self.factor - self.counter) % self.factor;
        if offset < data.len() {
            for sample in data[offset..].iter().step_by(self.factor) {
                self.buffer.push(sample.clone());
                if self.buffer.len() == self.chunk_size {
                    emit(&mut self.buffer);
                    self.buffer.clear();
                }
            }
        }
        self.counter = (self.counter + data.len()) % self.factor;
    }
}

pub struct Converter {
    f: f64,
    x: f64,
}

impl Converter {
    pub fn new(f: f64) -> Self {
        Converter { f, x: 0. }
    }

    pub fn process(&mut self, data: &mut [Complex<f32>]) {
        for sample in data.iter_mut() {
            *sample *= Complex::<f32>::cis(self.x as f32 * std::f32::consts::TAU);
            self.x = (self.x + self.f).fract();
        }
    }
}

pub struct FirFilter {
    impulse_response_fft: Vec<Complex<f32>>,
    overlap: usize,
    buffer: Vec<Complex<f32>>,
    fft_plan: std::sync::Arc<dyn rustfft::Fft<f32>>,
    ifft_plan: std::sync::Arc<dyn rustfft::Fft<f32>>,
    fft_buffer: Vec<Complex<f32>>,
}

impl FirFilter {
    pub fn new_from_impulse_response(impulse_response: &[Complex<f32>], fft_size: usize) -> Self {
        let mut impulse_response_fft = vec![Complex::<f32>::default(); fft_size];
        impulse_response_fft[..impulse_response.len()].copy_from_slice(impulse_response);

        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_forward(fft_size);
        let ifft_plan = planner.plan_fft_inverse(fft_size);

        // Take FFT of impulse response
        fft_plan.process(&mut impulse_response_fft);

        let overlap = impulse_response.len() - 1;

        FirFilter {
            impulse_response_fft,
            overlap,
            buffer: Vec::with_capacity(fft_size),
            fft_plan,
            ifft_plan,
            fft_buffer: vec![Complex::<f32>::default(); fft_size],
        }
    }

    pub fn process(
        &mut self,
        mut data: &[Complex<f32>],
        mut emit: impl FnMut(&mut [Complex<f32>]),
    ) {
        let fft_size = self.fft_buffer.len();
        let valid_output_size = fft_size - self.overlap;

        while !data.is_empty() {
            // Accumulate data in buffer
            let space_available = (self.fft_buffer.len() - self.buffer.len()).min(data.len());
            self.buffer.extend_from_slice(&data[..space_available]);
            data = &data[space_available..];

            // Process when buffer is full
            if self.buffer.len() == fft_size {
                // Copy to scratch buffer for FFT
                self.fft_buffer.copy_from_slice(&self.buffer);

                // Take FFT of the data
                self.fft_plan.process(&mut self.fft_buffer);

                // Element-wise multiply by impulse_response_fft
                for (sample, h) in self.fft_buffer.iter_mut().zip(&self.impulse_response_fft) {
                    *sample *= h;
                }

                // Take IFFT
                self.ifft_plan.process(&mut self.fft_buffer);

                // Normalize IFFT output
                let norm = 1.0 / fft_size as f32;
                for sample in self.fft_buffer.iter_mut() {
                    *sample *= norm;
                }

                // Emit the valid range
                emit(&mut self.fft_buffer[self.overlap..]);

                // Discard the valid samples from the beginning of the buffer
                // (shift overlap samples to beginning for the next chunk)
                self.buffer.drain(..valid_output_size);
            }
        }
    }
}

pub fn windowed_sinc(cutoff: f64, len: usize) -> Vec<Complex<f32>> {
    let l = (len - 1) as f64;
    let center: f64 = l / 2.;

    (0..len)
        .map(|i| {
            let t = i as f64 - center;
            let sinc = {
                let theta = t * cutoff * std::f64::consts::TAU;
                if theta.abs() > 1e-10 {
                    theta.sin() / theta
                } else {
                    1.
                }
            };

            let window = {
                let theta = t * std::f64::consts::TAU * (1. / l);
                (1. / l) * (0.5 + 0.5 * theta.cos())
            };

            Complex {
                re: (sinc * window) as f32,
                im: 0.,
            }
        })
        .collect()
}

pub struct Owner<T> {
    buffer: Vec<T>,
}

impl<T: Clone> Owner<T> {
    pub fn new() -> Self {
        Owner { buffer: vec![] }
    }

    pub fn process(&mut self, data: &[T]) -> &mut [T] {
        if self.buffer.len() != data.len() {
            self.buffer = data.to_vec();
        } else {
            self.buffer.clone_from_slice(data);
        }
        &mut self.buffer
    }
}

// From https://math.stackexchange.com/a/1105038
pub fn atan2_approx(y: f32, x: f32) -> f32 {
    let a = x.abs().min(y.abs()) / x.abs().max(y.abs());
    let s = a * a;
    // Horner evaluation of Remez polynomial approximation
    let mut r = ((-0.0464964749 * s + 0.15931422) * s - 0.327622764) * s * a + a;
    if y.abs() > x.abs() {
        r = std::f32::consts::FRAC_PI_2 - r;
    }
    if x < 0. {
        r = std::f32::consts::PI - r;
    }
    if y < 0. {
        r = -r;
    }
    r
}
