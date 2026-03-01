use std::{iter::repeat_n, ops::AddAssign, sync::Arc};

use num_complex::Complex;

#[derive(Clone)]
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

/*

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
    rechunker: Rechunker<Complex<f32>>,
    buffer: Vec<Complex<f32>>,
    counter: usize,
}

impl Reassemble {
    pub fn new(chunk_size: usize) -> Self {
        Reassemble {
            chunk_size,
            rechunker: Rechunker::new(chunk_size),
            buffer: vec![Default::default(); chunk_size + chunk_size / 2],
            counter: 0,
        }
    }

    pub fn process(&mut self, data: &[Complex<f32>], mut emit: impl FnMut(&mut [Complex<f32>])) {
        // TODO: get rid of this explicit rechunker. extra buffer isn't necessary.
        self.rechunker.process(data, |data| {
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
        });
    }
}
*/

/*
#[derive(Clone)]
pub struct ChunkedProcessor<T> {
    remainder: Vec<T>,
    chunk_size: usize,
}

impl<T: Clone> ChunkedProcessor<T> {
    pub fn new(chunk_size: usize) -> ChunkedProcessor<T> {
        ChunkedProcessor {
            remainder: Vec::with_capacity(chunk_size),
            chunk_size,
        }
    }

    fn handle_leftover(&mut self, data: &[T], process: impl FnOnce(&mut Vec<T>)) -> usize {
        let available = self.chunk_size - self.remainder.len();
        if !self.remainder.is_empty() && data.len() >= available {
            self.remainder.extend_from_slice(&data[..available]);
            if self.remainder.len() == self.chunk_size {
                process(&mut self.remainder);
                self.remainder.clear();
            }
            available
        } else {
            0
        }
    }

    /// Call the given processing function with slices of length `chunk_size`
    pub fn process(&mut self, mut data: &[T], mut process: impl FnMut(&[T])) {
        // Handle leftover from previous invocation
        let consumed = self.handle_leftover(data, |r| process(&r));
        data = &data[consumed..];

        // Process the data in chunks
        let mut chunks = data.chunks_exact(self.chunk_size);
        while let Some(chunk) = chunks.next() {
            process(chunk);
        }

        // Add leftovers to remainder
        self.remainder.extend_from_slice(chunks.remainder());
    }

    /// Call the given processing function with mutable slices of length `chunk_size`
    pub fn process_mut(&mut self, mut data: &mut [T], mut process: impl FnMut(&mut [T])) {
        // Handle leftover from previous invocation
        let consumed = self.handle_leftover(data, |r| process(r));
        data = &mut data[consumed..];

        // Process the data in chunks
        let mut chunks = data.chunks_exact_mut(self.chunk_size);
        while let Some(chunk) = chunks.next() {
            process(chunk);
        }

        // Add leftovers to remainder
        self.remainder.extend_from_slice(chunks.into_remainder());
    }

    /// Call the given processing function with slices whose lengths are multiples of `chunk_size`
    pub fn process_bulk(&mut self, mut data: &[T], mut process: impl FnMut(&[T])) {
        // Handle leftover from previous invocation
        let consumed = self.handle_leftover(data, |r| process(&r));
        data = &data[consumed..];

        // Process the data in chunks
        let (bulk, rest) = data.split_at(data.len() / self.chunk_size * self.chunk_size);
        process(bulk);

        // Add leftovers to remainder
        self.remainder.extend_from_slice(rest);
    }

    /// Call the given processing function with mutable slices whose lengths are multiples of `chunk_size`
    pub fn process_bulk_mut(&mut self, mut data: &mut [T], mut process: impl FnMut(&mut [T])) {
        // Handle leftover from previous invocation
        let consumed = self.handle_leftover(data, |r| process(r));
        data = &mut data[consumed..];

        // Process the data in chunks
        let (bulk, rest) = data.split_at_mut(data.len() / self.chunk_size * self.chunk_size);
        process(bulk);

        // Add leftovers to remainder
        self.remainder.extend_from_slice(rest);
    }
}
*/

#[derive(Clone)]
pub struct OverlapExpand<T> {
    chunk_size: usize,
    prev_half_chunk: Vec<T>,
}

impl<T: Clone + Default> OverlapExpand<T> {
    pub fn new(chunk_size: usize) -> Self {
        assert!(chunk_size % 2 == 0, "chunk size must be even");
        let half_chunk_size = chunk_size / 2;
        OverlapExpand {
            chunk_size,
            prev_half_chunk: vec![Default::default(); half_chunk_size],
        }
    }

    pub fn process(&mut self, input: &[T], output: &mut Vec<T>) {
        if input.is_empty() {
            return;
        }
        output.reserve(input.len() * 2);
        let (first_chunk, input) = input.split_at(self.chunk_size);
        output.extend_from_slice(&self.prev_half_chunk); // previous half chunk
        output.extend_from_slice(&first_chunk[..self.chunk_size / 2]); // first half of this chunk
        output.extend_from_slice(first_chunk); // full chunk

        let mut chunks = input.chunks_exact(self.chunk_size);
        while let Some(chunk) = chunks.next() {
            output.extend_from_within(output.len() - self.chunk_size / 2..); // previous half chunk
            output.extend_from_slice(&chunk[..self.chunk_size / 2]); // first half of this chunk
            output.extend_from_slice(chunk); // full chunk
        }
        assert!(chunks.remainder().is_empty());

        // Remember the last half chunk for the next call to process()
        self.prev_half_chunk
            .clone_from_slice(&output[output.len() - self.chunk_size / 2..]);
    }
}

#[derive(Clone)]
pub struct OverlapReduce<T> {
    half_chunk_size: usize,
    buffer: Vec<T>,
    start: bool,
}

impl<T: Clone + Default + AddAssign> OverlapReduce<T> {
    pub fn new(chunk_size: usize) -> Self {
        assert!(chunk_size % 2 == 0, "chunk size must be even");
        let half_chunk_size = chunk_size / 2;
        OverlapReduce {
            half_chunk_size,
            buffer: Vec::with_capacity(half_chunk_size),
            start: true,
        }
    }

    pub fn process(&mut self, mut input: &[T], output: &mut Vec<T>) {
        if input.is_empty() {
            return;
        }

        if self.start {
            // Skip first half chunk since it has no valid overlap
            input = &input[self.half_chunk_size..];
            self.start = false;
        }

        output.reserve(input.len() / 2);
        let mut chunks = input.chunks_exact(self.half_chunk_size);
        while let Some(half_chunk) = chunks.next() {
            if !self.buffer.is_empty() {
                for (buf, inp) in self.buffer.iter_mut().zip(half_chunk.iter()) {
                    *buf += inp.clone();
                }

                output.extend_from_slice(&self.buffer);
                self.buffer.clear();
            } else {
                self.buffer.extend_from_slice(half_chunk);
            }
        }
        assert!(chunks.remainder().is_empty());
        assert!(!self.buffer.is_empty()); // even number of half-chunks
    }
}

/*
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
            // *sample = (0.).into(); // XXX
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
*/

#[derive(Clone)]
pub struct Fft {
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    inv_len_f64: f64, // used for freq2bin & friends
    inv_len: f32,     // used for normalization
    scratch: Vec<Complex<f32>>,
}

impl Fft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_forward(fft_size);
        Fft {
            fft_plan,
            inv_len_f64: 1. / fft_size as f64,
            inv_len: 1. / fft_size as f32,
            scratch: vec![Default::default(); fft_size],
        }
    }

    pub fn bin2freq(&self, bin: usize) -> f64 {
        (bin as f64 - (self.scratch.len() / 2) as f64) * self.inv_len_f64
    }

    pub fn freq2bin(&self, freq: f64) -> usize {
        let bin = ((freq * self.scratch.len() as f64) + (self.scratch.len() / 2) as f64).round();
        bin as usize
    }

    pub fn apply(&mut self, data: &mut [Complex<f32>]) {
        self.fft_plan.process_with_scratch(data, &mut self.scratch);
        let mut chunks = data.chunks_exact_mut(self.size());
        while let Some(chunk) = chunks.next() {
            chunk.rotate_left(self.size() / 2);
        }
        assert!(chunks.into_remainder().is_empty());
        for sample in data.iter_mut() {
            *sample *= self.inv_len;
        }
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
    scratch: Vec<Complex<f32>>,
}

impl Ifft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_inverse(fft_size);
        Ifft {
            fft_plan,
            scratch: vec![Default::default(); fft_size],
        }
    }

    pub fn dc_bin(&self) -> usize {
        (self.scratch.len() + 1) / 2
    }

    pub fn apply(&mut self, data: &mut [Complex<f32>]) {
        let mut chunks = data.chunks_exact_mut(self.size());
        while let Some(chunk) = chunks.next() {
            chunk.rotate_right(self.size() / 2);
        }
        assert!(chunks.into_remainder().is_empty());

        self.fft_plan.process_with_scratch(data, &mut self.scratch);
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
    fn test_overlap() {
        let mut overlap_expand = OverlapExpand::new(4);
        let mut overlap_reduce = OverlapReduce::new(4);
        let mut overlapped = Vec::new();
        let mut output = Vec::new();

        let input: Vec<u32> = (0..20).collect();

        overlap_expand.process(&input, &mut overlapped);

        assert_eq!(
            overlapped[..16],
            vec![0, 0, 0, 1, 0, 1, 2, 3, 2, 3, 4, 5, 4, 5, 6, 7]
        );

        overlap_reduce.process(&overlapped, &mut output);

        assert_eq!(output, (0..18).map(|i| i * 2).collect::<Vec<_>>())
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

        let mut work = input.clone();
        fft.apply(&mut work);
        ifft.apply(&mut work);

        for (i, (output, input)) in work.iter().zip(input.iter()).enumerate() {
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
