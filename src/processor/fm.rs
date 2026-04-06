use std::{
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
};

use num_complex::Complex;
use serde::{Deserialize, Serialize};

use crate::{
    audio::{self, AudioBuffer, AudioOutput, FeedResult},
    chunked_deque::ChunkedDeque,
    document::ClipId,
    dsp::{
        CubicInterpolator, FmDemod, Ifft, OverlapExpand, OverlapReduce, RealFft, RealIfft,
        fft_bin2freq, fft_dc_bin, fft_freq2bin, hann_window,
    },
    id_factory::IdFactory,
    preprocessor::PreprocessedClipDescriptor,
    processor::{Processor, ProcessorHistory},
    ui::{StreamInspectorParameters, StreamInspectorResponse, StreamTransmission, Viewport},
};

type TransmissionId = usize;

pub const CHANNEL_MARGIN: f64 = 0.05; // Add 5% of channel bandwidth as margin on each side
const AUDIO_CUTOFF_FREQ: f64 = 22e3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FmProcessorParameters {
    pub frequency: f64,
    pub bandwidth: f64,
    pub squelch_db: f64,
    pub squelch_hysteresis_db: f64,
}

impl FmProcessorParameters {
    pub fn create_processor(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        let (sender, receiver) = channel();
        let processor = FmProcessor::new(self, sender);
        let history = FmHistory::new(self.frequency, self.bandwidth, receiver);
        (Box::new(processor), Box::new(history))
    }
}

pub struct FmProcessor {
    parameters: FmProcessorParameters,
    clips: BTreeMap<ClipId, ClipProcessor>,
    sender: Sender<FmMessage>,
    transmission_id_factory: IdFactory,
}

impl FmProcessor {
    pub fn new(parameters: &FmProcessorParameters, sender: Sender<FmMessage>) -> FmProcessor {
        FmProcessor {
            clips: BTreeMap::new(),
            parameters: parameters.clone(),
            sender,
            transmission_id_factory: IdFactory::default(),
        }
    }
}

impl Processor for FmProcessor {
    fn reset(&mut self) {
        self.clips.clear();
        self.sender.send(FmMessage::Reset).ok();
    }

    fn start_clip(&mut self, clip_id: usize, clip_descriptor: &PreprocessedClipDescriptor) {
        match self.clips.entry(clip_id) {
            Entry::Vacant(e) => {
                if let Some(processor) = ClipProcessor::new(&self.parameters, clip_descriptor) {
                    e.insert(processor);
                }
            }
            Entry::Occupied(_) => {
                panic!("start_clip() called with a clip that already exists");
            }
        }
    }

    fn process_chunk(&mut self, clip_id: ClipId, preprocessed_data: &[Complex<f32>]) {
        if let Some(processor) = self.clips.get_mut(&clip_id) {
            processor.process_chunk(
                preprocessed_data,
                &mut self.sender,
                &mut self.transmission_id_factory,
            );
        }
    }

    fn end_clip(&mut self, clip_id: usize) {
        if let Some(mut processor) = self.clips.remove(&clip_id) {
            processor.end_transmission(&mut self.sender);
        }
    }
}

pub struct ClipProcessor {
    fft_size: usize,
    margin_bin_count: usize,
    bins: Range<usize>,
    phasors: [f32; 2],
    counter: bool,
    squelch_low: f32,
    squelch_high: f32,
    ifft: Ifft,
    overlap_reduce: OverlapReduce<Complex<f32>>,
    fm_demod: FmDemod,
    demodulated_overlap_expand: OverlapExpand<f32>,
    demodulated_hann_window: Box<[f32]>,
    demodulated_fft: RealFft,
    audio_signal_bins: usize,
    audio_ifft: RealIfft,
    audio_overlap_reduce: OverlapReduce<f32>,
    audio_interpolator: CubicInterpolator<f32>,
    active_transmission: Option<TransmissionId>,
    output_sample_rate: f64,
    clip_start_time: f64,
    clip_chunk_size: usize,
    clip_sample_rate: f64,
    clip_chunk_count: usize,
}

impl ClipProcessor {
    fn new(
        parameters: &FmProcessorParameters,
        clip_descriptor: &PreprocessedClipDescriptor,
    ) -> Option<ClipProcessor> {
        let fft_size = clip_descriptor.fft_size;

        // Compute channel width, in bins
        let signal_bin_count = fft_size as f64 * parameters.bandwidth / clip_descriptor.sample_rate;
        let signal_bin_count = (signal_bin_count / 2.).max(1.).ceil() as usize * 2; // Round up to even size of at least 2
        let margin_bin_count =
            fft_size as f64 * parameters.bandwidth * CHANNEL_MARGIN / clip_descriptor.sample_rate;
        let margin_bin_count = margin_bin_count.max(1.).ceil() as usize;
        let ifft_size = signal_bin_count + 2 * margin_bin_count;

        // Find the bins of interest
        let center_bin = fft_freq2bin(
            fft_size,
            (parameters.frequency - clip_descriptor.frequency) / clip_descriptor.sample_rate,
        );
        // If left bin < 0, skip this channel
        let left_bin = center_bin.checked_sub(fft_dc_bin(ifft_size) - margin_bin_count)?;
        let right_bin = left_bin + signal_bin_count;
        if right_bin > fft_size {
            // If right bin > fft_size, skip this channel
            return None;
        }
        let bins = left_bin..right_bin;

        let tuning_error = fft_bin2freq(fft_size, center_bin) * clip_descriptor.sample_rate
            + clip_descriptor.frequency
            - parameters.frequency;

        // Phasor to correct the phase shift caused by overlapping chunks.
        // General form: e^(j * f_shift * 2pi * t_overlap)
        let phasor: f32 = (-1_f32).powi((center_bin % 2) as i32);
        // General form: [phasor^0, phasor^1, phasor^2, ...]
        let phasors = [1., phasor];

        let output_sample_rate = clip_descriptor.sample_rate * ifft_size as f64 / fft_size as f64;

        let audio_fft_size =
            2 * (0.5 * ifft_size as f64 * audio::SAMPLE_RATE / output_sample_rate).ceil() as usize;
        let audio_signal_bins =
            (0.5 * ifft_size as f64 * AUDIO_CUTOFF_FREQ / output_sample_rate).round() as usize;
        let audio_ifft_sample_rate = audio_fft_size as f64 * output_sample_rate / ifft_size as f64;

        Some(ClipProcessor {
            fft_size,
            margin_bin_count,
            bins,
            phasors,
            counter: false,
            squelch_low: 10_f64
                .powf(0.1 * (parameters.squelch_db - parameters.squelch_hysteresis_db))
                as f32,
            squelch_high: 10_f64.powf(0.1 * parameters.squelch_db) as f32,
            ifft: Ifft::new(ifft_size),
            overlap_reduce: OverlapReduce::new(ifft_size / 2),
            fm_demod: FmDemod::new(tuning_error / output_sample_rate),
            demodulated_overlap_expand: OverlapExpand::new(ifft_size),
            demodulated_hann_window: hann_window(ifft_size),
            demodulated_fft: RealFft::new(ifft_size),
            audio_signal_bins,
            audio_ifft: RealIfft::new(audio_fft_size),
            audio_overlap_reduce: OverlapReduce::new(audio_fft_size / 2),
            audio_interpolator: CubicInterpolator::new(audio_ifft_sample_rate / audio::SAMPLE_RATE),
            active_transmission: None,
            output_sample_rate,
            clip_start_time: clip_descriptor.start_time,
            clip_chunk_size: clip_descriptor.chunk_size,
            clip_sample_rate: clip_descriptor.sample_rate,
            clip_chunk_count: 0,
        })
    }

    fn process_chunk(
        &mut self,
        preprocessed_data: &[Complex<f32>],
        sender: &mut Sender<FmMessage>,
        transmission_id_factory: &mut IdFactory,
    ) {
        // Calculate time based on chunk count
        self.clip_chunk_count += 1;
        // Pick out the relevant bins from the overall FFT data
        let fft_count = preprocessed_data.len() / self.fft_size;
        let chunk_slice_len = self.bins.end - self.bins.start;
        let mut fft_buffer = Vec::with_capacity(chunk_slice_len * fft_count);
        for one_fft in preprocessed_data.chunks_exact(self.fft_size) {
            let start = fft_buffer.len() + self.margin_bin_count;
            let end = fft_buffer.len() + self.ifft.size() - self.margin_bin_count;
            // Resize to accomodate IFFT, filling margin with zeros
            fft_buffer.resize(fft_buffer.len() + self.ifft.size(), Complex::ZERO);
            // Copy in FFT data
            fft_buffer[start..end].clone_from_slice(&one_fft[self.bins.clone()]);

            // Apply phase correction due to overlap
            let phasor = self.phasors[self.counter as usize];
            for sample in fft_buffer[start..end].iter_mut() {
                *sample *= phasor;
            }
            self.counter = !self.counter;
        }

        // Measure mean energy in the band
        let energy = fft_buffer
            .iter()
            .map(|sample| sample.re * sample.re + sample.im * sample.im)
            .sum::<f32>()
            / fft_count as f32;

        // TODO: LPF energy?

        // Perform squelch, and only continue execution if there is an active transmission
        let active_transmission_id = match &mut self.active_transmission {
            Some(transmission) => {
                if energy < self.squelch_low {
                    sender.send(FmMessage::EndTransmission(*transmission)).ok();
                    self.active_transmission = None;
                    return;
                } else {
                    transmission
                }
            }
            None => {
                if energy > self.squelch_high {
                    let transmission_id = transmission_id_factory.create();
                    let transmission = self.active_transmission.insert(transmission_id);
                    let period = self.clip_chunk_size as f64 / self.clip_sample_rate;
                    sender
                        .send(FmMessage::StartTransmission {
                            transmission_id,
                            reference_time: self.clip_start_time
                                + self.clip_chunk_count as f64 * period,
                            period,
                            iq_sample_rate: self.output_sample_rate,
                        })
                        .ok();
                    transmission
                } else {
                    return;
                }
            }
        };

        // Recover IQ samples @ baseband by doing IFFT + overlap
        self.ifft.process_inplace(&mut fft_buffer);
        let iq_data = self.overlap_reduce.process(&fft_buffer);

        // Demodulate FM by finding angle of each IQ sample
        let demodulated = self.fm_demod.process(&iq_data);

        // Overlap
        let mut demodulated = self.demodulated_overlap_expand.process(&demodulated);

        let fft_count = demodulated.len() / self.ifft.size();

        // Apply Hann window
        for one_fft in demodulated.chunks_exact_mut(self.ifft.size()) {
            for (sample, win) in one_fft.iter_mut().zip(self.demodulated_hann_window.iter()) {
                *sample *= win;
            }
        }

        // FFT
        let demodulated_spectrum = self.demodulated_fft.process(demodulated);

        // Apply AA filter by selecting only bins below the cutoff freq
        let demodulated_bin_count = self.demodulated_fft.size() / 2 + 1;
        let audio_bin_count = self.audio_ifft.size() / 2 + 1;
        let mut audio_spectrum =
            vec![Complex::ZERO; audio_bin_count * fft_count].into_boxed_slice();
        for (in_fft, out_fft) in demodulated_spectrum
            .chunks_exact(demodulated_bin_count)
            .zip(audio_spectrum.chunks_exact_mut(audio_bin_count))
        {
            // Copy in FFT data
            out_fft[..self.audio_signal_bins].clone_from_slice(&in_fft[..self.audio_signal_bins]);
        }

        // IFFT (downsamples because IFFT is shorter than FFT)
        let audio_data = self.audio_ifft.process(audio_spectrum);
        // Reconstruct audio signal at new sample rate
        let audio_data = self.audio_overlap_reduce.process(&audio_data);
        let audio_data = self.audio_interpolator.process(&audio_data);

        sender
            .send(FmMessage::PushChunk {
                transmission_id: *active_transmission_id,
                iq_data,
                audio_data,
            })
            .ok();
    }

    fn end_transmission(&mut self, sender: &mut Sender<FmMessage>) {
        if let Some(transmission) = self.active_transmission.take() {
            sender.send(FmMessage::EndTransmission(transmission)).ok();
        }
    }
}

pub enum FmMessage {
    Reset,
    StartTransmission {
        transmission_id: TransmissionId,
        reference_time: f64,
        period: f64,
        iq_sample_rate: f64,
    },
    EndTransmission(TransmissionId),
    PushChunk {
        transmission_id: TransmissionId,
        iq_data: Box<[Complex<f32>]>,
        audio_data: Box<[f32]>,
    },
}

impl std::fmt::Debug for FmMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reset => write!(f, "Reset"),
            Self::StartTransmission {
                transmission_id,
                reference_time,
                period,
                iq_sample_rate,
            } => f
                .debug_struct("StartTransmission")
                .field("transmission_id", transmission_id)
                .field("reference_time", reference_time)
                .field("period", period)
                .field("iq_sample_rate", iq_sample_rate)
                .finish(),
            Self::EndTransmission(arg0) => f.debug_tuple("EndTransmission").field(arg0).finish(),
            Self::PushChunk {
                transmission_id,
                iq_data,
                audio_data,
            } => f
                .debug_struct("PushChunk")
                .field("transmission_id", transmission_id)
                .field("iq_data.len()", &iq_data.len())
                .field("audio_data.len()", &audio_data.len())
                .finish(),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

pub struct FmHistory {
    frequency: f64,
    bandwidth: f64,
    receiver: Receiver<FmMessage>,
    transmissions: BTreeMap<TransmissionId, FmTransmission>,
}

impl FmHistory {
    pub fn new(frequency: f64, bandwidth: f64, receiver: Receiver<FmMessage>) -> Self {
        FmHistory {
            frequency,
            bandwidth,
            receiver,
            transmissions: BTreeMap::new(),
        }
    }
}

impl ProcessorHistory for FmHistory {
    fn update(&mut self) {
        for msg in self.receiver.try_iter() {
            match msg {
                FmMessage::Reset => {
                    self.transmissions.clear();
                }
                FmMessage::StartTransmission {
                    transmission_id,
                    reference_time,
                    period,
                    iq_sample_rate,
                } => match self.transmissions.entry(transmission_id) {
                    Entry::Vacant(e) => {
                        e.insert(FmTransmission::new(reference_time, period, iq_sample_rate));
                    }
                    Entry::Occupied(_) => {
                        panic!("Tried to add a new transmission that already exists");
                    }
                },
                FmMessage::EndTransmission(transmission_id) => {
                    self.transmissions
                        .get_mut(&transmission_id)
                        .expect("Tried to end a transmission that doesn't exist")
                        .active = false;
                }
                FmMessage::PushChunk {
                    transmission_id,
                    iq_data,
                    audio_data,
                } => {
                    let transmission = self
                        .transmissions
                        .get_mut(&transmission_id)
                        .expect("Tried to push to a transmission that doesn't exist");

                    transmission.push(iq_data, audio_data);
                }
            }
        }
    }

    fn expire(&mut self, retain_time: f64) {
        self.transmissions
            .retain(|_, transmission| transmission.prune_old_data(retain_time));
    }

    fn draw(
        &self,
        ui: &mut egui::Ui,
        id: egui::Id,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: f64,
    ) {
        let freq_min = self.frequency - 0.5 * self.bandwidth;
        let freq_max = self.frequency + 0.5 * self.bandwidth;
        for (transmission_id, transmission) in self.transmissions.iter() {
            if transmission.chunks.is_empty() {
                continue;
            }
            let start_time = transmission.time(transmission.chunks.start_index() as f64);
            let end_time = transmission.time(transmission.chunks.end_index() as f64);

            let response = StreamTransmission::new(start_time, end_time, freq_min, freq_max).show(
                ui,
                figure_painter,
                figure_rect,
                viewport,
                dt,
                egui::Id::new((id, transmission_id)),
                |ui, StreamInspectorParameters { time, play, seek }, FmUiState { player }| {
                    ui.label(format!("Inspecting: {:.3}s", time));
                    ui.separator();

                    // Find the chunk closest to the inspected time
                    let chunk_index = (transmission.index(time) as isize).clamp(
                        transmission.chunks.start_index(),
                        transmission.chunks.end_index() - 1,
                    );
                    let chunk = &transmission.chunks[chunk_index];
                    ui.label(format!("Chunk index: {}", chunk_index));
                    ui.label(format!("Audio samples: {}", chunk.audio_data.len()));
                    ui.label(format!("IQ samples: {}", chunk.iq_data.len()));

                    ui.separator();

                    // IQ Scatter Plot
                    let num_iq_samples = chunk.iq_data.len();
                    let stride = if num_iq_samples <= 100 {
                        1
                    } else {
                        num_iq_samples / 100
                    };

                    // Calculate target max value for scaling
                    let target_max_val = chunk
                        .iq_data
                        .iter()
                        .step_by(stride)
                        .map(|c| c.re.abs().max(c.im.abs()))
                        .fold(0.0f32, f32::max)
                        * IQ_PLOT_MARGIN;

                    // Store target in memory and animate toward it
                    let iq_plot_id = egui::Id::new((id, transmission_id, "iq_max"));
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(iq_plot_id, target_max_val));
                    let animated_max_val = ui.ctx().animate_value_with_time(
                        iq_plot_id.with("animated"),
                        target_max_val,
                        1., // animation time in seconds
                    );

                    // Create IQ points for plotting
                    let iq_points: Vec<[f64; 2]> = chunk
                        .iq_data
                        .iter()
                        .step_by(stride)
                        .map(|c| [c.re as f64, c.im as f64])
                        .collect();

                    use egui_plot::{Line, Plot, Points};
                    Plot::new("iq_scatter")
                        .allow_axis_zoom_drag(false)
                        .allow_boxed_zoom(false)
                        .allow_double_click_reset(false)
                        .allow_drag(false)
                        .allow_scroll(false)
                        .allow_zoom(false)
                        .show_axes(false)
                        .width(200.0)
                        .height(200.0)
                        .auto_bounds(false)
                        .default_x_bounds(-animated_max_val as f64, animated_max_val as f64)
                        .default_y_bounds(-animated_max_val as f64, animated_max_val as f64)
                        .show(ui, |plot_ui| {
                            plot_ui.points(Points::new("iq", iq_points).radius(2.0));
                        });

                    ui.separator();

                    // Audio Waveform Plot
                    let plot_width = 600.0;
                    let plot_height = 200.0;
                    let num_audio_samples = chunk.audio_data.len();
                    let audio_stride =
                        (num_audio_samples as f32 / plot_width).ceil().max(1.0) as usize;

                    // Create audio points for plotting
                    let audio_points: Vec<[f64; 2]> = chunk
                        .audio_data
                        .iter()
                        .enumerate()
                        .step_by(audio_stride)
                        .map(|(i, &sample)| [i as f64, sample as f64])
                        .collect();

                    Plot::new("audio_waveform")
                        .allow_axis_zoom_drag(false)
                        .allow_boxed_zoom(false)
                        .allow_double_click_reset(false)
                        .allow_drag(false)
                        .allow_scroll(false)
                        .allow_zoom(false)
                        .show_axes(false)
                        .width(plot_width)
                        .height(plot_height)
                        .auto_bounds(false)
                        .default_x_bounds(0., audio_points.len() as f64)
                        .default_y_bounds(-1., 1.)
                        .show_grid(false)
                        .show(ui, |plot_ui| {
                            plot_ui.line(Line::new("audio", audio_points));
                        });

                    // Audio output
                    let mut time_adj = 0.0;

                    if play {
                        if let Some(player) = player.as_mut()
                            && !seek
                        {
                            let mut p = player.lock().unwrap();

                            // Feed new audio data to the audio player
                            let start = p.next_seq_num;
                            let end = (transmission.index(time + AUDIO_LOOKAHEAD_DURATION)
                                as isize)
                                .min(transmission.chunks.end_index());
                            if end > start {
                                let bufs = transmission.chunks.range(start..end).enumerate().map(
                                    |(i, chunk)| {
                                        let seq_num = start + i as isize;
                                        AudioBuffer {
                                            seq_num,
                                            data: chunk.audio_data.clone(),
                                        }
                                    },
                                );

                                let FeedResult {
                                    last_played_seq_num,
                                    underrun,
                                } = p.audio_output.feed(bufs).unwrap(); // TODO replace unwrap
                                if underrun {
                                    eprintln!("Audio underrun!");
                                }
                                p.next_seq_num = end;

                                // Look up last_played_seq_num and set_time based on it,
                                // to keep the inspector time (playhead)
                                // synchronized to the actual audio rate
                                if let Some(last_played_seq_num) = last_played_seq_num {
                                    let player_time = transmission.time(last_played_seq_num as f64);
                                    let chunk_time = transmission.time(chunk_index as f64);
                                    // Apply strong LPF since we have relatively poor introspection into audio
                                    let new_adj = (player_time - chunk_time) as f32;
                                    let alpha = 0.0001;
                                    p.time_adj += alpha * (new_adj - p.time_adj);
                                    time_adj = p.time_adj as f64;
                                }
                            }
                        } else {
                            let audio_output = AudioOutput::new().unwrap(); // TODO replace unwrap

                            *player = Some(Arc::new(Mutex::new(Player {
                                audio_output,
                                next_seq_num: chunk_index,
                                time_adj: 0.,
                            })));
                        }
                    } else {
                        *player = None;
                    }

                    StreamInspectorResponse { time_adj }
                },
            );

            egui::Popup::context_menu(&response)
                .id(egui::Id::new((id, transmission_id, "context_menu")))
                .show(|ui| {
                    if ui.button("Export audio...").clicked() {
                        ui.close();

                        // Sanitize the channel name for use as a filename
                        let default_name = format!("fm.wav").replace(" ", "_").replace("/", "_");

                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name(&default_name)
                            .add_filter("WAV Audio", &["wav"])
                            .save_file()
                        {
                            if let Err(e) = transmission.export_audio_data(&path) {
                                eprintln!("Failed to export audio data: {}", e);
                            }
                        }
                    }
                    if ui.button("Export IQ data...").clicked() {
                        ui.close();

                        // Sanitize the channel name for use as a filename
                        let default_name =
                            format!("fm_{}sps.raw", transmission.iq_sample_rate.round())
                                .replace(" ", "_")
                                .replace("/", "_");

                        if let Some(path) = rfd::FileDialog::new()
                            .set_file_name(&default_name)
                            .add_filter("Raw (complex f32 samples)", &["raw"])
                            .save_file()
                        {
                            if let Err(e) = transmission.export_iq_data(&path) {
                                eprintln!("Failed to export IQ data: {}", e);
                            }
                        }
                    }
                });
            //response.on_hover_text(descriptor.name.clone());
        }
    }
}

pub struct FmTransmission {
    active: bool,
    reference_time: f64,
    period: f64,
    iq_sample_rate: f64,
    chunks: ChunkedDeque<FmTransmissionChunk>,
}

impl FmTransmission {
    fn new(reference_time: f64, period: f64, iq_sample_rate: f64) -> FmTransmission {
        FmTransmission {
            active: true,
            reference_time,
            period,
            iq_sample_rate,
            chunks: ChunkedDeque::new(),
        }
    }

    fn push(&mut self, iq_data: Box<[Complex<f32>]>, audio_data: Box<[f32]>) {
        self.chunks.push_back(FmTransmissionChunk {
            iq_data,
            audio_data,
        })
    }

    fn prune_old_data(&mut self, retain_time: f64) -> bool {
        let cutoff_index = self.index(retain_time) as isize;
        self.chunks.remove_front(cutoff_index);
        !self.chunks.is_empty()
    }

    fn time(&self, index: f64) -> f64 {
        self.reference_time + index * self.period
    }

    fn index(&self, time: f64) -> f64 {
        (time - self.reference_time) / self.period
    }

    fn export_iq_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;

        let mut file = std::fs::File::create(path)?;

        for chunk in self.chunks.iter() {
            for sample in &chunk.iq_data {
                file.write_all(&sample.re.to_le_bytes())?;
                file.write_all(&sample.im.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }

    fn export_audio_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: audio::SAMPLE_RATE as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(path, spec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        for chunk in self.chunks.iter() {
            for sample in &chunk.audio_data {
                // Convert f32 [-1.0, 1.0] to i16 [-32768, 32767]
                let sample_i16 = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                writer
                    .write_sample(sample_i16)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            }
        }

        writer
            .finalize()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct FmTransmissionChunk {
    pub iq_data: Box<[Complex<f32>]>,
    pub audio_data: Box<[f32]>,
}

///////////////////////////////////////////////////////////////////////////////

const IQ_PLOT_MARGIN: f32 = 1.5;
const AUDIO_LOOKAHEAD_DURATION: f64 = 0.2;

#[derive(Default, Clone)]
struct FmUiState {
    player: Option<Arc<Mutex<Player>>>,
}

struct Player {
    audio_output: AudioOutput,
    next_seq_num: isize,
    time_adj: f32,
}
