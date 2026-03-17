use crate::{
    hardware::{HardwareResult, ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId},
    preprocessor::StreamPreprocessor,
    processor::{CreationContext, Processor, ProcessorHistory, ProcessorParameters},
    seqdeque::SeqDeque,
    ui::Viewport,
};
use chrono::{DateTime, TimeDelta, Utc};
use rayon::prelude::*;
use std::{
    any::Any,
    cmp::Reverse,
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
    sync::{
        Arc,
        mpsc::{Sender, channel},
    },
    thread::{JoinHandle, spawn},
};

pub type ProcessorId = usize;

pub struct RawHistory {
    streams: BTreeMap<StreamId, StreamHistory>,
    chunks: SeqDeque<Arc<ReceiveStreamChunk>>,

    // TODO: consider moving these to a separate module
    processors: BTreeMap<ProcessorId, MainThreadProcessorState>,
    last_processor_history_start: usize,
    last_processor_history_end: usize,
    processing_thread_sender: Sender<ProcessingInputMessage>,
    _processing_thread_handle: JoinHandle<()>,
}

impl RawHistory {
    pub fn new() -> RawHistory {
        let (main_thread_sender, child_thread_receiver) = channel::<ProcessingInputMessage>();

        // For the thread
        let _processing_thread_handle = spawn(move || {
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

                    // Add and remove any processor states that have changed
                    for processor_id in msg.removed_processors {
                        processors
                            .remove(&processor_id)
                            .expect("Tried to remove a non-existant processor");
                    }
                    for (processor_id, processor) in msg.new_processors {
                        match processors.entry(processor_id) {
                            Entry::Vacant(e) => {
                                e.insert(ChildThreadProcessorState {
                                    processor,
                                    next_seq_num: chunks.start_index(),
                                });
                            }
                            Entry::Occupied(_) => {
                                panic!("Tried to add a processor that already exists");
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
                    for processor_state in processors.values_mut() {
                        if processor_state.next_seq_num < chunks.start_index() {
                            processor_state.processor.reset();
                            processor_state.next_seq_num = chunks.start_index();
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
                        preprocessor_next_seq_num = seq_num + 1;

                        // Collect each processor that has work to do for this chunk
                        let work: Vec<_> = processors
                            .values_mut()
                            .filter(|processor_state| processor_state.next_seq_num <= seq_num)
                            .map(|processor_state| {
                                // Advance this processor's seq_num
                                processor_state.next_seq_num = seq_num + 1;
                                &mut processor_state.processor
                            })
                            .collect();

                        // Run work in parallel, sending back the result
                        work.into_par_iter().for_each(|processor| {
                            // End streams
                            for stream_id in preprocessed_stream_ends.iter() {
                                processor.end_stream(*stream_id);
                            }

                            // Start streams
                            for (stream_id, descriptor) in preprocessed_stream_starts.iter() {
                                processor.start_stream(*stream_id, descriptor);
                            }

                            // Process
                            processor.process_chunk(
                                chunk.stream_id,
                                chunk.time,
                                &preprocessed_data,
                            );
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
            _processing_thread_handle,
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
        cc: &CreationContext<'_>,
    ) {
        // 1. Add & remove any processor states, as per processor_params

        // Remove:
        let mut removed_processors = vec![];
        self.processors.retain(|&processor_id, _| {
            let keep = processor_parameters.contains_key(&processor_id);
            if !keep {
                removed_processors.push(processor_id);
            }
            keep
        });

        // Add:
        let mut new_processors = vec![];
        for (processor_id, processor_parameters) in processor_parameters.iter() {
            if let Entry::Vacant(e) = self.processors.entry(*processor_id) {
                let (processor, history) = processor_parameters.create_processor(cc);
                e.insert(MainThreadProcessorState {
                    last_parameters: processor_parameters.clone(),
                    history,
                });
                new_processors.push((*processor_id, processor));
            }
        }

        // Processors aren't allowed to change params without being assigned a new ID,
        // so enforce this by setting all params to their previous values
        for (processor_id, processor_state) in self.processors.iter() {
            processor_parameters.insert(*processor_id, processor_state.last_parameters.clone());
        }

        // 2. Update each processor history (e.g. receive processed data from thread)
        for processor_state in self.processors.values_mut() {
            processor_state.history.update();
        }

        // 3. Send history & parameter updates to the processing thread
        let chunks_start = self.chunks.start_index();
        let history_end = self.chunks.end_index();
        // Don't bother sending a message if nothing changed
        if !removed_processors.is_empty()
            || !new_processors.is_empty()
            || chunks_start != self.last_processor_history_start
            || history_end != self.last_processor_history_end
        {
            let new_chunks: Vec<_> = self
                .chunks
                .range(self.last_processor_history_end..)
                .cloned()
                .collect();

            let msg = ProcessingInputMessage {
                removed_processors,
                new_processors,
                new_chunks,
                chunks_start,
                streams: self.streams.clone(),
            };
            self.processing_thread_sender.send(msg).unwrap();

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
        for (processor_id, processor) in self.processors.iter() {
            processor
                .history
                .draw(ui, egui::Id::new(processor_id), figure_rect, viewport, dt);
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
    removed_processors: Vec<ProcessorId>,
    new_processors: Vec<(ProcessorId, Box<dyn Processor>)>,
    new_chunks: Vec<Arc<ReceiveStreamChunk>>,
    chunks_start: usize,
    streams: BTreeMap<StreamId, StreamHistory>,
}

pub enum ProcessingOutputMessageType {
    Reset,
    Data(Box<dyn Any + Send>),
}
