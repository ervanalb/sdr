use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

const SAMPLE_RATE: f64 = 48000.;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioError {
    Overrun,
    Underrun,
}

#[derive(Debug, Clone, Copy)]
pub struct FeedResult {
    pub last_played_seq_num: Option<usize>,
    pub underrun: bool,
}

pub struct AudioBuffer {
    pub seq_num: usize,
    pub data: Vec<f32>,
}

impl AudioBuffer {
    pub fn new(len: usize) -> AudioBuffer {
        AudioBuffer {
            seq_num: 0, // Will get overwritten
            data: Vec::with_capacity(len),
        }
    }
}

enum AudioResponse {
    Played(usize),
    Underrun,
}

pub struct AudioOutput {
    to_audio_thread: Sender<AudioBuffer>,
    from_audio_thread: Receiver<AudioResponse>,
    last_played_seq_num: Option<usize>,
    _stream: cpal::Stream, // Keep stream alive
}

impl AudioOutput {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Setup cpal
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No output device available")?;

        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: SAMPLE_RATE as u32,
            buffer_size: cpal::BufferSize::Default,
        };

        // Create channels with capacity for all buffers
        let (to_audio_tx, to_audio_rx) = mpsc::channel::<AudioBuffer>();
        // (plus an underrun message)
        let (from_audio_tx, from_audio_rx) = mpsc::channel::<AudioResponse>();

        // Spawn audio thread
        let stream = {
            // Setting this to true will avoid emitting underruns
            // until the first real audio data arrives
            let mut underrun = true;
            let mut in_data = vec![];
            let data_callback = move |mut out_data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                while !out_data.is_empty() {
                    // See if we need to read more input data
                    if in_data.is_empty() {
                        in_data = match to_audio_rx.try_recv() {
                            Ok(buffer) => {
                                from_audio_tx
                                    .send(AudioResponse::Played(buffer.seq_num))
                                    .unwrap();
                                underrun = false;
                                buffer.data
                            }
                            Err(TryRecvError::Empty) => {
                                // No buffer available - underrun
                                // Fill rest with zeros and report it
                                for sample in out_data {
                                    *sample = 0.;
                                }
                                if !underrun {
                                    from_audio_tx.send(AudioResponse::Underrun).unwrap();
                                    underrun = true;
                                }
                                return;
                            }
                            Err(TryRecvError::Disconnected) => {
                                // Channel closed
                                panic!("Channel closed");
                            }
                        }
                    }

                    let len = out_data.len().min(in_data.len());
                    for (out_sample, in_sample) in
                        out_data[..len].iter_mut().zip(in_data.drain(..len))
                    {
                        *out_sample = in_sample;
                    }
                    out_data = &mut out_data[len..];
                }
            };

            let stream = device.build_output_stream(
                &config,
                data_callback,
                // TODO: from_audio_tx.try_send(AudioResponse::Error)
                |err| eprintln!("Audio stream error: {}", err),
                None,
            )?;

            stream.play()?;
            stream
        };

        Ok(Self {
            to_audio_thread: to_audio_tx,
            from_audio_thread: from_audio_rx,
            last_played_seq_num: None,
            _stream: stream,
        })
    }

    pub fn feed(
        &mut self,
        bufs: impl Iterator<Item = AudioBuffer>,
    ) -> Result<FeedResult, Box<dyn std::error::Error>> {
        let mut underrun = false;

        // Receive from the audio thread
        loop {
            match self.from_audio_thread.try_recv() {
                Ok(AudioResponse::Played(seq_num)) => {
                    // Update last played index from the buffer we just got back
                    self.last_played_seq_num = Some(seq_num);
                }
                Ok(AudioResponse::Underrun) => {
                    underrun = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err("Audio thread disconnected".into());
                }
            }
        }

        for buf in bufs {
            self.to_audio_thread.send(buf).unwrap();
        }

        Ok(FeedResult {
            last_played_seq_num: self.last_played_seq_num,
            underrun,
        })
    }
}
