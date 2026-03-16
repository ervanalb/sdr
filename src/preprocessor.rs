use std::sync::Arc;

use chrono::{DateTime, Utc};
use num_complex::Complex;

use crate::hardware::{ReceiveStreamChunk, ReceiveStreamDescriptor, StreamId};

pub struct Preprocessor {}

impl Preprocessor {
    pub fn new() -> Preprocessor {
        todo!()
    }
    pub fn reset(&mut self) {
        todo!()
    }
    pub fn process(&mut self, chunk: &ReceiveStreamChunk) -> PreprocessedChunk {
        todo!()
    }
}

#[derive(Debug)]
pub struct PreprocessedChunk {
    pub stream_id: StreamId,
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    pub time: DateTime<Utc>,
    pub data: Box<Complex<f32>>,
}
