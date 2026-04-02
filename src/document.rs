use crate::{
    hardware::{HardwareResult, RawIqSamples, StreamId},
    id_factory::IdFactory,
    seqdeque::SeqDeque,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    rc::{Rc, Weak},
    sync::Arc,
};

pub type ClipId = usize;
pub type RecordingId = usize;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipDescriptor {
    pub name: String,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Clip {
    pub descriptor: ClipDescriptor,
    pub chunks: SeqDeque<Chunk>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Chunk {
    #[serde(with = "serde_arc")]
    pub data: Arc<RawIqSamples>,
}

mod serde_arc {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(value: &Arc<RawIqSamples>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        (**value).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<RawIqSamples>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Arc::new(RawIqSamples::deserialize(deserializer)?))
    }
}

/// The serializable document containing clips
#[derive(Debug, Serialize, Deserialize)]
pub struct Document {
    pub clips: BTreeMap<ClipId, Clip>,
}

/// Magic number for SDR file format: "SDR\0" + version byte
/// Version 1: 0x53 0x44 0x52 0x00 0x01
const MAGIC_NUMBER: [u8; 5] = [0x53, 0x44, 0x52, 0x00, 0x01];

/// Wrapper for serialization that includes magic number and version
#[derive(Serialize, Deserialize)]
struct SavedDocument {
    magic: [u8; 5],
    document: Document,
}

impl SavedDocument {
    fn new(document: Document) -> Self {
        SavedDocument {
            magic: MAGIC_NUMBER,
            document,
        }
    }

    fn validate_magic(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.magic != MAGIC_NUMBER {
            return Err(format!(
                "Invalid file format. Expected magic number {:?}, got {:?}",
                MAGIC_NUMBER, self.magic
            )
            .into());
        }
        Ok(())
    }
}

/// The active document with runtime state
#[derive(Debug)]
pub struct ActiveDocument {
    pub document: Document,
    pub active_clips: BTreeSet<ClipId>,
    clip_id_factory: IdFactory,
    recording_id_factory: IdFactory,
    recordings: BTreeMap<RecordingId, RecordingInfo>,
    clip_name_counter: usize,
}

#[derive(Debug)]
struct RecordingInfo {
    canary: Weak<RecordingId>,
    wall_clock_start_time: DateTime<Utc>,
    document_start_time: f64,
    stream_to_clip: BTreeMap<StreamId, ClipId>,
}

impl ActiveDocument {
    pub fn new() -> ActiveDocument {
        ActiveDocument {
            document: Document {
                clips: BTreeMap::new(),
            },
            clip_id_factory: IdFactory::default(),
            recording_id_factory: IdFactory::default(),
            recordings: BTreeMap::new(),
            active_clips: BTreeSet::new(),
            clip_name_counter: 1,
        }
    }

    /// Reconstruct the runtime state from a loaded document
    fn from_document(document: Document) -> ActiveDocument {
        // Find the highest clip ID and set the factory to one higher
        let max_clip_id = document.clips.keys().max().copied().unwrap_or(0);
        let mut clip_id_factory = IdFactory::default();
        for _ in 0..=max_clip_id {
            clip_id_factory.create();
        }

        // Find the highest clip number from clip names and set counter
        let max_clip_number = document
            .clips
            .values()
            .filter_map(|clip| {
                clip.descriptor
                    .name
                    .strip_prefix("Clip ")
                    .and_then(|s| s.parse::<usize>().ok())
            })
            .max()
            .unwrap_or(0);
        let clip_name_counter = max_clip_number + 1;

        ActiveDocument {
            document,
            active_clips: BTreeSet::new(),
            clip_id_factory,
            recording_id_factory: IdFactory::default(),
            recordings: BTreeMap::new(),
            clip_name_counter,
        }
    }

    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let saved = SavedDocument::new(Document {
            clips: self.document.clips.clone(),
        });
        let file = std::fs::File::create(path)?;
        bincode::serialize_into(file, &saved)?;
        Ok(())
    }

    pub fn load_from_file(path: &std::path::Path) -> Result<ActiveDocument, Box<dyn std::error::Error>> {
        let file = std::fs::File::open(path)?;
        let saved: SavedDocument = bincode::deserialize_from(file)?;
        saved.validate_magic()?;
        Ok(ActiveDocument::from_document(saved.document))
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
                    let clip_name = format!("Clip {}", self.clip_name_counter);
                    self.clip_name_counter += 1;
                    self.document.clips.insert(
                        clip_id,
                        Clip {
                            descriptor: ClipDescriptor {
                                name: clip_name,
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

            let clip = self.document.clips.get_mut(&clip_id).unwrap();
            clip.chunks.push_back(Chunk {
                data: Arc::new(chunk.chunk),
            });
        }
    }

    pub fn expire(&mut self, retain_time: f64) {
        // Remove chunks that are older than retain_time
        self.document.clips.retain(|clip_id, clip| {
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

    pub fn delete_selection(&mut self, selection: &mut BTreeSet<ClipId>) {
        // Remove clips that are not active
        selection.retain(|clip_id| {
            if self.active_clips.contains(clip_id) {
                // Skip active clips, keep them in selection
                true
            } else {
                // Delete non-active clips
                self.document.clips.remove(clip_id);
                false
            }
        });
    }
}
