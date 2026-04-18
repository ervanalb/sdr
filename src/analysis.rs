use crate::{
    asr_provider::AsrProvider,
    document::{Clip, ClipDescriptor, ClipId, Document},
    id_factory::IdFactory,
    preprocessor::StreamPreprocessor,
    processor::{Processor, ProcessorHistory, ProcessorParameters},
};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::{Receiver, Sender, channel},
    thread::{JoinHandle, spawn},
};

pub type ProcessorId = usize;
pub type ProcessorInstanceId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ClipCursor {
    Before,
    Index(isize),
    After,
}

#[derive(Clone, Debug)]
struct Cursor {
    cursors: BTreeMap<ClipId, ClipCursor>,
}

impl Cursor {
    fn new_empty() -> Self {
        Cursor {
            cursors: BTreeMap::new(),
        }
    }

    fn start_of_document(document: &Document) -> Self {
        Self {
            cursors: document
                .clips
                .keys()
                .map(|clip_id| (*clip_id, ClipCursor::Before))
                .collect(),
        }
    }

    fn get(&mut self, clip_id: ClipId) -> ClipCursor {
        self.cursors.get(&clip_id).unwrap().clone()
    }

    fn add_clip(&mut self, clip_id: ClipId) {
        self.cursors.insert(clip_id, ClipCursor::Before);
    }

    fn remove_clip(&mut self, clip_id: ClipId) {
        self.cursors.remove(&clip_id);
    }

    // Calculate the time of this cursor position.
    // Returns None if there are no clips in the document
    fn time(&self, document: &Document) -> Option<f64> {
        self.cursors
            .iter()
            .map(|(clip_id, clip_cursor)| {
                Self::clip_time_at_index(document.clips.get(clip_id).unwrap(), *clip_cursor)
            })
            .min_by(|a, b| a.partial_cmp(b).unwrap())
    }

    fn clip_time_at_index(clip: &Clip, clip_cursor: ClipCursor) -> f64 {
        let chunk_index = match clip_cursor {
            ClipCursor::Before => clip.chunks.start_index(),
            ClipCursor::Index(index) => index,
            ClipCursor::After => clip.chunks.end_index(),
        };
        let chunk_period = clip.descriptor.chunk_size as f64 / clip.descriptor.sample_rate;
        clip.descriptor.reference_time
            + (chunk_index - clip.chunks.start_index()) as f64 * chunk_period
    }

    fn is_before(&self, other: &Cursor) -> bool {
        assert_eq!(self.cursors.len(), other.cursors.len());

        for (clip_id, cursor_a) in self.cursors.iter() {
            let cursor_b = other.cursors.get(clip_id).unwrap();
            if cursor_a < cursor_b {
                return true;
            }
        }
        false
    }

    fn advance(&mut self, document: &Document, active_clips: &BTreeSet<ClipId>) -> Option<Event> {
        // Collect all clips with their times and sort by time
        let mut clips_by_time: Vec<_> = self
            .cursors
            .iter()
            .map(|(&clip_id, clip_cursor)| {
                let clip = document.clips.get(&clip_id).unwrap();
                let time = Self::clip_time_at_index(clip, *clip_cursor);
                (
                    time,
                    clip_id,
                    clip.chunks.start_index(),
                    clip.chunks.end_index(),
                )
            })
            .collect();

        clips_by_time.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Try to advance each clip in order of earliest time
        for (_time, clip_id, start_index, end_index) in clips_by_time {
            let clip_cursor = self.cursors.get_mut(&clip_id).unwrap();

            let event = match *clip_cursor {
                ClipCursor::Before => {
                    // Transition from Before to Index(start_index)
                    *clip_cursor = ClipCursor::Index(start_index);
                    Some(Some(Event::ClipStart(clip_id)))
                }
                ClipCursor::Index(chunk_index) => {
                    if chunk_index < end_index {
                        // We have a chunk to process
                        let event = Event::Chunk(clip_id, chunk_index);
                        // Advance to next chunk
                        *clip_cursor = ClipCursor::Index(chunk_index + 1);
                        Some(Some(event))
                    } else {
                        // We're at the end of available chunks
                        if active_clips.contains(&clip_id) {
                            // Clip is still active, don't advance to After,
                            // and furthermore, stop iteration here
                            Some(None) // Return None; don't try to advance a different clip
                        } else {
                            // Clip is finished, transition to After
                            *clip_cursor = ClipCursor::After;
                            Some(Some(Event::ClipEnd(clip_id)))
                        }
                    }
                }
                ClipCursor::After => {
                    // Already finished, nothing to do on this clip
                    None // Don't return; try to advance a different clip
                }
            };

            // If we got an event, return it; otherwise try the next clip
            if let Some(event) = event {
                return event;
            }
        }

        // No clips could be advanced
        None
    }
}

pub struct Analysis {
    instance_id_factory: IdFactory,
    processors: BTreeMap<ProcessorId, MainThreadProcessorState>,
    processing_thread_sender: Sender<ProcessingInputMessage>,
    _processing_thread_handle: JoinHandle<()>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    asr_provider: Option<AsrProvider>,
    prev_document: Document,
}

impl Analysis {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        asr_provider: Option<AsrProvider>,
    ) -> Analysis {
        let (main_thread_sender, child_thread_receiver) = channel::<ProcessingInputMessage>();

        let _processing_thread_handle = spawn(move || {
            processing_thread_loop(child_thread_receiver);
        });

        Analysis {
            instance_id_factory: IdFactory::default(),
            processors: BTreeMap::new(),
            processing_thread_sender: main_thread_sender,
            _processing_thread_handle,
            device,
            queue,
            asr_provider,
            prev_document: Document::new(),
        }
    }

    pub fn process(
        &mut self,
        processor_parameters: &mut BTreeMap<ProcessorId, ProcessorParameters>,
        document: &Document,
        active_clips: &BTreeSet<ClipId>,
    ) {
        // 1. Add & remove processor states
        let mut removed_processors = vec![];
        self.processors.retain(|&processor_id, processor_state| {
            // Keep processor if it exists in parameters and is enabled
            let keep = processor_parameters
                .get(&processor_id)
                .map_or(false, |p| p.enabled);
            if !keep {
                removed_processors.push(processor_state.instance_id);
            }
            keep
        });

        let mut new_processors = vec![];
        for (processor_id, processor_parameters) in processor_parameters.iter() {
            // Only create instances for enabled processors
            if !processor_parameters.enabled {
                continue;
            }

            if !self.processors.contains_key(processor_id) {
                let (processor, history) = processor_parameters
                    .specific_parameters
                    .create_instance(&self.device, &self.queue, self.asr_provider.as_ref());
                let instance_id = self.instance_id_factory.create();
                self.processors.insert(
                    *processor_id,
                    MainThreadProcessorState {
                        instance_id,
                        last_parameters: processor_parameters.clone(),
                        history,
                    },
                );
                new_processors.push((instance_id, processor));
            }
        }

        // Check for specific parameter changes (not name changes)
        for (processor_id, processor_state) in self.processors.iter_mut() {
            let parameters = processor_parameters.get_mut(processor_id).unwrap();
            // Only recreate if specific_parameters changed, not name or enabled
            if parameters.specific_parameters != processor_state.last_parameters.specific_parameters
            {
                removed_processors.push(processor_state.instance_id);
                let instance_id = self.instance_id_factory.create();
                let (processor, history) = parameters.specific_parameters.create_instance(
                    &self.device,
                    &self.queue,
                    self.asr_provider.as_ref(),
                );
                *processor_state = MainThreadProcessorState {
                    instance_id,
                    last_parameters: parameters.clone(),
                    history,
                };
                new_processors.push((instance_id, processor));
            } else {
                // Update last_parameters to reflect name changes without recreating
                processor_state.last_parameters = parameters.clone();
            }
        }

        // 2. Update processor histories
        for processor_state in self.processors.values_mut() {
            processor_state.history.update();
        }

        // 3. Send document snapshot to processing thread
        let msg = ProcessingInputMessage::Process {
            removed_processors,
            new_processors,
            document: document.clone(),
            active_clips: active_clips.clone(),
        };
        self.processing_thread_sender.send(msg).unwrap();
        self.prev_document = document.clone();
    }

    // Call this after calling .expire() on the document.
    // It is invalid to call this if the document has been modified
    // in a way that is not consistent with .expire()
    // since the last call to .process().
    pub fn process_expiry(&mut self, document: &Document, retain_time: f64) {
        // 1. Call expire on all processor histories
        for processor_state in self.processors.values_mut() {
            processor_state.history.expire(retain_time);
        }

        // 2. Send document snapshot to proecssing thread
        let msg = ProcessingInputMessage::ProcessExpiry {
            document: document.clone(),
        };
        self.processing_thread_sender.send(msg).unwrap();
        self.prev_document = document.clone();
    }

    pub fn get_processor_history_mut(
        &mut self,
        processor_id: ProcessorId,
    ) -> Option<&mut Box<dyn ProcessorHistory>> {
        self.processors
            .get_mut(&processor_id)
            .map(|p| &mut p.history)
    }

    /// Get both the processor instance ID and mutable history reference
    pub fn get_processor_instance_and_history_mut(
        &mut self,
        processor_id: ProcessorId,
    ) -> Option<(ProcessorInstanceId, &mut Box<dyn ProcessorHistory>)> {
        self.processors
            .get_mut(&processor_id)
            .map(|p| (p.instance_id, &mut p.history))
    }
}

pub struct MainThreadProcessorState {
    instance_id: ProcessorInstanceId,
    last_parameters: ProcessorParameters,
    history: Box<dyn ProcessorHistory>,
}

struct ChildThreadProcessorState {
    processor: Box<dyn Processor>,
    cursor: Cursor,
}

enum ProcessingInputMessage {
    Process {
        removed_processors: Vec<ProcessorInstanceId>,
        new_processors: Vec<(ProcessorInstanceId, Box<dyn Processor>)>,
        document: Document,
        active_clips: BTreeSet<ClipId>,
    },
    ProcessExpiry {
        document: Document,
    },
}

#[derive(Debug)]
enum Event {
    ClipStart(ClipId),
    ClipEnd(ClipId),
    Chunk(ClipId, isize), // ClipId and chunk index
}

fn processing_thread_loop(child_thread_receiver: Receiver<ProcessingInputMessage>) {
    let mut preprocessors: BTreeMap<ClipId, StreamPreprocessor> = BTreeMap::new();
    let mut prev_document = Document::new();
    let mut prev_active_clips = BTreeSet::new();
    let mut preprocessor_cursor = Cursor::new_empty();
    let mut processors = BTreeMap::<ProcessorInstanceId, ChildThreadProcessorState>::new();

    while let Ok(mut msg) = child_thread_receiver.recv() {
        'interrupted: loop {
            match msg {
                ProcessingInputMessage::Process {
                    removed_processors,
                    document: new_document,
                    new_processors,
                    active_clips,
                } => {
                    // 1. Remove processors
                    for instance_id in removed_processors {
                        processors.remove(&instance_id);
                    }

                    // 2. Get the document update
                    // and see if we need to restart all processors from the beginning.
                    let mut restart_all = false;

                    // 2a. Deleted clips
                    for (clip_id, _prev_clip) in prev_document.removed_clips(&new_document) {
                        // See if this clip removal invalidates the cursor
                        let clip_cursor = preprocessor_cursor.get(clip_id);
                        if matches!(clip_cursor, ClipCursor::Index(_) | ClipCursor::After) {
                            // This clip invalidates all cursors
                            restart_all = true;
                        } else {
                            // Remove this clip from all cursors
                            preprocessor_cursor.remove_clip(clip_id);
                            for processor in processors.values_mut() {
                                processor.cursor.remove_clip(clip_id);
                            }
                        }
                    }

                    // 2b. Modified clips
                    if let Some(cursor_t) = preprocessor_cursor.time(&prev_document) {
                        for (clip_id, prev_clip, new_clip) in
                            prev_document.modified_clips(&new_document)
                        {
                            let ClipDescriptor {
                                name: _,
                                frequency: prev_frequency,
                                sample_rate: prev_sample_rate,
                                reference_time: prev_start_time,
                                chunk_size: prev_chunk_size,
                            } = prev_clip.descriptor;
                            let ClipDescriptor {
                                name: _,
                                frequency,
                                sample_rate,
                                reference_time: start_time,
                                chunk_size,
                            } = new_clip.descriptor;

                            let clip_cursor = preprocessor_cursor.get(clip_id);
                            let new_clip_start_t = new_clip
                                .descriptor
                                .time(new_clip.chunks.start_index() as f64);
                            let clip_affects_history =
                                matches!(clip_cursor, ClipCursor::Index(_) | ClipCursor::After)
                                    || new_clip_start_t < cursor_t;

                            let descriptor_changed = frequency != prev_frequency
                                || sample_rate != prev_sample_rate
                                || start_time != prev_start_time
                                || chunk_size != prev_chunk_size;

                            if clip_affects_history
                                && (descriptor_changed
                                    || !new_clip.chunks.is_continuation_of(&prev_clip.chunks)
                                    || {
                                        // If we are are `After` the clip, it has already been "finalized" in the processors.
                                        // So we don't allow any further edits to the chunk data,
                                        // even if the edits constitute a continuation.
                                        matches!(clip_cursor, ClipCursor::After)
                                            && new_clip.chunks.end_index()
                                                > prev_clip.chunks.end_index()
                                    })
                            {
                                // This clip invalidates all cursors
                                restart_all = true;
                            }
                        }
                    }

                    // 2c. New clips
                    for (clip_id, new_clip) in prev_document.added_clips(&new_document) {
                        if let Some(cursor_t) = preprocessor_cursor.time(&prev_document)
                            && {
                                let clip_start_t = new_clip
                                    .descriptor
                                    .time(new_clip.chunks.start_index() as f64);
                                clip_start_t < cursor_t
                            }
                        {
                            // This clip invalidates all cursors
                            restart_all = true;
                        } else {
                            // Add this clip to all cursors
                            preprocessor_cursor.add_clip(clip_id);
                            for processor in processors.values_mut() {
                                processor.cursor.add_clip(clip_id);
                            }
                        }
                    }

                    // 2d. Invalidate all cursors if the flag was set
                    if restart_all {
                        preprocessor_cursor = Cursor::start_of_document(&new_document);
                        for processor in processors.values_mut() {
                            processor.processor.reset();
                            processor.cursor = preprocessor_cursor.clone();
                        }
                    }

                    // 3. Add new processors
                    for (instance_id, processor) in new_processors {
                        // Adding a new processor means we need to preprocess from the beginning
                        // (but we don't need to invalidate existing processors)
                        preprocessor_cursor = Cursor::start_of_document(&new_document);
                        processors.insert(
                            instance_id,
                            ChildThreadProcessorState {
                                processor,
                                cursor: preprocessor_cursor.clone(),
                            },
                        );
                    }

                    prev_document = new_document.clone();
                    prev_active_clips = active_clips.clone();
                }
                ProcessingInputMessage::ProcessExpiry {
                    document: new_document,
                } => {
                    // 1. Get the document update
                    // and see if we need to restart all processors from the beginning.
                    let mut restart_all = false;

                    // 1a. Deleted clips
                    // These only invalidate the cursor if the cursor is not After them.
                    for (clip_id, _prev_clip) in prev_document.removed_clips(&new_document) {
                        let clip_cursor = preprocessor_cursor.get(clip_id);
                        if matches!(clip_cursor, ClipCursor::Before | ClipCursor::Index(_)) {
                            // This clip invalidates all cursors
                            restart_all = true;
                        } else {
                            // Remove this clip from all cursors.
                            preprocessor_cursor.remove_clip(clip_id);
                            for processor in processors.values_mut() {
                                processor.cursor.remove_clip(clip_id);
                            }
                        }
                    }

                    // 2b. Modified clips
                    // These only invalidate the cursor if the cursor is not after the clip's new start_index
                    // (indicating a discontinuity in the data)
                    for (clip_id, _prev_clip, new_clip) in
                        prev_document.modified_clips(&new_document)
                    {
                        let clip_cursor = preprocessor_cursor.get(clip_id);
                        if matches!(clip_cursor, ClipCursor::Before)
                            || matches!(clip_cursor, ClipCursor::Index(i) if i < new_clip.chunks.start_index())
                        {
                            restart_all = true;
                        }
                    }

                    // 2c. New clips (invalid--expiry never creates new clips)
                    if prev_document.added_clips(&new_document).next().is_some() {
                        panic!("process_expiry() called with added clips--expiry cannot add clips")
                    }

                    // 2d. Invalidate all cursors if the flag was set
                    if restart_all {
                        preprocessor_cursor = Cursor::start_of_document(&new_document);
                        for processor in processors.values_mut() {
                            processor.processor.reset();
                            processor.cursor = preprocessor_cursor.clone();
                        }
                    }

                    prev_document = new_document.clone();
                }
            }

            // Process from the preprocessor cursor (which will be the earliest)
            let new_document = &prev_document;
            let active_clips = &prev_active_clips;
            while let Some(event) = preprocessor_cursor.advance(new_document, active_clips) {
                match event {
                    Event::ClipStart(clip_id) => {
                        let clip = new_document.clips.get(&clip_id).unwrap();
                        let (preprocessor, desc) = StreamPreprocessor::new(&clip.descriptor);
                        preprocessors.insert(clip_id, preprocessor);

                        // Notify processors of clip start
                        for processor_state in processors
                            .values_mut()
                            .filter(|ps| ps.cursor.is_before(&preprocessor_cursor))
                        {
                            processor_state.processor.start_clip(clip_id, &desc);
                            processor_state.cursor = preprocessor_cursor.clone();
                        }
                    }
                    Event::ClipEnd(clip_id) => {
                        preprocessors.remove(&clip_id);

                        // Notify processors of clip end
                        for processor_state in processors
                            .values_mut()
                            .filter(|ps| ps.cursor.is_before(&preprocessor_cursor))
                        {
                            processor_state.processor.end_clip(clip_id);
                            processor_state.cursor = preprocessor_cursor.clone();
                        }
                    }
                    Event::Chunk(clip_id, chunk_index) => {
                        let clip = new_document.clips.get(&clip_id).unwrap();
                        let chunk = clip.chunks.get(chunk_index).unwrap();

                        // Preprocess
                        if let Some(preprocessor) = preprocessors.get_mut(&clip_id) {
                            let preprocessed_data = preprocessor.process(chunk.as_ref());

                            // Process with all processors
                            let work: Vec<_> = processors
                                .values_mut()
                                .filter(|ps| ps.cursor.is_before(&preprocessor_cursor))
                                .map(|ps| {
                                    ps.cursor = preprocessor_cursor.clone();
                                    &mut ps.processor
                                })
                                .collect();

                            work.into_par_iter().for_each(|processor| {
                                processor.process_chunk(clip_id, chunk_index, &preprocessed_data);
                            });
                        }
                    }
                }

                // Check for interrupting messages
                if let Ok(new_msg) = child_thread_receiver.try_recv() {
                    msg = new_msg;
                    // Continue outer loop to handle new message
                    continue 'interrupted;
                }
            }
            break;
        }
    }
}
