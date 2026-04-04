use crate::{
    document::{ActiveDocument, Clip, ClipId},
    id_factory::IdFactory,
    preprocessor::StreamPreprocessor,
    processor::{Processor, ProcessorHistory, ProcessorParameters},
};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    sync::mpsc::{Receiver, Sender, channel},
    thread::{JoinHandle, spawn},
};

pub type ProcessorId = usize;
type ProcessorInstanceId = usize;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ClipCursor {
    Before,
    Index(isize),
    After,
}

#[derive(Clone)]
struct Cursor {
    cursors: BTreeMap<ClipId, ClipCursor>,
}

impl Cursor {
    fn start_of_document(document: &DocumentSnapshot) -> Self {
        Self {
            cursors: document
                .clips
                .keys()
                .map(|clip_id| (*clip_id, ClipCursor::Before))
                .collect(),
        }
    }

    fn try_update(mut self, document: &DocumentSnapshot) -> Option<Self> {
        let mut invalidated = false;
        // Check for removed clips
        self.cursors.retain(|clip_id, clip_cursor| {
            // Reset cursor if a started clip was deleted
            if !document.clips.contains_key(clip_id) {
                if !matches!(clip_cursor, ClipCursor::Before) {
                    invalidated = true;
                }
                // Remove the clip from the cursor
                false
            } else {
                true
            }
        });

        if invalidated {
            return None;
        }

        // Check for added clips
        let t = self.time(document);
        for (&clip_id, clip) in document.clips.iter() {
            // Reset cursor if a clip was added that starts before the cursor
            match self.cursors.entry(clip_id) {
                Entry::Vacant(e) => {
                    if let Some(t) = t
                        && clip.descriptor.start_time < t
                    {
                        return None;
                    }
                    // Otherwise, add the clip to the cursor
                    e.insert(ClipCursor::Before);
                }
                Entry::Occupied(_) => {}
            }
        }

        Some(self)
    }

    // Calculate the time of this cursor position.
    // Returns None if there are no clips in the document
    fn time(&self, document: &DocumentSnapshot) -> Option<f64> {
        self.cursors
            .iter()
            .map(|(&clip_id, &clip_cursor)| {
                let clip = document.clips.get(&clip_id).unwrap();
                Self::clip_time_at_index(clip, clip_cursor)
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
        clip.descriptor.start_time + (chunk_index - clip.chunks.start_index()) as f64 * chunk_period
    }

    fn is_before(&self, other: &Cursor) -> bool {
        assert_eq!(self.cursors.len(), other.cursors.len());

        for (clip_id, a) in self.cursors.iter() {
            let b = other.cursors.get(clip_id).unwrap();
            if a < b {
                return true;
            }
        }
        false
    }

    fn advance(&mut self, document: &DocumentSnapshot) -> Option<Event> {
        // Collect all clips with their times and sort by time
        let mut clips_by_time: Vec<(f64, ClipId)> = self
            .cursors
            .iter()
            .map(|(&clip_id, &clip_cursor)| {
                let clip = document.clips.get(&clip_id).unwrap();
                let time = Self::clip_time_at_index(clip, clip_cursor);
                (time, clip_id)
            })
            .collect();

        clips_by_time.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Try to advance each clip in order of earliest time
        for (_time, clip_id) in clips_by_time {
            let clip = document.clips.get(&clip_id).unwrap();
            let clip_cursor = self.cursors.get_mut(&clip_id).unwrap();

            let event = match *clip_cursor {
                ClipCursor::Before => {
                    // Transition from Before to Index(start_index)
                    *clip_cursor = ClipCursor::Index(clip.chunks.start_index());
                    Some(Some(Event::ClipStart(clip_id)))
                }
                ClipCursor::Index(chunk_index) => {
                    if chunk_index < clip.chunks.end_index() {
                        // We have a chunk to process
                        let event = Event::Chunk(clip_id, chunk_index);
                        // Advance to next chunk
                        *clip_cursor = ClipCursor::Index(chunk_index + 1);
                        Some(Some(event))
                    } else {
                        // We're at the end of available chunks
                        if document.active_clips.contains(&clip_id) {
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

#[derive(Clone)]
struct DocumentSnapshot {
    clips: BTreeMap<ClipId, Clip>,
    active_clips: BTreeSet<ClipId>,
}

impl DocumentSnapshot {
    fn new() -> Self {
        DocumentSnapshot {
            clips: BTreeMap::new(),
            active_clips: BTreeSet::new(),
        }
    }

    fn from_document(document: &ActiveDocument) -> Self {
        DocumentSnapshot {
            clips: document.document.clips.clone(),
            active_clips: document.active_clips.clone(),
        }
    }
}

pub struct Analysis {
    instance_id_factory: IdFactory,
    processors: BTreeMap<ProcessorId, MainThreadProcessorState>,
    processing_thread_sender: Sender<ProcessingInputMessage>,
    _processing_thread_handle: JoinHandle<()>,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl Analysis {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Analysis {
        let (main_thread_sender, child_thread_receiver) = channel::<ProcessingInputMessage>();

        let _processing_thread_handle = spawn(move || {
            processing_thread_loop(child_thread_receiver);
        });

        Analysis {
            instance_id_factory: IdFactory::default(),
            processors: BTreeMap::new(),
            processing_thread_sender: main_thread_sender,
            _processing_thread_handle,
            device: device.clone(),
            queue: queue.clone(),
        }
    }

    pub fn process(
        &mut self,
        processor_parameters: &mut BTreeMap<ProcessorId, ProcessorParameters>,
        document: &ActiveDocument,
    ) {
        // 1. Add & remove processor states
        let mut removed_processors = vec![];
        self.processors.retain(|&processor_id, processor_state| {
            let keep = processor_parameters.contains_key(&processor_id);
            if !keep {
                removed_processors.push(processor_state.instance_id);
            }
            keep
        });

        let mut new_processors = vec![];
        for (processor_id, processor_parameters) in processor_parameters.iter() {
            if !self.processors.contains_key(processor_id) {
                let (processor, history) =
                    processor_parameters.create_processor(&self.device, &self.queue);
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

        // Check for parameter changes
        for (processor_id, processor_state) in self.processors.iter_mut() {
            let parameters = processor_parameters.get_mut(processor_id).unwrap();
            if parameters != &processor_state.last_parameters {
                removed_processors.push(processor_state.instance_id);
                let instance_id = self.instance_id_factory.create();
                let (processor, history) = parameters.create_processor(&self.device, &self.queue);
                *processor_state = MainThreadProcessorState {
                    instance_id,
                    last_parameters: parameters.clone(),
                    history,
                };
                new_processors.push((instance_id, processor));
            }
        }

        // 2. Update processor histories
        for processor_state in self.processors.values_mut() {
            processor_state.history.update();
        }

        // 3. Send document snapshot to processing thread
        let msg = ProcessingInputMessage {
            removed_processors,
            new_processors,
            document: DocumentSnapshot::from_document(document),
        };
        self.processing_thread_sender.send(msg).unwrap();
    }

    pub fn expire(&mut self, retain_time: f64) {
        // Note: ProcessorHistory::expire still takes DateTime<Utc>, so we skip calling it for now
        // TODO: Update ProcessorHistory::expire to take f64
        let _ = retain_time;
    }

    pub fn draw(
        &self,
        ui: &mut egui::Ui,
        figure_rect: egui::Rect,
        viewport: &crate::ui::Viewport,
        dt: f64,
    ) {
        for (processor_id, processor) in self.processors.iter() {
            processor
                .history
                .draw(ui, egui::Id::new(processor_id), figure_rect, viewport, dt);
        }
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

struct ProcessingInputMessage {
    removed_processors: Vec<ProcessorInstanceId>,
    new_processors: Vec<(ProcessorInstanceId, Box<dyn Processor>)>,
    document: DocumentSnapshot,
}

#[derive(Debug)]
enum Event {
    ClipStart(ClipId),
    ClipEnd(ClipId),
    Chunk(ClipId, isize), // ClipId and chunk index
}

fn processing_thread_loop(child_thread_receiver: Receiver<ProcessingInputMessage>) {
    let mut document = DocumentSnapshot::new();
    let mut preprocessors: BTreeMap<ClipId, StreamPreprocessor> = BTreeMap::new();
    let mut preprocessor_cursor = Cursor::start_of_document(&document);
    let mut processors = BTreeMap::<ProcessorInstanceId, ChildThreadProcessorState>::new();

    while let Ok(mut msg) = child_thread_receiver.recv() {
        'interrupted: loop {
            // 1. Read new document
            document = msg.document;

            // 2. Add and remove processors
            for instance_id in msg.removed_processors {
                processors.remove(&instance_id);
            }
            for (instance_id, processor) in msg.new_processors {
                processors.insert(
                    instance_id,
                    ChildThreadProcessorState {
                        processor,
                        cursor: Cursor::start_of_document(&document),
                    },
                );
            }

            // 3. Reset processors and preprocessor if invalidated
            for processor_state in processors.values_mut() {
                processor_state.cursor = processor_state
                    .cursor
                    .clone()
                    .try_update(&document)
                    .unwrap_or_else(|| {
                        processor_state.processor.reset();
                        Cursor::start_of_document(&document)
                    });
            }

            // Reset the preprocessor if try_update fails
            // or if any processor has a cursor that falls before the preprocessor cursor
            preprocessor_cursor = preprocessor_cursor
                .clone()
                .try_update(&document)
                .filter(|preprocessor_cursor| {
                    processors
                        .values_mut()
                        .all(|ps| !ps.cursor.is_before(preprocessor_cursor))
                })
                .unwrap_or_else(|| {
                    preprocessors.clear();
                    Cursor::start_of_document(&document)
                });

            // 4. Process from the preprocessor cursor (which will be the earliest)
            while let Some(event) = preprocessor_cursor.advance(&document) {
                match event {
                    Event::ClipStart(clip_id) => {
                        let clip = document.clips.get(&clip_id).unwrap();
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
                        let clip = document.clips.get(&clip_id).unwrap();
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
                                processor.process_chunk(clip_id, &preprocessed_data);
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
