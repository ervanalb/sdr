use crate::{
    hardware::{HardwareResult, RawIqSamples, StreamId},
    id_factory::IdFactory,
    seqdeque::SeqDeque,
};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
    sync::Arc,
};

pub type ClipId = usize;
pub type RecordingId = usize;

#[derive(Clone, Debug)]
pub struct ClipDescriptor {
    pub frequency: f64,
    pub sample_rate: f64,
    pub start_time: f64,
    pub chunk_size: usize,
}

impl ClipDescriptor {
    pub fn time(&self, index: f64) -> f64 {
        self.start_time + index * self.chunk_size as f64 / self.sample_rate
    }

    pub fn index(&self, time: f64) -> f64 {
        (time - self.start_time) * self.sample_rate / self.chunk_size as f64
    }

    pub fn freq_min(&self) -> f64 {
        self.frequency - 0.5 * self.sample_rate
    }

    pub fn freq_max(&self) -> f64 {
        self.frequency + 0.5 * self.sample_rate
    }
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub descriptor: ClipDescriptor,
    pub chunks: SeqDeque<Chunk>,
}

#[derive(Clone)]
pub struct Chunk {
    pub data: Arc<RawIqSamples>,
}

#[derive(Debug)]
pub struct Document {
    pub clips: BTreeMap<ClipId, Clip>,
    pub active_clips: BTreeSet<ClipId>,
    clip_id_factory: IdFactory,
    recording_id_factory: IdFactory,
    recordings: BTreeMap<RecordingId, RecordingInfo>,
}

#[derive(Debug)]
struct RecordingInfo {
    canary: Weak<RecordingId>,
    wall_clock_start_time: DateTime<Utc>,
    document_start_time: f64,
    stream_to_clip: BTreeMap<StreamId, ClipId>,
}

impl Document {
    pub fn new() -> Document {
        Document {
            clip_id_factory: IdFactory::default(),
            recording_id_factory: IdFactory::default(),
            clips: BTreeMap::new(),
            recordings: BTreeMap::new(),
            active_clips: BTreeSet::new(),
        }
    }

    pub fn update(&mut self) {
        self.active_clips.clear();

        // Remove dead recordings and collect active clip IDs
        self.recordings.retain(|_, recording_info| {
            if recording_info.canary.upgrade().is_some() {
                // Recording is still alive--
                // mark its clips as active
                for &clip_id in recording_info.stream_to_clip.values() {
                    self.active_clips.insert(clip_id);
                }
                true
            } else {
                // Recording has been dropped
                false
            }
        });
    }

    pub fn record(
        &mut self,
        wall_clock_start_time: DateTime<Utc>,
        document_start_time: f64,
    ) -> Rc<RecordingId> {
        let recording_id = self.recording_id_factory.create();
        let recording_id_rc = Rc::new(recording_id);

        self.recordings.insert(
            recording_id,
            RecordingInfo {
                canary: Rc::downgrade(&recording_id_rc),
                wall_clock_start_time,
                document_start_time,
                stream_to_clip: BTreeMap::new(),
            },
        );

        recording_id_rc
    }

    pub fn update_recording(&mut self, recording_id: &Rc<RecordingId>, result: HardwareResult) {
        if result.chunks.is_empty() {
            return;
        }

        let recording = self
            .recordings
            .get_mut(&**recording_id)
            .expect("Recording not found");

        // Group chunks by stream_id
        for chunk in result.chunks {
            // Get or create clip for this stream
            let clip_id = *recording
                .stream_to_clip
                .entry(chunk.stream_id)
                .or_insert_with(|| {
                    // Find the descriptor for this stream_id
                    let descriptor = result
                        .active_streams
                        .iter()
                        .find(|(id, _)| *id == chunk.stream_id)
                        .expect("Chunk received for stream without descriptor")
                        .1
                        .as_ref();

                    // Calculate clip start time: document start + elapsed wall-clock time
                    let elapsed_seconds = chunk
                        .time
                        .signed_duration_since(recording.wall_clock_start_time)
                        .as_seconds_f64();
                    let clip_start_time = recording.document_start_time + elapsed_seconds;

                    let clip_id = self.clip_id_factory.create();
                    self.clips.insert(
                        clip_id,
                        Clip {
                            descriptor: ClipDescriptor {
                                frequency: descriptor.frequency,
                                sample_rate: descriptor.sample_rate,
                                start_time: clip_start_time,
                                chunk_size: descriptor.chunk_size,
                            },
                            chunks: SeqDeque::new(),
                        },
                    );
                    clip_id
                });

            let clip = self.clips.get_mut(&clip_id).unwrap();
            clip.chunks.push_back(Chunk {
                data: Arc::new(chunk.chunk),
            });
        }
    }

    pub fn expire(&mut self, retain_time: f64) {
        // Remove chunks that are older than retain_time
        self.clips.retain(|clip_id, clip| {
            // Calculate elapsed time from clip start to retain_time
            if retain_time > clip.descriptor.start_time {
                // Convert retain_time to retain_index
                // (the index into clip.chunks where retain_time sits)
                // by shifting & scaling by the chunk rate
                let retain_index = clip.descriptor.index(retain_time);
                let retain_index = retain_index.clamp(
                    clip.chunks.start_index() as f64,
                    clip.chunks.end_index() as f64,
                );
                let retain_index = retain_index as usize;
                clip.chunks.remove_front(retain_index);

                // Retain empty clips that are part of an active recording
                !clip.chunks.is_empty() || self.active_clips.contains(clip_id)
            } else {
                true
            }
        });
    }
}
