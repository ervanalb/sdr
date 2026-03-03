use std::{mem, ops::AddAssign, sync::Arc};

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

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn process(&mut self, mut data: &[T], mut emit: impl FnMut(Vec<T>)) {
        while !data.is_empty() {
            // Fill buffer until full or we run out of samples
            let space_available = (self.chunk_size - self.buffer.len()).min(data.len());

            self.buffer.extend_from_slice(&data[..space_available]);
            data = &data[space_available..];
            if self.buffer.len() == self.chunk_size {
                let output = mem::replace(&mut self.buffer, Vec::with_capacity(self.chunk_size));
                emit(output);
            }
        }
    }

    pub fn process_iter(&mut self, data: impl Iterator<Item = T>, mut emit: impl FnMut(Vec<T>)) {
        for sample in data {
            self.buffer.push(sample);
            if self.buffer.len() == self.chunk_size {
                let output = mem::replace(&mut self.buffer, Vec::with_capacity(self.chunk_size));
                emit(output);
            }
        }
    }
}

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

    pub fn process(&mut self, input: &[T]) -> Vec<T> {
        if input.is_empty() {
            return vec![];
        }
        let mut output = Vec::with_capacity(input.len() * 2);
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
        output
    }
}

#[derive(Clone)]
pub struct OverlapReduce<T> {
    half_chunk_size: usize,
    buffer: Vec<T>,
    start: bool,
}

impl<T: Clone + Default + AddAssign> OverlapReduce<T> {
    pub fn new(half_chunk_size: usize) -> Self {
        OverlapReduce {
            half_chunk_size,
            buffer: Vec::with_capacity(half_chunk_size),
            start: true,
        }
    }

    pub fn process(&mut self, mut input: &[T]) -> Vec<T> {
        if input.is_empty() {
            return vec![];
        }

        if self.start {
            // Skip first half chunk since it has no valid overlap
            input = &input[self.half_chunk_size..];
            self.start = false;
        }

        let mut output = Vec::with_capacity(input.len() / 2);
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

        output
    }
}

#[derive(Clone)]
pub struct Fft {
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    fft_size: usize,
    fft_scratch: Vec<Complex<f32>>,
    inv_len_f64: f64, // used for freq2bin & friends
    inv_len: f32,     // used for normalization
}

impl Fft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_forward(fft_size);
        let fft_scratch = vec![Default::default(); fft_plan.get_inplace_scratch_len()];
        Fft {
            fft_plan,
            fft_size,
            fft_scratch,
            inv_len_f64: 1. / fft_size as f64,
            inv_len: 1. / fft_size as f32,
        }
    }

    pub fn bin2freq(&self, bin: usize) -> f64 {
        (bin as f64 - (self.fft_size / 2) as f64) * self.inv_len_f64
    }

    pub fn freq2bin(&self, freq: f64) -> usize {
        let bin = ((freq * self.fft_size as f64) + (self.fft_size / 2) as f64).round();
        bin as usize
    }

    pub fn process_inplace(&mut self, data: &mut [Complex<f32>]) {
        self.fft_plan.process_with_scratch(data, &mut self.fft_scratch);
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
        (self.fft_size + 1) / 2
    }

    pub fn size(&self) -> usize {
        self.fft_size
    }
}

#[derive(Clone)]
pub struct Ifft {
    fft_plan: Arc<dyn rustfft::Fft<f32>>,
    fft_size: usize,
    fft_scratch: Vec<Complex<f32>>,
}

impl Ifft {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fft_plan = planner.plan_fft_inverse(fft_size);
        let fft_scratch = vec![Default::default(); fft_plan.get_inplace_scratch_len()];
        Ifft { fft_plan, fft_size, fft_scratch }
    }

    pub fn dc_bin(&self) -> usize {
        (self.fft_size + 1) / 2
    }

    pub fn process_inplace(&mut self, data: &mut [Complex<f32>]) {
        let mut chunks = data.chunks_exact_mut(self.size());
        while let Some(chunk) = chunks.next() {
            chunk.rotate_right(self.size() / 2);
        }
        assert!(chunks.into_remainder().is_empty());

        self.fft_plan.process_with_scratch(data, &mut self.fft_scratch);
    }

    pub fn size(&self) -> usize {
        self.fft_size
    }
}

pub fn hann_window(len: usize) -> Vec<f32> {
    let inv_len = 1. / len as f32;
    let center = len as f32 / 2.;
    (0..len)
        .map(|i| {
            let t = i as f32 - center;
            let theta = t * std::f32::consts::TAU * inv_len;
            0.5 + 0.5 * theta.cos()
        })
        .collect()
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
        let mut overlap_reduce = OverlapReduce::new(2);

        let input: Vec<u32> = (0..24).collect();

        let overlapped = overlap_expand.process(&input);

        assert_eq!(overlapped.len(), 48);
        assert_eq!(
            overlapped[..16],
            vec![0, 0, 0, 1, 0, 1, 2, 3, 2, 3, 4, 5, 4, 5, 6, 7]
        );

        let output = overlap_reduce.process(&overlapped);
        assert_eq!(output.len(), 22);

        assert_eq!(output, (0..22).map(|i| i * 2).collect::<Vec<_>>())
    }

    #[test]
    fn test_overlap_multiple_calls() {
        let input: Vec<u32> = (0..24).collect();

        let mut overlapped_from_chunks = vec![];
        let mut output_from_chunks = vec![];

        {
            // Process in chunks
            let mut overlap_expand = OverlapExpand::new(4);
            let mut overlap_reduce = OverlapReduce::new(2);

            for input_chunk in input.chunks_exact(4) {
                let overlapped_chunk = overlap_expand.process(&input_chunk);
                overlapped_from_chunks.extend_from_slice(&overlapped_chunk);
                let output_chunk = overlap_reduce.process(&overlapped_chunk);
                output_from_chunks.extend(output_chunk);
            }
        }

        let overlapped_bulk;
        let output_bulk;
        {
            // Process in bulk
            let mut overlap_expand = OverlapExpand::new(4);
            let mut overlap_reduce = OverlapReduce::new(2);

            overlapped_bulk = overlap_expand.process(&input);
            output_bulk = overlap_reduce.process(&overlapped_from_chunks);
        }

        assert_eq!(overlapped_from_chunks, overlapped_bulk,);
        assert_eq!(output_from_chunks, output_bulk,);
    }

    #[test]
    fn test_fft_ifft() {
        let input: Vec<Complex<f32>> = (1..=20)
            .map(|x| Complex {
                re: x as f32,
                im: 0.0,
            })
            .collect();

        let mut fft = Fft::new(4);
        let mut ifft = Ifft::new(4);

        let mut work = input.clone();
        fft.process_inplace(&mut work);
        ifft.process_inplace(&mut work);

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
