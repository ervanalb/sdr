use crate::{
    hardware::ReceiveStreamChunk,
    preprocessor::{PreprocessedChunk, Preprocessor},
    seqdeque::SeqDeque,
};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use std::{
    any::Any,
    collections::{BTreeMap, btree_map::Entry},
    sync::{
        Arc,
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    thread::{JoinHandle, spawn},
};

const PROCESSING_INPUT_CHANNEL_SIZE: usize = 100;
const PROCESSING_OUTPUT_CHANNEL_SIZE: usize = 100;

type ProcessorId = usize;

pub struct RawHistory {
    // TODO: Consider putting active_streams into
    // ReceiveStreamChunk
    history: SeqDeque<Arc<ReceiveStreamChunk>>,
    processors: BTreeMap<ProcessorId, MainThreadProcessorState>,
    last_processor_history_start: usize,
    last_processor_history_end: usize,
    processing_thread_sender: SyncSender<ProcessingInputMessage>,
    processing_thread_receiver: Receiver<ProcessingOutputMessage>,
    processing_thread_handle: JoinHandle<()>,
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
            let mut history = SeqDeque::new();
            let mut preprocessor = Preprocessor::new();
            let mut preprocessor_next_seq_num: usize = 0;
            let mut processors = BTreeMap::<ProcessorId, ChildThreadProcessorState>::new();

            while let Ok(mut msg) = child_thread_receiver.recv() {
                'interrupted: loop {
                    // Add and remove history entries
                    for new_history_entry in msg.new_history_entries.into_iter() {
                        history.push_back(new_history_entry);
                    }
                    history.remove_front(msg.history_start);

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
                                    next_seq_num: history.start_index(),
                                });
                            }
                        }
                    }

                    // Reset the preprocessor if its next_seq_num is earlier than the start of history.
                    if preprocessor_next_seq_num < history.start_index() {
                        preprocessor.reset();
                        preprocessor_next_seq_num = history.start_index();
                    }

                    // Reset any processors whose next_seq_num is earlier than the start of history.
                    // Also send a message so that processor's history will get reset too.
                    for (processor_id, processor_state) in processors.iter_mut() {
                        if processor_state.next_seq_num < history.start_index() {
                            processor_state.processor.reset();
                            processor_state.next_seq_num = history.start_index();
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
                        preprocessor.reset();
                        processing_start_index = history.start_index();
                    } else {
                        processing_start_index = preprocessor_next_seq_num;
                    }

                    // Process unprocessed chunks
                    for (i, chunk) in history.range(processing_start_index..).enumerate() {
                        let seq_num = i + processing_start_index;

                        let preprocessed_chunk = preprocessor.process(chunk);

                        // Collect each processor that has work to do for this chunk
                        let work: Vec<_> = processors
                            .iter_mut()
                            .filter_map(|(processor_id, processor_state)| {
                                (processor_state.next_seq_num <= seq_num).then(|| {
                                    processor_state.next_seq_num = seq_num;
                                    (*processor_id, &mut processor_state.processor)
                                })
                            })
                            .collect();

                        // Run work in parallel, sending back the result
                        work.into_par_iter().for_each(|(processor_id, processor)| {
                            let result = processor.process(&preprocessed_chunk);

                            child_thread_sender
                                .try_send(ProcessingOutputMessage {
                                    processor_id,
                                    msg_type: ProcessingOutputMessageType::Data(result),
                                })
                                .unwrap();
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
            history: SeqDeque::new(),
            processors: BTreeMap::new(),
            last_processor_history_start: 0,
            last_processor_history_end: 0,
            processing_thread_sender: main_thread_sender,
            processing_thread_receiver: main_thread_receiver,
            processing_thread_handle,
        }
    }

    pub fn extend(&mut self, chunks: impl Iterator<Item = ReceiveStreamChunk>) {
        // Add new messages
        for chunk in chunks {
            self.history.push_back(Arc::new(chunk));
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
            Err(TryRecvError::Disconnected) => panic!("Processing thread crashed"),
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
        let history_start = self.history.start_index();
        let history_end = self.history.end_index();
        // Don't bother sending a message if nothing changed
        if processors_changed
            || history_start != self.last_processor_history_start
            || history_end != self.last_processor_history_end
        {
            let new_history_entries: Vec<_> = self
                .history
                .range(self.last_processor_history_end..)
                .cloned()
                .collect();

            let msg = ProcessingInputMessage {
                processor_parameters: processors_changed.then(|| processor_parameters.clone()),
                new_history_entries,
                history_start,
            };
            self.processing_thread_sender.try_send(msg).unwrap();

            self.last_processor_history_start = history_start;
            self.last_processor_history_end = history_end;
        }
    }

    pub fn expire(&mut self, retain_time: DateTime<Utc>) {
        let new_start = self
            .history
            .partition_point(|chunk| chunk.time < retain_time);
        self.history.remove_front(new_start);

        for processor in self.processors.values_mut() {
            processor.history.expire(retain_time);
        }
    }
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
    new_history_entries: Vec<Arc<ReceiveStreamChunk>>,
    history_start: usize,
}

struct ProcessingOutputMessage {
    processor_id: ProcessorId,
    msg_type: ProcessingOutputMessageType,
}

pub enum ProcessingOutputMessageType {
    Reset,
    Data(Box<dyn Any + Send>),
}

pub trait ProcessorParameters: Send + Sync {
    fn create_history(&self) -> Box<dyn ProcessorHistory>;
    fn create_processor(&self) -> Box<dyn Processor>;
}

pub trait ProcessorHistory {
    fn push(&mut self, data: Box<dyn Any>);
    fn reset(&mut self);
    fn expire(&mut self, retain_time: DateTime<Utc>);
}

pub trait Processor: Send {
    fn reset(&mut self);
    fn process(&mut self, chunk: &PreprocessedChunk) -> Box<dyn Any + Send>;
}
