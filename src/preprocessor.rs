use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, Utc};
use num_complex::Complex;

use crate::hardware::{ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId};

pub struct Preprocessor {
    streams: BTreeMap<StreamId, StreamPreprocessor>,
}

impl Preprocessor {
    pub fn new() -> Preprocessor {
        Preprocessor {
            streams: BTreeMap::new(),
        }
    }

    pub fn reset(&mut self) {
        self.streams.clear();
    }

    pub fn start_stream(
        &mut self,
        stream_id: usize,
        descriptor: &ReceiveStreamDescriptor,
    ) -> PreprocessedStreamDescriptor {
        todo!()
    }

    pub fn process_chunk(&mut self, chunk: &ReceiveStreamChunk) -> PreprocessedChunk {
        todo!()
    }

    pub fn end_stream(&mut self, stream_id: usize) {
        todo!()
    }
}

pub struct PreprocessedStreamDescriptor {
    descriptor: Arc<ReceiveStreamDescriptor>,
}

pub struct StreamPreprocessor {
    descriptor: Arc<ReceiveStreamDescriptor>,
}

#[derive(Debug)]
pub struct PreprocessedChunk {
    pub stream_id: StreamId,
    pub time: DateTime<Utc>,
    pub data: Box<Complex<f32>>,
}
