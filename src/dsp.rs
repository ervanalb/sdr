use std::sync::Arc;

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

#[derive(Clone)]
pub struct Overlap {
    chunk_size: usize,
    buffer: Vec<Complex<f32>>,
    buffer2: Vec<Complex<f32>>,
    window: Arc<Vec<f32>>,
    counter: u32,
}

impl Overlap {
    pub fn new(chunk_size: usize) -> Self {
        // Calculate Hann window
        let inv_len = 1. / chunk_size as f32;
        let center = chunk_size as f32 / 2.;
        let window = (0..chunk_size)
            .map(|i| {
                let t = i as f32 - center;
                let theta = t * std::f32::consts::TAU * inv_len;
                1. + theta.cos()
            })
            .collect();

        Overlap {
            chunk_size,
            buffer: Vec::with_capacity(chunk_size),
            buffer2: vec![Default::default(); chunk_size],
            window: Arc::new(window),
            counter: 0,
        }
    }

    pub fn process(
        &mut self,
        mut data: &[Complex<f32>],
        mut emit: impl FnMut(&mut [Complex<f32>], u32),
    ) {
        while !data.is_empty() {
            // Fill buffer until full or we run out of samples
            let space_available = (self.chunk_size - self.buffer.len()).min(data.len());

            self.buffer.extend_from_slice(&data[..space_available]);
            data = &data[space_available..];
            self.maybe_emit(&mut emit);
        }
    }

    pub fn process_iter(
        &mut self,
        data: impl Iterator<Item = Complex<f32>>,
        mut emit: impl FnMut(&mut [Complex<f32>], u32),
    ) {
        for sample in data {
            self.buffer.push(sample);
            self.maybe_emit(&mut emit);
        }
    }

    fn maybe_emit<F: FnMut(&mut [Complex<f32>], u32)>(&mut self, emit: &mut F) {
        if self.buffer.len() == self.chunk_size {
            for ((buf, buf2), win) in self
                .buffer
                .iter()
                .zip(self.buffer2.iter_mut())
                .zip(self.window.iter())
            {
                *buf2 = buf * win
            }
            emit(&mut self.buffer2, self.counter);
            self.buffer.drain(0..self.chunk_size / 2);
            self.counter += 1;
            if self.counter >= 2 {
                self.counter = 0;
            }
        }
    }
}

#[derive(Clone)]
pub struct Reassemble {
    chunk_size: usize,
    buffer: Vec<Complex<f32>>,
    counter: usize,
}

impl Reassemble {
    pub fn new(chunk_size: usize) -> Self {
        Reassemble {
            chunk_size,
            buffer: vec![Default::default(); chunk_size + chunk_size / 2],
            counter: 0,
        }
    }

    pub fn process(&mut self, data: &[Complex<f32>], mut emit: impl FnMut(&mut [Complex<f32>])) {
        let buf_section = &mut self.buffer[(self.counter * self.chunk_size / 2)
            ..(self.counter * self.chunk_size / 2 + self.chunk_size)];
        for (inp_sample, buf_sample) in data.iter().zip(buf_section.iter_mut()) {
            *buf_sample += inp_sample;
        }
        self.counter += 1;
        if self.counter >= 2 {
            emit(&mut self.buffer[0..self.chunk_size]);
            // Drain the chunk we just emitted,
            // and ensure the remainder is filled with zeros
            self.buffer.copy_within(self.chunk_size.., 0);
            self.buffer[self.chunk_size / 2..].fill(Default::default());
            self.counter = 0;
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
    f: f32,
    x: f32,
}

impl Converter {
    pub fn new(f: f32) -> Self {
        Converter { f, x: 0. }
    }

    pub fn process(&mut self, data: &mut [Complex<f32>]) {
        for sample in data.iter_mut() {
            //*sample = (0.).into(); // XXX
            *sample *= Complex::<f32>::cis(self.x * std::f32::consts::TAU);
            self.x = (self.x + self.f).fract();
        }
    }
}

#[derive(Clone)]
pub struct FirFilter {
    impulse_response_fft: Arc<Vec<Complex<f32>>>,
    overlap: usize,
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    ifft_plan: Arc<dyn rustfft::Fft<f32>>,
    buffer: Vec<Complex<f32>>,
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
            impulse_response_fft: Arc::new(impulse_response_fft),
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
                for (sample, h) in self
                    .fft_buffer
                    .iter_mut()
                    .zip(self.impulse_response_fft.iter())
                {
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

#[derive(Clone)]
pub struct Fft {
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    inv_len: f64,
    scratch: Vec<Complex<f32>>,
}

impl Fft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_forward(fft_size);
        Fft {
            fft_plan,
            inv_len: 1. / fft_size as f64,
            scratch: vec![Default::default(); fft_size],
        }
    }

    pub fn bin2freq(&self, bin: usize) -> f64 {
        (bin as f64 - (self.scratch.len() / 2) as f64) * self.inv_len
    }

    pub fn freq2bin(&self, freq: f64) -> usize {
        let bin = ((freq * self.scratch.len() as f64) + (self.scratch.len() / 2) as f64).round();
        bin as usize
    }

    pub fn apply(&mut self, data: &mut [Complex<f32>]) {
        self.fft_plan.process_with_scratch(data, &mut self.scratch);
        data.rotate_left(data.len() / 2);
    }

    pub fn dc_bin(&self) -> usize {
        (self.scratch.len() + 1) / 2
    }

    pub fn size(&self) -> usize {
        self.scratch.len()
    }
}

#[derive(Clone)]
pub struct Ifft {
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Ifft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_inverse(fft_size);
        Ifft {
            fft_plan,
            output: vec![Default::default(); fft_size],
            scratch: vec![Default::default(); fft_size],
        }
    }

    pub fn dc_bin(&self) -> usize {
        (self.scratch.len() + 1) / 2
    }

    pub fn process(&mut self, data: &[Complex<f32>]) -> &mut [Complex<f32>] {
        self.output.clone_from_slice(data);
        self.output.rotate_right(data.len() / 2);
        self.fft_plan
            .process_with_scratch(&mut self.output, &mut self.scratch);
        &mut self.output
    }

    pub fn size(&self) -> usize {
        self.scratch.len()
    }
}

/*
#[derive(Clone)]
pub struct HannWindow {
    window: Arc<Vec<f32>>,
}

impl HannWindow {
    pub fn new(len: usize) -> Self {
        let inv_len = 1. / len as f32;
        let center = len as f32 / 2.;
        let window = (0..len)
            .map(|i| {
                let t = i as f32 - center;
                let theta = t * std::f32::consts::TAU * inv_len;
                inv_len * (0.5 + 0.5 * theta.cos())
            })
            .collect();

        HannWindow {
            window: Arc::new(window),
        }
    }

    pub fn apply(&self, data: &mut [Complex<f32>]) {
        for (sample, window_sample) in data.iter_mut().zip(self.window.iter()) {
            *sample *= window_sample;
        }
    }
}
*/

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

// Algorithm from https://www.apulsoft.ch/blog/branchless-sincos/
pub fn cis_approx(x: f32) -> Complex<f32> {
    // Approximate sin(x) and cos(x) between -pi and pi
    // relative err |f(x)/sin(x) - 1|
    // sin x: 1.32e-6 near 0
    // cos x: 2.07e-6 at +-2.99

    const S0: f32 = -0.10132104963779; // x
    const S1: f32 = 0.00662060857089096; // x^3
    const S2: f32 = -0.000173351320734045; // x^5
    const S3: f32 = 2.48668816803878e-06; // x^7
    const S4: f32 = -1.97103310997063e-08; // x^9

    const C0: f32 = -0.405284410277645; // 1
    const C1: f32 = 0.0383849982168558; // x^2
    const C2: f32 = -0.00132798793179218; // x^4
    const C3: f32 = 2.37446117208029e-05; // x^6
    const C4: f32 = -2.23984068352572e-07; // x^8

    let x2 = x * x;

    // evaluate two 4th-order polynomials of (x^2) using estrin's scheme.
    let x4 = x2 * x2;
    let x8 = x4 * x4;
    let poly1 = x8.mul_add(S4, x4.mul_add(S3.mul_add(x2, S2), S1.mul_add(x2, S0)));
    let poly2 = x8.mul_add(C4, x4.mul_add(C3.mul_add(x2, C2), C1.mul_add(x2, C0)));

    let si = (x - std::f32::consts::PI) * (x + std::f32::consts::PI) * x * poly1;
    let co = (x - std::f32::consts::FRAC_PI_2) * (x + std::f32::consts::FRAC_PI_2) * poly2;
    Complex { re: co, im: si }
}

pub fn powf_approx(base: f32, exponent: f32) -> f32 {
    1. + (base - 1.) * exponent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overlap_reassemble() {
        let mut overlap = Overlap::new(4);
        let mut reassemble = Reassemble::new(4);
        let mut output = Vec::new();

        let input: Vec<Complex<f32>> = (1..=20)
            .map(|x| Complex {
                re: x as f32,
                im: 0.0,
            })
            .collect();

        overlap.process(&input, |chunk, _counter| {
            for sample in chunk.iter_mut() {
                *sample *= 0.5;
            }
            reassemble.process(chunk, |chunk| {
                output.push(chunk.iter().map(|c| c.re).collect::<Vec<f32>>());
            });
        });

        let expected = vec![
            None,
            Some(vec![5.0, 6.0, 7.0, 8.0]),
            Some(vec![9.0, 10.0, 11.0, 12.0]),
            Some(vec![13.0, 14.0, 15.0, 16.0]),
        ];

        assert_eq!(
            output.len(),
            expected.len(),
            "Should produce 4 output chunks"
        );

        for (i, (actual_chunk, expected_chunk)) in output.iter().zip(expected.iter()).enumerate() {
            let Some(expected_chunk) = expected_chunk else {
                continue;
            };
            assert_eq!(
                actual_chunk.len(),
                expected_chunk.len(),
                "Chunk {} should have correct length",
                i
            );
            for (j, (actual, expected)) in
                actual_chunk.iter().zip(expected_chunk.iter()).enumerate()
            {
                assert!(
                    (actual - expected).abs() < 1e-5,
                    "Chunk {} sample {} mismatch: expected {}, got {}",
                    i,
                    j,
                    expected,
                    actual
                );
            }
        }
    }
    #[test]
    fn test_fft_ifft() {
        let input: Vec<Complex<f32>> = (1..=20)
            .map(|x| Complex {
                re: x as f32,
                im: 0.0,
            })
            .collect();

        let mut fft = Fft::new(20);
        let mut ifft = Ifft::new(20);

        let mut fft_result = input.clone();
        fft.apply(&mut fft_result);
        let output = ifft.process(&fft_result);
        for sample in output.iter_mut() {
            *sample *= 1. / 20.;
        }

        for (i, (output, input)) in output.iter().zip(input.iter()).enumerate() {
            assert!(
                (output - input).norm_sqr() < 1e-10,
                "Sample {} mismatch: expected {}, got {}",
                i,
                input,
                output
            );
        }
    }
}
