use crate::{
    hardware::{HardwareResult, ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId},
    preprocessor::StreamPreprocessor,
    processor::{Processor, ProcessorHistory, ProcessorParameters},
    seqdeque::SeqDeque,
    ui::Viewport,
};
use chrono::{DateTime, TimeDelta, Utc};
use num_complex::Complex;
use rayon::prelude::*;
use std::{
    any::Any,
    cmp::Reverse,
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
    panic,
    sync::{
        Arc,
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    thread::{JoinHandle, spawn},
};

const PROCESSING_INPUT_CHANNEL_SIZE: usize = 100;
const PROCESSING_OUTPUT_CHANNEL_SIZE: usize = 100;

pub type ProcessorId = usize;

pub struct RawHistory {
    streams: BTreeMap<StreamId, StreamHistory>,
    chunks: SeqDeque<Arc<ReceiveStreamChunk>>,

    // TODO: consider moving these to a separate module
    processors: BTreeMap<ProcessorId, MainThreadProcessorState>,
    last_processor_history_start: usize,
    last_processor_history_end: usize,
    processing_thread_sender: SyncSender<ProcessingInputMessage>,
    processing_thread_receiver: Receiver<ProcessingOutputMessage>,
    processing_thread_handle: Option<JoinHandle<()>>,
}

impl RawHistory {
    pub fn new() -> RawHistory {
        let (main_thread_sender, child_thread_receiver) =
            sync_channel::<ProcessingInputMessage>(PROCESSING_INPUT_CHANNEL_SIZE);
        let (child_thread_sender, main_thread_receiver) =
            sync_channel::<ProcessingOutputMessage>(PROCESSING_OUTPUT_CHANNEL_SIZE);

        // For the thread
        let processing_thread_handle = spawn(move || {
            // We maintain a carbon copy of the history inside of the processing thread
            // to avoid lock contention in the radio hardware threads--
            // they write to the one in the main thread,
            // and updates get sent to this thread when process() is called
            // so it stays in sync.
            // The actual buffers are behind Arc, so only metadata is copied.
            let mut chunks = SeqDeque::new();
            let mut streams;
            let mut preprocessors = BTreeMap::new();
            let mut preprocessor_next_seq_num: usize = 0;
            let mut processors = BTreeMap::<ProcessorId, ChildThreadProcessorState>::new();

            while let Ok(mut msg) = child_thread_receiver.recv() {
                'interrupted: loop {
                    // Add and remove history entries & update stream spans
                    for new_history_entry in msg.new_chunks.into_iter() {
                        chunks.push_back(new_history_entry);
                    }
                    chunks.remove_front(msg.chunks_start);
                    streams = msg.streams;

                    // Add and remove any processor states
                    if let Some(processor_parameters) = msg.processor_parameters {
                        // Remove:
                        processors.retain(|&processor_id, _| {
                            processor_parameters.contains_key(&processor_id)
                        });

                        // Add:
                        for (processor_id, processor_parameters) in processor_parameters.iter() {
                            if let Entry::Vacant(e) = processors.entry(*processor_id) {
                                e.insert(ChildThreadProcessorState {
                                    processor: processor_parameters.create_processor(),
                                    next_seq_num: chunks.start_index(),
                                });
                            }
                        }
                    }

                    // Reset the preprocessor if its next_seq_num is earlier than the start of history.
                    if preprocessor_next_seq_num < chunks.start_index() {
                        preprocessors.clear();
                        preprocessor_next_seq_num = chunks.start_index();
                    }

                    // Reset any processors whose next_seq_num is earlier than the start of history.
                    // Also send a message so that processor's history will get reset too.
                    for (processor_id, processor_state) in processors.iter_mut() {
                        if processor_state.next_seq_num < chunks.start_index() {
                            processor_state.processor.reset();
                            processor_state.next_seq_num = chunks.start_index();
                            child_thread_sender
                                .try_send(ProcessingOutputMessage {
                                    processor_id: *processor_id,
                                    msg_type: ProcessingOutputMessageType::Reset,
                                })
                                .unwrap();
                        }
                    }

                    // If any processor's next_seq_num predates the preprocessor's next_seq_num,
                    // then we need to restart from the beginning.
                    // Otherwise, we will start where we left off
                    // with the preprocessor's next_seq_num.
                    let processing_start_index;
                    if processors.values().any(|processor_state| {
                        processor_state.next_seq_num < preprocessor_next_seq_num
                    }) {
                        preprocessors.clear();
                        processing_start_index = chunks.start_index();
                    } else {
                        processing_start_index = preprocessor_next_seq_num;
                    }
                    let processing_span = processing_start_index..chunks.end_index();

                    // Look through the stream spans and generate two lists of events
                    // for streams starting and stopping
                    let mut stream_starts = vec![];
                    let mut stream_ends = vec![];
                    for (stream_id, stream_history) in streams.iter() {
                        if processing_span.contains(&stream_history.span.start) {
                            stream_starts.push((
                                stream_history.span.start,
                                *stream_id,
                                &stream_history.descriptor,
                            ));
                        }
                        if processing_span.contains(&stream_history.span.end) {
                            stream_ends.push((stream_history.span.end, *stream_id));
                        }
                    }
                    stream_starts.sort_by_key(|&(seq_num, _, _)| Reverse(seq_num));
                    stream_ends.sort_by_key(|&(seq_num, _)| Reverse(seq_num));

                    // Process unprocessed chunks
                    for (i, chunk) in chunks.range(processing_start_index..).enumerate() {
                        let seq_num = i + processing_start_index;

                        // Preprocessor: end streams
                        let mut preprocessed_stream_ends = vec![];
                        while let Some(&(stream_end_seq_num, stream_id)) = stream_ends.last()
                            && stream_end_seq_num == seq_num
                        {
                            preprocessors
                                .remove(&stream_id)
                                .expect("Closed a stream that didn't exist");
                            preprocessed_stream_ends.push(stream_id);
                            stream_ends.pop();
                        }

                        // Preprocessor: start streams
                        let mut preprocessed_stream_starts = vec![];
                        while let Some(&(stream_start_seq_num, stream_id, descriptor)) =
                            stream_starts.last()
                            && stream_start_seq_num == seq_num
                        {
                            preprocessors.entry(stream_id).or_insert_with(|| {
                                let (processor, descriptor) = StreamPreprocessor::new(descriptor);
                                preprocessed_stream_starts.push((stream_id, descriptor));
                                processor
                            });
                            stream_starts.pop();
                        }

                        // Preprocess
                        let stream_processor = preprocessors
                            .get_mut(&chunk.stream_id)
                            .expect("Stream's preprocessor does not exist");
                        let preprocessed_data = stream_processor.process(&chunk.chunk);
                        let preprocessed_chunk = PreprocessedChunk {
                            stream_id: chunk.stream_id,
                            time: chunk.time,
                            chunk: preprocessed_data,
                        };

                        // Collect each processor that has work to do for this chunk
                        let work: Vec<_> = processors
                            .iter_mut()
                            .filter(|(_processor_id, processor_state)| {
                                processor_state.next_seq_num <= seq_num
                            })
                            .map(|(processor_id, processor_state)| {
                                // Advance this processor's seq_num
                                processor_state.next_seq_num = seq_num;
                                (*processor_id, &mut processor_state.processor)
                            })
                            .collect();

                        // Run work in parallel, sending back the result
                        work.into_par_iter().for_each(|(processor_id, processor)| {
                            // End streams
                            for stream_id in preprocessed_stream_ends.iter() {
                                processor.end_stream(*stream_id);
                            }

                            // Start streams
                            for (stream_id, descriptor) in preprocessed_stream_starts.iter() {
                                processor.start_stream(*stream_id, descriptor);
                            }

                            // Process & send message
                            if let Some(result) = processor.process_chunk(&preprocessed_chunk) {
                                child_thread_sender
                                    .try_send(ProcessingOutputMessage {
                                        processor_id,
                                        msg_type: ProcessingOutputMessageType::Data(result),
                                    })
                                    .unwrap();
                            }
                        });

                        // After processing each chunk, see if we have received new parameters.
                        // If so, stop processing chunks.
                        if let Ok(new_msg) = child_thread_receiver.try_recv() {
                            msg = new_msg;
                            continue 'interrupted;
                        }
                    }
                    break;
                }
            }
        });

        RawHistory {
            chunks: SeqDeque::new(),
            streams: BTreeMap::new(),

            processors: BTreeMap::new(),
            last_processor_history_start: 0,
            last_processor_history_end: 0,
            processing_thread_sender: main_thread_sender,
            processing_thread_receiver: main_thread_receiver,
            processing_thread_handle: Some(processing_thread_handle),
        }
    }

    pub fn update(&mut self, result: HardwareResult) {
        if result.chunks.is_empty() {
            return; // Avoid inserting zero-length spans
        }

        // Add new messages
        let start = self.chunks.end_index();
        for chunk in result.chunks {
            self.chunks.push_back(Arc::new(chunk));
        }
        let end = self.chunks.end_index();

        // Update stream spans
        for (stream_id, descriptor) in result.active_streams.iter() {
            match self.streams.entry(*stream_id) {
                Entry::Occupied(mut e) => {
                    // If this stream is already active,
                    // update the end of its span
                    e.get_mut().span.end = end;
                }
                Entry::Vacant(e) => {
                    // Add this new stream to the history
                    e.insert(StreamHistory {
                        descriptor: descriptor.clone(),
                        span: start..end,
                    });
                }
            }
        }
    }

    pub fn process(
        &mut self,
        processor_parameters: &mut BTreeMap<ProcessorId, Arc<dyn ProcessorParameters>>,
    ) {
        // 1. Add & remove any processor states, as per processor_params

        let mut processors_changed = false;

        // Remove:
        self.processors.retain(|&processor_id, _| {
            let keep = processor_parameters.contains_key(&processor_id);
            if !keep {
                processors_changed = true;
            }
            keep
        });

        // Add:
        for (processor_id, processor) in processor_parameters.iter() {
            if let Entry::Vacant(e) = self.processors.entry(*processor_id) {
                e.insert(MainThreadProcessorState {
                    last_parameters: processor.clone(),
                    history: processor.create_history(),
                });
                processors_changed = true;
            }
        }

        // Processors aren't allowed to change params without being assigned a new ID,
        // so enforce this by setting all params to their previous values
        for (processor_id, processor_state) in self.processors.iter() {
            processor_parameters.insert(*processor_id, processor_state.last_parameters.clone());
        }

        // 2. Read & apply all results from the processing thread
        while let Some(msg) = match self.processing_thread_receiver.try_recv() {
            Ok(msg) => Some(msg),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // Propagate panic
                panic::resume_unwind(
                    self.processing_thread_handle
                        .take()
                        .unwrap()
                        .join()
                        .unwrap_err(),
                );
            }
        } {
            if let Some(processor_state) = self.processors.get_mut(&msg.processor_id) {
                match msg.msg_type {
                    ProcessingOutputMessageType::Reset => {
                        processor_state.history.reset();
                    }
                    ProcessingOutputMessageType::Data(data) => {
                        processor_state.history.push(data);
                    }
                }
            }
        }

        // 3. Send history & parameter updates to the processing thread
        let chunks_start = self.chunks.start_index();
        let history_end = self.chunks.end_index();
        // Don't bother sending a message if nothing changed
        if processors_changed
            || chunks_start != self.last_processor_history_start
            || history_end != self.last_processor_history_end
        {
            let new_chunks: Vec<_> = self
                .chunks
                .range(self.last_processor_history_end..)
                .cloned()
                .collect();

            let msg = ProcessingInputMessage {
                processor_parameters: processors_changed.then(|| processor_parameters.clone()),
                new_chunks,
                chunks_start,
                streams: self.streams.clone(),
            };
            self.processing_thread_sender.try_send(msg).unwrap();

            self.last_processor_history_start = chunks_start;
            self.last_processor_history_end = history_end;
        }
    }

    pub fn expire(&mut self, retain_time: DateTime<Utc>) {
        let new_start = self
            .chunks
            .partition_point(|chunk| chunk.time < retain_time);

        // Remove old chunks
        self.chunks.remove_front(new_start);

        // Remove and/or update stream spans
        self.streams.retain(|_stream_id, stream_history| {
            if new_start >= stream_history.span.end {
                false // discard
            } else {
                if new_start > stream_history.span.start {
                    // adjust the span start
                    stream_history.span.start = new_start;
                }
                true // keep
            }
        });

        for processor in self.processors.values_mut() {
            processor.history.expire(retain_time);
        }
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &Viewport,
        dt: TimeDelta,
    ) {
        for processor in self.processors.values() {
            processor.history.draw(ui, figure_rect, viewport, dt);
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamHistory {
    descriptor: Arc<ReceiveStreamDescriptor>,
    span: Range<usize>,
}

pub struct MainThreadProcessorState {
    last_parameters: Arc<dyn ProcessorParameters>,
    history: Box<dyn ProcessorHistory>,
}

pub struct ChildThreadProcessorState {
    processor: Box<dyn Processor>,
    next_seq_num: usize,
}

struct ProcessingInputMessage {
    processor_parameters: Option<BTreeMap<ProcessorId, Arc<dyn ProcessorParameters>>>,
    new_chunks: Vec<Arc<ReceiveStreamChunk>>,
    chunks_start: usize,
    streams: BTreeMap<StreamId, StreamHistory>,
}

struct ProcessingOutputMessage {
    processor_id: ProcessorId,
    msg_type: ProcessingOutputMessageType,
}

pub enum ProcessingOutputMessageType {
    Reset,
    Data(Box<dyn Any + Send>),
}

pub struct PreprocessedChunk {
    pub stream_id: StreamId,
    pub time: DateTime<Utc>,
    pub chunk: Box<[Complex<f32>]>,
}
