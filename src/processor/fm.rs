use std::{
    collections::{BTreeMap, VecDeque, btree_map::Entry},
    ops::Range,
    sync::mpsc::{Receiver, Sender, channel},
};

use egui::{Align, Layout};
use num_complex::Complex;
use serde::{Deserialize, Serialize};

use crate::{
    asr_provider::{self, AsrProvider, AsrStream},
    audio::{self, AudioBuffer, AudioOutput, FeedResult},
    chunked_deque::ChunkedDeque,
    document::ClipId,
    dsp::{
        CubicInterpolator, FmDemod, Ifft, OverlapExpand, OverlapReduce, RealFft, RealIfft,
        Rechunker, fft_bin2freq, fft_dc_bin, fft_freq2bin, hann_window,
    },
    id_factory::IdFactory,
    preprocessor::PreprocessedClipDescriptor,
    processor::{Processor, ProcessorHistory, ProcessorUi},
    ui::{
        PlayState, StreamTransmissionResponse, TransmissionInspectorState, Viewport,
        stream_transmission_ui,
    },
};
use std::any::Any;

type TransmissionId = usize;

pub const CHANNEL_MARGIN: f64 = 0.05; // Add 5% of channel bandwidth as margin on each side
const AUDIO_CUTOFF_FREQ: f64 = 22e3;
const ASR_CUTOFF_FREQ: f64 = asr_provider::SAMPLE_RATE * 0.45; // Cutoff slightly below Nyquist

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FmProcessorParameters {
    pub frequency: f64,
    pub bandwidth: f64,
    pub squelch_db: f64,
    pub squelch_hysteresis_db: f64,
}

impl Default for FmProcessorParameters {
    fn default() -> Self {
        Self {
            frequency: 100.0e6,
            bandwidth: 200e3,
            squelch_db: -100.0,
            squelch_hysteresis_db: 3.0,
        }
    }
}

impl FmProcessorParameters {
    pub fn create_processor(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        asr_provider: Option<&AsrProvider>,
    ) -> (Box<dyn Processor>, Box<dyn ProcessorHistory>) {
        let (sender, receiver) = channel();
        let processor = FmProcessor::new(self, sender, asr_provider.cloned());
        let transcription_latency = asr_provider.map(|p| p.latency()).unwrap_or(0.0);
        let history = FmHistory::new(
            self.frequency,
            self.bandwidth,
            receiver,
            transcription_latency,
        );
        (Box::new(processor), Box::new(history))
    }

    pub fn draw_setup(&mut self, ui: &mut egui::Ui) {
        ui.label("Frequency (Hz):");
        ui.add(egui::DragValue::new(&mut self.frequency).suffix(" Hz"));

        ui.label("Bandwidth (Hz):");
        ui.add(egui::DragValue::new(&mut self.bandwidth).suffix(" Hz"));

        ui.label("Squelch (dB):");
        ui.add(egui::DragValue::new(&mut self.squelch_db).suffix(" dB"));

        ui.label("Squelch Hysteresis (dB):");
        ui.add(egui::DragValue::new(&mut self.squelch_hysteresis_db).suffix(" dB"));
    }
}

pub struct FmProcessor {
    parameters: FmProcessorParameters,
    clips: BTreeMap<ClipId, ClipProcessor>,
    sender: Sender<FmMessage>,
    transmission_id_factory: IdFactory,
    asr_provider: Option<AsrProvider>,
}

impl FmProcessor {
    pub fn new(
        parameters: &FmProcessorParameters,
        sender: Sender<FmMessage>,
        asr_provider: Option<AsrProvider>,
    ) -> FmProcessor {
        FmProcessor {
            clips: BTreeMap::new(),
            parameters: parameters.clone(),
            sender,
            transmission_id_factory: IdFactory::default(),
            asr_provider,
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
                if let Some(processor) = ClipProcessor::new(
                    clip_id,
                    &self.parameters,
                    clip_descriptor,
                    self.asr_provider.as_ref(),
                ) {
                    e.insert(processor);
                }
            }
            Entry::Occupied(_) => {
                panic!("start_clip() called with a clip that already exists");
            }
        }
    }

    fn process_chunk(&mut self, clip_id: ClipId, index: isize, preprocessed_data: &[Complex<f32>]) {
        if let Some(processor) = self.clips.get_mut(&clip_id) {
            processor.process_chunk(
                index,
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
    clip_id: ClipId,
    clip_name: String,
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
    asr_signal_bins: usize,
    asr_ifft: RealIfft,
    asr_overlap_reduce: OverlapReduce<f32>,
    asr_interpolator: CubicInterpolator<f32>,
    asr: Option<(Rechunker<f32>, AsrStream)>,
    active_transmission: Option<TransmissionId>,
    output_sample_rate: f64,
    clip_reference_time: f64,
    clip_chunk_size: usize,
    clip_sample_rate: f64,
}

impl ClipProcessor {
    fn new(
        clip_id: ClipId,
        parameters: &FmProcessorParameters,
        clip_descriptor: &PreprocessedClipDescriptor,
        asr_provider: Option<&AsrProvider>,
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

        let asr_fft_size = 2
            * (0.5 * ifft_size as f64 * asr_provider::SAMPLE_RATE / output_sample_rate).ceil()
                as usize;
        let asr_signal_bins =
            (0.5 * ifft_size as f64 * ASR_CUTOFF_FREQ / output_sample_rate).round() as usize;
        let asr_ifft_sample_rate = asr_fft_size as f64 * output_sample_rate / ifft_size as f64;

        Some(ClipProcessor {
            clip_id,
            clip_name: clip_descriptor.clip_name.clone(),
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
            asr_signal_bins,
            asr_ifft: RealIfft::new(asr_fft_size),
            asr_overlap_reduce: OverlapReduce::new(asr_fft_size / 2),
            asr_interpolator: CubicInterpolator::new(
                asr_ifft_sample_rate / asr_provider::SAMPLE_RATE,
            ),
            asr: asr_provider.map(|provider| {
                let rechunker = Rechunker::new(provider.chunk_samples());
                let stream = provider
                    .create_stream()
                    .expect("Could not create ASR stream");
                (rechunker, stream)
            }),
            active_transmission: None,
            output_sample_rate,
            clip_reference_time: clip_descriptor.reference_time,
            clip_chunk_size: clip_descriptor.chunk_size,
            clip_sample_rate: clip_descriptor.sample_rate,
        })
    }

    fn process_chunk(
        &mut self,
        index: isize,
        preprocessed_data: &[Complex<f32>],
        sender: &mut Sender<FmMessage>,
        transmission_id_factory: &mut IdFactory,
    ) {
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
                            clip_id: self.clip_id,
                            clip_name: self.clip_name.clone(),
                            reference_time: self.clip_reference_time,
                            start_index: index,
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

        // Automatic speech recognition (ASR)
        let transcription = if let Some((asr_rechunker, asr_stream)) = self.asr.as_mut() {
            // Apply AA filter by selecting only bins below the cutoff freq
            let demodulated_bin_count = self.demodulated_fft.size() / 2 + 1;
            let asr_bin_count = self.asr_ifft.size() / 2 + 1;
            let mut asr_spectrum =
                vec![Complex::ZERO; asr_bin_count * fft_count].into_boxed_slice();
            for (in_fft, out_fft) in demodulated_spectrum
                .chunks_exact(demodulated_bin_count)
                .zip(asr_spectrum.chunks_exact_mut(asr_bin_count))
            {
                // Copy in FFT data
                out_fft[..self.asr_signal_bins].clone_from_slice(&in_fft[..self.asr_signal_bins]);
            }

            // IFFT (downsamples because IFFT is shorter than FFT)
            let asr_data = self.asr_ifft.process(asr_spectrum);
            // Reconstruct asr signal at new sample rate
            let asr_data = self.asr_overlap_reduce.process(&asr_data);
            let asr_data = self.asr_interpolator.process(&asr_data);

            let mut transcription = None;
            asr_rechunker.process(&asr_data, |asr_data| {
                // Convert f32 to i16 for ASR
                let asr_i16_data: Box<[i16]> = asr_data
                    .iter()
                    .map(|&sample| (sample.clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();

                // Send to ASR stream and get transcription

                if let Some(t) = asr_stream
                    .transcribe(asr_i16_data)
                    .expect("Error running ASR")
                {
                    transcription
                        .get_or_insert_with(|| String::new())
                        .push_str(&t);
                }
            });
            transcription
        } else {
            None
        };

        sender
            .send(FmMessage::PushChunk {
                transmission_id: *active_transmission_id,
                iq_data,
                audio_data,
                transcription,
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
        clip_id: ClipId,
        clip_name: String,
        reference_time: f64,
        start_index: isize,
        period: f64,
        iq_sample_rate: f64,
    },
    EndTransmission(TransmissionId),
    PushChunk {
        transmission_id: TransmissionId,
        iq_data: Box<[Complex<f32>]>,
        audio_data: Box<[f32]>,
        transcription: Option<String>,
    },
}

impl std::fmt::Debug for FmMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reset => write!(f, "Reset"),
            Self::StartTransmission {
                transmission_id,
                clip_id,
                clip_name,
                reference_time,
                start_index,
                period,
                iq_sample_rate,
            } => f
                .debug_struct("StartTransmission")
                .field("transmission_id", transmission_id)
                .field("clip_id", clip_id)
                .field("clip_name", clip_name)
                .field("reference_time", reference_time)
                .field("start_index", start_index)
                .field("period", period)
                .field("iq_sample_rate", iq_sample_rate)
                .finish(),
            Self::EndTransmission(arg0) => f.debug_tuple("EndTransmission").field(arg0).finish(),
            Self::PushChunk {
                transmission_id,
                iq_data,
                audio_data,
                transcription,
            } => f
                .debug_struct("PushChunk")
                .field("transmission_id", transmission_id)
                .field("iq_data.len()", &iq_data.len())
                .field("audio_data.len()", &audio_data.len())
                .field("transcription", transcription)
                .finish(),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

pub struct FmUi {
    inspector_state: Option<TransmissionInspectorState>,
    player: Option<Player>,
}

impl FmUi {
    pub fn new() -> Self {
        FmUi {
            inspector_state: None,
            player: None,
        }
    }

    fn inspect_and_play(
        inspector_state: &mut Option<TransmissionInspectorState>,
        player: &mut Option<Player>,
        transmission_id: TransmissionId,
        time: f64,
    ) {
        let inspector = inspector_state.get_or_insert_with(|| TransmissionInspectorState {
            transmission_id,
            time,
            play_state: PlayState::Paused,
        });

        inspector.time = time;
        inspector.transmission_id = transmission_id;

        // Play this transmission
        if inspector.play_state != PlayState::Play {
            inspector.play_state = PlayState::PlayTemp {
                seek_on_release: time,
            };
        }
        *player = None; // Invalidate the player due to seek
    }

    fn stop_temp_play(
        inspector_state: &mut TransmissionInspectorState,
        player: &mut Option<Player>,
        seek_time: f64,
    ) {
        if inspector_state.play_state != PlayState::Play {
            inspector_state.time = seek_time;
            *player = None; // Invalidate the player due to seek
        }

        // Clear temp_play
        inspector_state.play_state = PlayState::Paused;
    }
}

impl ProcessorUi for FmUi {
    fn draw_clip(
        &mut self,
        history: &mut Box<dyn ProcessorHistory>,
        ui: &mut egui::Ui,
        figure_painter: &egui::Painter,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        _dt: f64,
        clip_id: ClipId,
        clip_response: &mut egui::Response,
    ) {
        let fm_history = history
            .as_any_mut()
            .downcast_mut::<FmHistory>()
            .expect("FmUi should only be used with FmHistory");

        let freq_min = fm_history.frequency - 0.5 * fm_history.bandwidth;
        let freq_max = fm_history.frequency + 0.5 * fm_history.bandwidth;
        for (transmission_id, transmission) in fm_history.transmissions.iter() {
            if transmission.chunks.is_empty() || transmission.clip_id != clip_id {
                continue;
            }

            ui.push_id(ui.id().with(("transmission", transmission_id)), |ui| {
                let start_time = transmission.time(transmission.chunks.start_index() as f64);
                let end_time = transmission.time(transmission.chunks.end_index() as f64);

                // Determine playhead position if this transmission is being inspected
                let playhead = self
                    .inspector_state
                    .as_ref()
                    .filter(|s| s.transmission_id == *transmission_id)
                    .map(|s| s.time);

                let StreamTransmissionResponse {
                    response,
                    pressed_at,
                } = stream_transmission_ui(
                    start_time,
                    end_time,
                    freq_min,
                    freq_max,
                    playhead,
                    ui,
                    figure_painter,
                    figure_rect,
                    viewport,
                );

                // Handle inspector state updates based on user interaction
                if let Some(time) = pressed_at {
                    // Handle click and drag behavior similar to canvas
                    if response.hovered() && ui.ctx().input(|i| i.pointer.primary_pressed()) {
                        Self::inspect_and_play(
                            &mut self.inspector_state,
                            &mut self.player,
                            *transmission_id,
                            time,
                        )
                    }
                }

                // Pass hover/click down to parent
                *clip_response = clip_response.union(response.clone());

                egui::Popup::context_menu(&response)
                    .id(ui.id().with("context_menu"))
                    .show(|ui| {
                        if ui.button("Export audio...").clicked() {
                            ui.close();

                            // Sanitize the channel name for use as a filename
                            let default_name =
                                format!("fm.wav").replace(" ", "_").replace("/", "_");

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
            });
        }
    }

    fn draw(
        &mut self,
        history: &mut Box<dyn ProcessorHistory>,
        ui: &mut egui::Ui,
        id: egui::Id,
        dt: f64,
    ) {
        let fm_history = history
            .as_any_mut()
            .downcast_mut::<FmHistory>()
            .expect("FmUi should only be used with FmHistory");

        let mut close_inspector = false;

        // Show list of available transmissions
        egui::ScrollArea::vertical()
            .max_height(400.0)
            .show(ui, |ui| {
                for (transmission_id, transmission) in fm_history.transmissions.iter() {
                    if transmission.chunks.is_empty() {
                        continue;
                    }
                    let start_time = transmission.time(transmission.chunks.start_index() as f64);
                    let end_time = transmission.time(transmission.chunks.end_index() as f64);
                    let duration = end_time - start_time;

                    let is_current = self
                        .inspector_state
                        .as_ref()
                        .map_or(false, |s| s.transmission_id == *transmission_id);

                    let label = format!(
                        "{} | {:.2}s - {:.2}s ({:.2}s)",
                        transmission.clip_name, start_time, end_time, duration
                    );

                    let response = ui.add(
                        egui::Button::new(label)
                            .selected(is_current)
                            .sense(egui::Sense::click_and_drag()),
                    );

                    // Handle click and drag behavior similar to canvas
                    if response.hovered() && ui.ctx().input(|i| i.pointer.primary_pressed()) {
                        Self::inspect_and_play(
                            &mut self.inspector_state,
                            &mut self.player,
                            *transmission_id,
                            start_time,
                        );
                    }
                }
            });

        // This variable is slightly different from inspector.play_state
        // because it may be set to false if the end of a transmission has been reached
        let mut playing = false;

        if let Some(inspector) = &mut self.inspector_state
            && let Some(transmission) = fm_history.transmissions.get(&inspector.transmission_id)
        {
            // Handle mouse release--stop temp play
            if let PlayState::PlayTemp { seek_on_release } = inspector.play_state
                && !ui.ctx().input(|i| i.pointer.primary_down())
            {
                Self::stop_temp_play(inspector, &mut self.player, seek_on_release);
            }

            // Advance the inspector playhead
            if !matches!(inspector.play_state, PlayState::Paused) {
                inspector.time += dt;
                playing = true;
            }

            // Clamp inspector time to the bounds of the transmission being inspected
            let start_time = transmission.time(transmission.chunks.start_index() as f64);
            let end_time = transmission.time(transmission.chunks.end_index() as f64);
            if inspector.time > end_time {
                if inspector.play_state == PlayState::Play {
                    // Advance to next clip if there is one.
                    // Otherwise, stop playing.
                    let next_transmission_id = inspector.transmission_id + 1;
                    if let Some(next_transmission) =
                        fm_history.transmissions.get(&next_transmission_id)
                    {
                        let next_start_time =
                            next_transmission.time(next_transmission.chunks.start_index() as f64);
                        inspector.transmission_id = next_transmission_id;
                        inspector.time = next_start_time;
                        self.player = None; // Invalidate the player due to seek
                    } else {
                        inspector.time = end_time;
                        playing = false;
                    }
                } else {
                    // Clamp to the end of the transmission
                    inspector.time = end_time;
                }
            } else {
                // Clamp inspector playhead to start of clip
                if inspector.time < start_time {
                    inspector.time = start_time;
                }
            }
        } else {
            self.inspector_state = None;
            self.player = None;
        }

        if let Some(inspector) = &mut self.inspector_state {
            ui.add_space(10.0);
            ui.separator();

            // Playback controls
            ui.horizontal(|ui| {
                let (enabled, play_text) = match inspector.play_state {
                    PlayState::PlayTemp { .. } => (false, "⏸"),
                    PlayState::Play => (true, "⏸"),
                    PlayState::Paused => (true, "▶"),
                };

                // Seek backwards button
                let prev_button = ui.add_enabled(enabled, egui::Button::new("⏮"));
                if prev_button.double_clicked() {
                    // Double-click: go to previous transmission if it exists
                    if inspector.transmission_id > 0 {
                        let prev_transmission_id = inspector.transmission_id - 1;
                        if let Some(prev_transmission) =
                            fm_history.transmissions.get(&prev_transmission_id)
                        {
                            let prev_start_time = prev_transmission
                                .time(prev_transmission.chunks.start_index() as f64);
                            inspector.transmission_id = prev_transmission_id;
                            inspector.time = prev_start_time;
                            self.player = None; // Invalidate the player due to seek
                        }
                    }
                } else if prev_button.clicked() {
                    // Single click: go to start of current transmission
                    if let Some(transmission) =
                        fm_history.transmissions.get(&inspector.transmission_id)
                    {
                        let start_time =
                            transmission.time(transmission.chunks.start_index() as f64);
                        inspector.time = start_time;
                        self.player = None; // Invalidate the player due to seek
                    }
                }

                // Play button
                let play_button = ui.add_enabled(enabled, egui::Button::new(play_text));
                if play_button.clicked() {
                    inspector.play_state = match inspector.play_state {
                        PlayState::Play => PlayState::Paused,
                        PlayState::Paused | PlayState::PlayTemp { .. } => PlayState::Play,
                    };
                }

                // Seek forward button
                let next_button = ui.add_enabled(enabled, egui::Button::new("⏭"));
                if next_button.clicked() {
                    if let Some(transmission) =
                        fm_history.transmissions.get(&inspector.transmission_id)
                    {
                        let end_time = transmission.time(transmission.chunks.end_index() as f64);

                        // Try to advance to next transmission
                        let next_transmission_id = inspector.transmission_id + 1;
                        if let Some(next_transmission) =
                            fm_history.transmissions.get(&next_transmission_id)
                        {
                            let next_start_time = next_transmission
                                .time(next_transmission.chunks.start_index() as f64);
                            inspector.transmission_id = next_transmission_id;
                            inspector.time = next_start_time;
                            self.player = None; // Invalidate the player due to seek
                        } else {
                            // No next transmission, go to end of current
                            inspector.time = end_time;
                            self.player = None; // Invalidate the player due to seek
                        }
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    if ui.button("✖").clicked() {
                        close_inspector = true;
                    }
                });
            });

            ui.separator();
            ui.label(format!("Inspecting: {:.3}s", inspector.time));
            ui.separator();
        }

        if let Some(inspector) = &mut self.inspector_state
            && let Some(transmission) = fm_history.transmissions.get(&inspector.transmission_id)
        {
            let mut seek = None;

            // Find the chunk closest to the inspected time
            let chunk_index = (transmission.index(inspector.time) as isize).clamp(
                transmission.chunks.start_index(),
                transmission.chunks.end_index() - 1,
            );
            let chunk = &transmission.chunks[chunk_index];
            ui.label(format!("Chunk index: {}", chunk_index));
            ui.label(format!("Audio samples: {}", chunk.audio_data.len()));
            ui.label(format!("IQ samples: {}", chunk.iq_data.len()));
            // Find the current chunk index based on inspector time, accounting for transcription latency
            let current_chunk_idx =
                transmission.index(inspector.time + fm_history.transcription_latency) as isize;
            let current_transcription_idx = transmission
                .transcription
                .partition_point(|chunk| chunk.chunk_idx < current_chunk_idx)
                .saturating_sub(1);

            ui.label("Transcription:");
            ui.horizontal_wrapped(|ui| {
                for (idx, transcription_chunk) in transmission.transcription.iter().enumerate() {
                    let is_current = idx == current_transcription_idx;

                    // Highlight current chunk
                    let button = if is_current {
                        egui::Button::new(&transcription_chunk.text)
                            .fill(ui.visuals().selection.bg_fill)
                    } else {
                        egui::Button::new(&transcription_chunk.text)
                            .fill(egui::Color32::TRANSPARENT)
                    };

                    // Make chunk clickable to seek
                    if ui.add(button).clicked() {
                        // Convert chunk_idx to time, accounting for transcription latency
                        seek = Some(
                            transmission.time(transcription_chunk.chunk_idx as f64)
                                - fm_history.transcription_latency,
                        );
                    }
                }
            });

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
            let iq_plot_id = egui::Id::new((id, inspector.transmission_id, "iq_max"));
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
            let plot_width = 300.0;
            let plot_height = 200.0;
            let num_audio_samples = chunk.audio_data.len();
            let audio_stride = (num_audio_samples as f32 / plot_width).ceil().max(1.0) as usize;

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

            // Handle audio playback
            let time = inspector.time;

            if playing {
                let player = self.player.get_or_insert_with(|| Player {
                    audio_output: AudioOutput::new().unwrap(),
                    next_seq_num: chunk_index,
                    time_adj: 0.,
                });
                // Feed new audio data to the audio player
                let start = player.next_seq_num;
                let end = (transmission.index(time + AUDIO_LOOKAHEAD_DURATION) as isize)
                    .min(transmission.chunks.end_index());
                if end > start {
                    let bufs =
                        transmission
                            .chunks
                            .range(start..end)
                            .enumerate()
                            .map(|(i, chunk)| {
                                let seq_num = start + i as isize;
                                AudioBuffer {
                                    seq_num,
                                    data: chunk.audio_data.clone(),
                                }
                            });

                    let FeedResult {
                        last_played_seq_num,
                        underrun,
                    } = player.audio_output.feed(bufs).unwrap(); // TODO replace unwrap
                    if underrun {
                        eprintln!("Audio underrun!");
                    }
                    player.next_seq_num = end;

                    // Look up last_played_seq_num and set_time based on it,
                    // to keep the inspector time (playhead)
                    // synchronized to the actual audio rate
                    if let Some(last_played_seq_num) = last_played_seq_num {
                        let player_time = transmission.time(last_played_seq_num as f64);
                        let chunk_time = transmission.time(chunk_index as f64);
                        // Apply strong LPF since we have relatively poor introspection into audio
                        let new_adj = (player_time - chunk_time) as f32;
                        let alpha = 0.0001;
                        player.time_adj += alpha * (new_adj - player.time_adj);
                        inspector.time += player.time_adj as f64;
                    }
                }
            } else {
                // Not playing - clear the player
                self.player = None;
            }

            if let Some(time) = seek {
                inspector.time = time;
                self.player = None;
            }
        } else {
            self.inspector_state = None;
            self.player = None;
        }

        if close_inspector {
            self.inspector_state = None;
            self.player = None;
        }
    }
}

pub struct FmHistory {
    frequency: f64,
    bandwidth: f64,
    receiver: Receiver<FmMessage>,
    pub transmissions: BTreeMap<TransmissionId, FmTransmission>,
    pub transcription_latency: f64,
}

impl FmHistory {
    pub fn new(
        frequency: f64,
        bandwidth: f64,
        receiver: Receiver<FmMessage>,
        transcription_latency: f64,
    ) -> Self {
        FmHistory {
            frequency,
            bandwidth,
            receiver,
            transmissions: BTreeMap::new(),
            transcription_latency,
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
                    clip_id,
                    clip_name,
                    reference_time,
                    start_index,
                    period,
                    iq_sample_rate,
                } => match self.transmissions.entry(transmission_id) {
                    Entry::Vacant(e) => {
                        e.insert(FmTransmission::new(
                            clip_id,
                            clip_name,
                            reference_time,
                            start_index,
                            period,
                            iq_sample_rate,
                        ));
                    }
                    Entry::Occupied(_) => {
                        panic!("Tried to add a new transmission that already exists");
                    }
                },
                FmMessage::EndTransmission(transmission_id) => {
                    if let Some(transmission) = self.transmissions.get_mut(&transmission_id) {
                        transmission.active = false;
                    }
                    // Transmission may be missing if it was expired
                }
                FmMessage::PushChunk {
                    transmission_id,
                    iq_data,
                    audio_data,
                    transcription,
                } => {
                    if let Some(transmission) = self.transmissions.get_mut(&transmission_id) {
                        transmission.push(iq_data, audio_data, transcription);
                    }
                    // Transmission may be missing if it was expired
                }
            }
        }
    }

    fn expire(&mut self, retain_time: f64) {
        self.transmissions
            .retain(|_, transmission| transmission.prune_old_data(retain_time));
    }

    fn new_ui(&self) -> Box<dyn ProcessorUi> {
        Box::new(FmUi::new())
    }

    fn name(&self) -> &str {
        "FM Demodulator"
    }

    fn has_data(&self) -> bool {
        !self.transmissions.is_empty()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub struct FmTransmission {
    active: bool,
    clip_id: ClipId,
    clip_name: String,
    reference_time: f64,
    period: f64,
    iq_sample_rate: f64,
    chunks: ChunkedDeque<FmTransmissionChunk>,
    transcription: VecDeque<TranscriptionChunk>,
}

impl FmTransmission {
    fn new(
        clip_id: ClipId,
        clip_name: String,
        reference_time: f64,
        start_index: isize,
        period: f64,
        iq_sample_rate: f64,
    ) -> FmTransmission {
        FmTransmission {
            active: true,
            clip_id: clip_id,
            clip_name,
            reference_time,
            period,
            iq_sample_rate,
            chunks: ChunkedDeque::starting_at(start_index),
            transcription: VecDeque::new(),
        }
    }

    fn push(
        &mut self,
        iq_data: Box<[Complex<f32>]>,
        audio_data: Box<[f32]>,
        transcription: Option<String>,
    ) {
        self.chunks.push_back(FmTransmissionChunk {
            iq_data,
            audio_data,
        });
        if let Some(text) = transcription {
            self.transcription.push_back(TranscriptionChunk {
                chunk_idx: self.chunks.end_index(),
                text,
            })
        }
    }

    fn prune_old_data(&mut self, retain_time: f64) -> bool {
        let cutoff_index = self.index(retain_time) as isize;
        self.chunks.remove_front(cutoff_index);

        let transcription_cutoff_index = self
            .transcription
            .partition_point(|&TranscriptionChunk { chunk_idx, .. }| chunk_idx < cutoff_index);
        self.transcription.drain(..transcription_cutoff_index);

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

pub struct TranscriptionChunk {
    pub chunk_idx: isize,
    pub text: String,
}

///////////////////////////////////////////////////////////////////////////////

const IQ_PLOT_MARGIN: f32 = 1.5;
const AUDIO_LOOKAHEAD_DURATION: f64 = 0.2;

struct Player {
    audio_output: AudioOutput,
    next_seq_num: isize,
    time_adj: f32,
}
