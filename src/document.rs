use crate::{
    hardware::{HardwareResult, ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId},
    seqdeque::SeqDeque,
};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    ops::Range,
    sync::Arc,
};

pub use crate::analysis::ProcessorId;

pub struct Document {
    streams: BTreeMap<StreamId, StreamHistory>,
    chunks: SeqDeque<Arc<ReceiveStreamChunk>>,
}

impl Document {
    pub fn new() -> Document {
        Document {
            chunks: SeqDeque::new(),
            streams: BTreeMap::new(),
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
                    // Add this new stream to the document
                    e.insert(StreamHistory {
                        descriptor: descriptor.clone(),
                        span: start..end,
                    });
                }
            }
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
    }

    pub fn chunks_start_index(&self) -> usize {
        self.chunks.start_index()
    }

    pub fn chunks_end_index(&self) -> usize {
        self.chunks.end_index()
    }

    pub fn chunks_range(&self, range: impl std::ops::RangeBounds<usize>) -> impl Iterator<Item = &Arc<ReceiveStreamChunk>> {
        self.chunks.range(range)
    }

    pub fn streams(&self) -> &BTreeMap<StreamId, StreamHistory> {
        &self.streams
    }

}

#[derive(Clone, Debug)]
pub struct StreamHistory {
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    pub span: Range<usize>,
}

