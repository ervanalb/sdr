use crate::id_factory::IdFactory;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

type StreamId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrError {
    Crashed,
}

enum WorkerRequest {
    CreateStream {
        reply: oneshot::Sender<StreamId>,
    },
    Transcribe {
        stream_id: StreamId,
        audio_data: Box<[i16]>,
        reply: oneshot::Sender<Option<String>>,
    },
}

pub struct AsrProvider {
    request_tx: Sender<WorkerRequest>,
    thread_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Clone for AsrProvider {
    fn clone(&self) -> Self {
        Self {
            request_tx: self.request_tx.clone(),
            thread_handle: Arc::clone(&self.thread_handle),
        }
    }
}

pub struct AsrStream {
    stream_id: StreamId,
    request_tx: Sender<WorkerRequest>,
}

impl AsrProvider {
    pub fn new() -> Result<Self, String> {
        let (request_tx, request_rx) = mpsc::channel();

        let handle = thread::Builder::new()
            .name("asr-worker".to_string())
            .spawn(move || {
                Self::worker_thread(request_rx);
            })
            .map_err(|e| format!("Thread spawn error: {e}"))?;

        Ok(Self {
            request_tx,
            thread_handle: Arc::new(Mutex::new(Some(handle))),
        })
    }

    pub fn create_stream(&self) -> Result<AsrStream, AsrError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.request_tx
            .send(WorkerRequest::CreateStream { reply: reply_tx })
            .map_err(|_| AsrError::Crashed)?;

        let stream_id = reply_rx.recv().map_err(|_| AsrError::Crashed)?;

        Ok(AsrStream {
            stream_id,
            request_tx: self.request_tx.clone(),
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

    fn worker_thread(request_rx: Receiver<WorkerRequest>) {
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

        let mut streams: BTreeMap<StreamId, nemotron_asr::Stream> = BTreeMap::new();
        let mut id_factory = IdFactory::default();

        while let Ok(request) = request_rx.recv() {
            match request {
                WorkerRequest::CreateStream { reply } => {
                    let stream = context
                        .create_stream(None)
                        .expect("Failed to create ASR stream");
                    let stream_id = id_factory.create();
                    streams.insert(stream_id, stream);
                    reply.send(stream_id).ok();
                }
                WorkerRequest::Transcribe {
                    stream_id,
                    audio_data,
                    reply,
                } => {
                    let stream = streams
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
            })
            .map_err(|_| AsrError::Crashed)?;

        reply_rx.recv().map_err(|_| AsrError::Crashed)
    }
}
