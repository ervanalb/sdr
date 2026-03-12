use super::*;
use crate::{
    dsp::{FmDemod, Ifft, OverlapReduce},
    id_factory::IdFactory,
    processor::ChannelDescriptor,
};
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

type TransmissionId = usize;

#[derive(Clone, Debug, Serialize, Deserialize)]
// #[serde(rename = "fm")] -- Doesn't work?
pub struct FmModulationParameters {
    squelch_db: f64,
    squelch_hysteresis_db: f64,
}

#[typetag::serde]
impl ModulationParameters for FmModulationParameters {
    fn create_demodulator(&self, ifft_size: usize) -> Box<dyn Demodulator> {
        Box::new(FmDemodulator {
            inv_fft_len: 1. / ifft_size as f32,
            squelch_low: 10_f64.powf(0.1 * (self.squelch_db - self.squelch_hysteresis_db)) as f32,
            squelch_high: 10_f64.powf(0.1 * self.squelch_db) as f32,
            ifft: Ifft::new(ifft_size),
            overlap_reduce: OverlapReduce::new(ifft_size / 2),
            fm_demod: FmDemod::new(0.), // TODO compute omega from tuning error
            transmission_id_factory: IdFactory::default(),
            active_transmission: None,
        })
    }

    fn create_history(&self) -> Box<dyn ModulationHistory> {
        Box::new(FmHistory::new())
    }
}

pub struct FmDemodulator {
    inv_fft_len: f32,
    squelch_low: f32,
    squelch_high: f32,
    ifft: Ifft,
    overlap_reduce: OverlapReduce<Complex<f32>>,
    fm_demod: FmDemod,
    transmission_id_factory: IdFactory,
    active_transmission: Option<TransmissionId>,
}

impl Demodulator for FmDemodulator {
    fn process(
        &mut self,
        time: Instant,
        mut fft_data: Vec<Complex<f32>>,
        noise_floor: f32,
    ) -> Option<Box<dyn Any + Send>> {
        // Measure mean energy in the band
        let energy = fft_data
            .iter()
            .map(|sample| sample.re * sample.re + sample.im * sample.im)
            .sum::<f32>()
            * self.inv_fft_len;

        // TODO: LPF energy?

        // Perform squelch, and only continue execution if there is an active transmission
        let active_transmission = match &mut self.active_transmission {
            Some(transmission_id) => {
                if energy < noise_floor * self.squelch_low {
                    self.active_transmission = None;
                    return None;
                } else {
                    transmission_id
                }
            }
            None => {
                if energy > noise_floor * self.squelch_high {
                    let transmission_id = self.transmission_id_factory.create();
                    self.active_transmission.insert(transmission_id)
                } else {
                    return None;
                }
            }
        };

        // Recover IQ samples @ baseband by doing IFFT + overlap
        self.ifft.process_inplace(&mut fft_data);
        let iq_data = self.overlap_reduce.process(&fft_data);

        // Demodulate FM by finding angle of each IQ sample
        let audio_data = self.fm_demod.process(&iq_data);

        Some(Box::new(FmDemodulation {
            transmission_id: *active_transmission,
            time,
            iq_data,
            audio_data,
        }))
    }
}

#[derive(Debug)]
pub struct FmDemodulation {
    pub transmission_id: TransmissionId,
    pub time: Instant,
    pub iq_data: Vec<Complex<f32>>,
    pub audio_data: Vec<f32>,
}

pub struct FmHistory {
    transmissions: BTreeMap<TransmissionId, FmTransmission>,
}

pub struct FmTransmission {
    chunks: VecDeque<FmTransmissionChunk>,
}

pub struct FmTransmissionChunk {
    pub time: Instant,
    pub iq_data: Vec<Complex<f32>>,
    pub audio_data: Vec<f32>,
}

impl FmHistory {
    pub fn new() -> Self {
        FmHistory {
            transmissions: BTreeMap::new(),
        }
    }
}

impl ModulationHistory for FmHistory {
    fn add(&mut self, demodulation: Box<dyn Any + Send>) {
        let FmDemodulation {
            transmission_id,
            time,
            iq_data,
            audio_data,
        } = *demodulation.downcast::<FmDemodulation>().unwrap();

        let active_transmission = self
            .transmissions
            .entry(transmission_id)
            .or_insert_with(|| FmTransmission {
                chunks: VecDeque::new(),
            });
        active_transmission.chunks.push_back(FmTransmissionChunk {
            time,
            iq_data,
            audio_data,
        });
    }

    fn prune_old_data(&mut self, retain_time: Instant) -> bool {
        self.transmissions
            .retain(|_, transmission| transmission.prune_old_data(retain_time));
        !self.transmissions.is_empty()
    }

    fn draw_list<'a>(
        &'a self,
        stream_id: StreamId,
        channel_id: ChannelId,
        descriptor: &'a Arc<ChannelDescriptor>,
    ) -> Box<dyn Iterator<Item = (Instant, Instant, ModulationUiFn<'a>)> + 'a> {
        let iter = self
            .transmissions
            .iter()
            .map(move |(transmission_id, transmission)| {
                let ui: ModulationUiFn = Box::new(move |response| {
                    egui::Popup::context_menu(response)
                        .id(egui::Id::new((stream_id, channel_id, transmission_id)))
                        .show(|ui| {
                            if ui.button("Export audio...").clicked() {
                                ui.close();

                                // Sanitize the channel name for use as a filename
                                let default_name = format!(
                                    "{}_{}sps.raw",
                                    descriptor.name,
                                    descriptor.sample_rate.round()
                                )
                                .replace(" ", "_")
                                .replace("/", "_");

                                if let Some(path) = rfd::FileDialog::new()
                                    .set_file_name(&default_name)
                                    .add_filter("Raw (f32 samples)", &["raw"])
                                    .save_file()
                                {
                                    if let Err(e) = transmission.export_audio_data(&path) {
                                        eprintln!("Failed to export audio data: {}", e);
                                    }
                                }
                            }
                            if ui.button("Export IQ data...").clicked() {
                                ui.close();

                                // Sanitize the channel name for use as a filename
                                let default_name = format!(
                                    "{}_{}sps.raw",
                                    descriptor.name,
                                    descriptor.sample_rate.round()
                                )
                                .replace(" ", "_")
                                .replace("/", "_");

                                if let Some(path) = rfd::FileDialog::new()
                                    .set_file_name(&default_name)
                                    .add_filter("Raw (complex f32 samples)", &["raw"])
                                    .save_file()
                                {
                                    if let Err(e) = transmission.export_iq_data(&path) {
                                        eprintln!("Failed to export IQ data: {}", e);
                                    }
                                }
                            }
                        });
                });
                let (start_time, end_time) = transmission.time_range();
                (start_time, end_time, ui)
            });
        Box::new(iter)
    }
}

impl FmTransmission {
    fn time_range(&self) -> (Instant, Instant) {
        (
            self.chunks.front().unwrap().time,
            self.chunks.back().unwrap().time,
        )
    }

    fn export_iq_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;

        let mut file = std::fs::File::create(path)?;

        for chunk in &self.chunks {
            for sample in &chunk.iq_data {
                file.write_all(&sample.re.to_le_bytes())?;
                file.write_all(&sample.im.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }

    fn export_audio_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;

        let mut file = std::fs::File::create(path)?;

        for chunk in &self.chunks {
            for sample in &chunk.audio_data {
                file.write_all(&sample.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }

    fn prune_old_data(&mut self, retain_time: Instant) -> bool {
        let first_index = self
            .chunks
            .partition_point(|chunk| chunk.time <= retain_time);
        self.chunks.drain(..first_index);
        !self.chunks.is_empty()
    }
}
