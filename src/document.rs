use crate::{
    chunked_deque::ChunkedDeque,
    hardware::{HardwareResult, RawIqSamples, StreamId},
    id_factory::IdFactory,
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
    pub reference_time: f64,
    pub chunk_size: usize,
}

impl ClipDescriptor {
    pub fn time(&self, index: f64) -> f64 {
        self.reference_time + index * self.chunk_size as f64 / self.sample_rate
    }

    pub fn index(&self, time: f64) -> f64 {
        (time - self.reference_time) * self.sample_rate / self.chunk_size as f64
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
    pub chunks: ChunkedDeque<Chunk>,
}

#[derive(Clone, Debug)]
pub struct Chunk(Arc<RawIqSamples>);

impl Chunk {
    pub fn new(value: RawIqSamples) -> Self {
        Chunk(Arc::new(value))
    }

    pub fn as_ref(&self) -> &RawIqSamples {
        &self.0
    }
}

impl PartialEq for Chunk {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl serde::Serialize for Chunk {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.as_ref().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Chunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Chunk::new(RawIqSamples::deserialize(deserializer)?))
    }
}

/// The serializable document containing clips
// TODO: Remove Clone and pass around Arc<Document>
#[derive(Clone, Debug, Default)]
pub struct Document {
    pub clips: BTreeMap<ClipId, Arc<Clip>>,
}

impl serde::Serialize for Document {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Document", 1)?;
        // Serialize the BTreeMap<ClipId, Arc<Clip>> as BTreeMap<ClipId, Clip>
        let clips: BTreeMap<ClipId, &Clip> = self
            .clips
            .iter()
            .map(|(id, clip)| (*id, clip.as_ref()))
            .collect();
        state.serialize_field("clips", &clips)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for Document {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DocumentHelper {
            clips: BTreeMap<ClipId, Clip>,
        }

        let helper = DocumentHelper::deserialize(deserializer)?;
        Ok(Document {
            clips: helper
                .clips
                .into_iter()
                .map(|(id, clip)| (id, Arc::new(clip)))
                .collect(),
        })
    }
}

impl Document {
    pub fn new() -> Self {
        Document {
            clips: BTreeMap::new(),
        }
    }

    /// Find the earliest time in the document (earliest clip start)
    pub fn earliest_time(&self) -> Option<f64> {
        self.clips.values()
            .map(|clip| clip.descriptor.time(clip.chunks.start_index() as f64))
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Find the latest time in the document (latest clip end)
    pub fn latest_time(&self) -> Option<f64> {
        self.clips.values()
            .map(|clip| clip.descriptor.time(clip.chunks.end_index() as f64))
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    pub fn removed_clips<'a>(
        &'a self,
        new: &'a Document,
    ) -> impl Iterator<Item = (ClipId, &'a Arc<Clip>)> {
        self.clips.iter().filter_map(|(clip_id, clip)| {
            (!new.clips.contains_key(clip_id)).then_some((*clip_id, clip))
        })
    }

    pub fn added_clips<'a>(
        &'a self,
        new: &'a Document,
    ) -> impl Iterator<Item = (ClipId, &'a Arc<Clip>)> {
        new.clips.iter().filter_map(|(clip_id, clip)| {
            (!self.clips.contains_key(clip_id)).then_some((*clip_id, clip))
        })
    }

    pub fn modified_clips<'a>(
        &'a self,
        new: &'a Document,
    ) -> impl Iterator<Item = (ClipId, &'a Arc<Clip>, &'a Arc<Clip>)> {
        self.clips.iter().filter_map(|(clip_id, old_clip)| {
            new.clips.get(clip_id).and_then(|new_clip| {
                (!Arc::ptr_eq(old_clip, new_clip)).then_some((*clip_id, old_clip, new_clip))
            })
        })
    }
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

    pub fn load_from_file(
        path: &std::path::Path,
    ) -> Result<ActiveDocument, Box<dyn std::error::Error>> {
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
        let recording = self
            .recordings
            .get_mut(&**recording_id)
            .expect("Recording not found");

        // Remove streams from stream_to_clip mapping that are no longer active
        recording.stream_to_clip.retain(|stream_id, _| {
            result
                .active_streams
                .iter()
                .any(|(active_stream_id, _)| active_stream_id == stream_id)
        });

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
                        Arc::new(Clip {
                            descriptor: ClipDescriptor {
                                name: clip_name,
                                frequency: descriptor.frequency,
                                sample_rate: descriptor.sample_rate,
                                reference_time: clip_start_time,
                                chunk_size: descriptor.chunk_size,
                            },
                            chunks: ChunkedDeque::new(),
                        }),
                    );
                    clip_id
                });

            let clip = self.document.clips.get_mut(&clip_id).unwrap();
            Arc::make_mut(clip)
                .chunks
                .push_back(Chunk::new(chunk.chunk));
        }
    }

    pub fn expire(&mut self, retain_time: f64) {
        // Remove chunks that are older than retain_time
        self.document.clips.retain(|clip_id, clip| {
            // Calculate elapsed time from clip start to retain_time
            if retain_time > clip.descriptor.reference_time {
                // Convert retain_time to retain_index
                // (the index into clip.chunks where retain_time sits)
                // by shifting & scaling by the chunk rate
                let retain_index = clip.descriptor.index(retain_time);
                let retain_index = retain_index.clamp(
                    clip.chunks.start_index() as f64,
                    clip.chunks.end_index() as f64,
                );
                let retain_index = retain_index as isize;
                Arc::make_mut(clip).chunks.remove_front(retain_index);

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
