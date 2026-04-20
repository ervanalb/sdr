use nemotron_asr::{CacheConfig, LatencyMode};

use crate::id_factory::IdFactory;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};

type StreamId = usize;
type Canary = Arc<()>;
type WeakCanary = Weak<()>;

pub const SAMPLE_RATE: f64 = 16_000.; // 16 KHz sample rate for ASR
// ASR algorithm latency in seconds
const INHERENT_LATENCY: f64 = 0.560 + 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrError {
    Crashed,
}

enum WorkerRequest {
    CreateStream {
        reply: oneshot::Sender<(StreamId, Canary)>,
    },
    Transcribe {
        stream_id: StreamId,
        audio_data: Box<[i16]>,
        reply: oneshot::Sender<Option<String>>,
        _canary: Canary,
    },
}

pub struct AsrProvider {
    request_tx: Sender<WorkerRequest>,
    thread_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    chunk_samples: usize,
}

impl Clone for AsrProvider {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
            thread_handle: Arc::clone(&self.thread_handle),
            chunk_samples: self.chunk_samples,
        }
    }
}

pub struct AsrStream {
    stream_id: StreamId,
    request_tx: Sender<WorkerRequest>,
    canary: Canary,
}

impl AsrProvider {
    pub fn new() -> Result<Self, String> {
        let cache_config = CacheConfig::with_latency(LatencyMode::Low);
        let chunk_samples = cache_config.chunk_samples() as usize;

        let (request_tx, request_rx) = mpsc::channel();

        let handle = thread::Builder::new()
            .name("asr-worker".to_string())
            .spawn(move || {
                Self::worker_thread(request_rx, cache_config);
            })
            .map_err(|e| format!("Thread spawn error: {e}"))?;

        Ok(Self {
            request_tx,
            thread_handle: Arc::new(Mutex::new(Some(handle))),
            chunk_samples,
        })
    }

    pub fn chunk_samples(&self) -> usize {
        self.chunk_samples
    }

    pub fn latency(&self) -> f64 {
        INHERENT_LATENCY + self.chunk_samples() as f64 / SAMPLE_RATE
    }

    pub fn create_stream(&self) -> Result<AsrStream, AsrError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.request_tx
            .send(WorkerRequest::CreateStream { reply: reply_tx })
            .map_err(|_| AsrError::Crashed)?;

        let (stream_id, canary) = reply_rx.recv().map_err(|_| AsrError::Crashed)?;

        Ok(AsrStream {
            stream_id,
            request_tx: self.request_tx.clone(),
            canary,
        })
    }

    pub fn is_thread_alive(&self) -> bool {
        let handle_guard = self.thread_handle.lock().unwrap();
        if let Some(handle) = handle_guard.as_ref() {
            !handle.is_finished()
        } else {
            false
        }
    }

    fn worker_thread(request_rx: Receiver<WorkerRequest>, cache_config: CacheConfig) {
        nemotron_asr::load_backends_from_path(
            std::env::var("GGML_BACKEND_DIR")
                .expect("GGML_BACKEND_DIR environment variable must be set"),
        );
        let mut context = match nemotron_asr::Context::new(
            std::env::var("NEMOTRON_ASR_MODEL_PATH")
                .expect("NEMOTRON_ASR_MODEL_PATH environment variable must be set"),
            None,
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                eprintln!("Failed to initialize ASR context: {:?}", e);
                return;
            }
        };

        let mut streams: BTreeMap<StreamId, (nemotron_asr::Stream, WeakCanary)> = BTreeMap::new();
        let mut id_factory = IdFactory::default();

        while let Ok(request) = request_rx.recv() {
            // Clean up dead streams
            streams.retain(|_, (_, canary)| canary.upgrade().is_some());

            match request {
                WorkerRequest::CreateStream { reply } => {
                    let stream = context
                        .create_stream(Some(&cache_config))
                        .expect("Failed to create ASR stream");
                    let stream_id = id_factory.create();
                    let canary = Arc::new(());
                    let weak_canary = Arc::downgrade(&canary);
                    streams.insert(stream_id, (stream, weak_canary));
                    reply.send((stream_id, canary)).ok();
                }
                WorkerRequest::Transcribe {
                    stream_id,
                    audio_data,
                    reply,
                    _canary,
                } => {
                    let (stream, _) = streams
                        .get_mut(&stream_id)
                        .expect("Invalid stream ID - this is a bug");
                    let transcript = stream.process(&audio_data);
                    let result = if transcript.is_empty() {
                        None
                    } else {
                        Some(transcript)
                    };
                    reply.send(result).ok();
                }
            }
        }
    }
}

impl AsrStream {
    pub fn transcribe(&self, audio_data: Box<[i16]>) -> Result<Option<String>, AsrError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.request_tx
            .send(WorkerRequest::Transcribe {
                stream_id: self.stream_id,
                audio_data,
                reply: reply_tx,
                _canary: self.canary.clone(),
            })
            .map_err(|_| AsrError::Crashed)?;

        reply_rx.recv().map_err(|_| AsrError::Crashed)
    }
}
