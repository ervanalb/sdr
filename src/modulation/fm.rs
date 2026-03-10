use super::*;
use crate::{
    dsp::{Ifft, OverlapReduce},
    processor::ChannelDescriptor,
};
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use std::{any::Any, sync::Arc};

#[derive(Clone, Debug, Serialize, Deserialize)]
// #[serde(rename = "fm")] -- Doesn't work?
pub struct FmModulationParameters {}

#[typetag::serde]
impl ModulationParameters for FmModulationParameters {
    fn create_demodulator(&self, ifft_size: usize) -> Box<dyn Demodulator> {
        Box::new(FmDemodulator {
            ifft: Ifft::new(ifft_size),
            overlap_reduce: OverlapReduce::new(ifft_size / 2),
        })
    }

    fn create_history(&self, start_time: Instant) -> Box<dyn ModulationHistory> {
        Box::new(FmHistory::new(start_time))
    }
}

pub struct FmDemodulator {
    ifft: Ifft,
    overlap_reduce: OverlapReduce<Complex<f32>>,
}

impl Demodulator for FmDemodulator {
    fn process(&mut self, time: Instant, mut fft_data: Vec<Complex<f32>>) -> Box<dyn Any + Send> {
        self.ifft.process_inplace(&mut fft_data);
        let data = self.overlap_reduce.process(&fft_data);
        Box::new(FmDemodulation { time, data })
    }
}

#[derive(Debug)]
pub struct FmDemodulation {
    pub time: Instant,
    pub data: Vec<Complex<f32>>,
}

pub struct FmHistory {
    pub samples: Vec<Vec<Complex<f32>>>,
    pub start_time: Instant,
    pub end_time: Instant,
}

impl FmHistory {
    pub fn new(time: Instant) -> Self {
        FmHistory {
            samples: vec![],
            start_time: time,
            end_time: time,
        }
    }
}

impl ModulationHistory for FmHistory {
    fn add(&mut self, demodulation: Box<dyn Any + Send>) {
        let demodulation = *demodulation.downcast::<FmDemodulation>().unwrap();
        self.samples.push(demodulation.data);
        self.end_time = demodulation.time;
    }
    fn prune(&mut self, _retain_time: Instant) -> bool {
        // TODO
        true
    }
    fn draw_list<'a>(
        &'a self,
        stream_id: StreamId,
        channel_id: ChannelId,
        descriptor: &'a Arc<ChannelDescriptor>,
    ) -> Box<dyn Iterator<Item = (Instant, Instant, ModulationUiFn<'a>)> + 'a> {
        let ui: ModulationUiFn = Box::new(move |response| {
            egui::Popup::context_menu(response)
                .id(egui::Id::new((stream_id, channel_id)))
                .show(|ui| {
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
                            if let Err(e) = self.export_iq_data(&path) {
                                eprintln!("Failed to export IQ data: {}", e);
                            }
                        }
                    }
                });
        });
        Box::new(Some((self.start_time, self.end_time, ui)).into_iter())
    }
}

impl FmHistory {
    fn export_iq_data(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        use std::io::Write;

        let mut file = std::fs::File::create(path)?;

        for samples in &self.samples {
            for sample in samples {
                file.write_all(&sample.re.to_le_bytes())?;
                file.write_all(&sample.im.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }
}
