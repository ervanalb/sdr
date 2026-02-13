mod waterfall;

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{JoinHandle, spawn};
use waterfall::{Waterfall, WaterfallMessage};

const WATERFALL_MESSAGE_CAPACITY: usize = 64;
const CONTROL_MESSAGE_CAPACITY: usize = 64;
const STREAM_READ_TIMEOUT: f64 = 0.01;
const STREAM_BUFFER_SIZE: usize = 1024;
const WATERFALL_OUTPUT_RATE: f64 = 120.0; // 120 waterfall rows per second

fn snap_to_ranges(ranges: &[soapysdr::Range], mut value: f64) -> f64 {
    if ranges.is_empty() {
        return value;
    }

    // Find the closest range by checking distance to both min and max

    let (closest_range, _) = ranges
        .iter()
        .map(|range| {
            let dist_to_min = (value - range.minimum).abs();
            let dist_to_max = (value - range.maximum).abs();
            let dist_to_range = if value < range.minimum {
                dist_to_min
            } else if value > range.maximum {
                dist_to_max
            } else {
                0.0 // value is within range
            };
            (range, dist_to_range)
        })
        .min_by(|(_, dist_a), (_, dist_b)| dist_a.partial_cmp(dist_b).unwrap())
        .unwrap();

    // Snap to the nearest discrete step if stepsize is non-zero
    if closest_range.step > 0.0 {
        let steps_from_min = (value - closest_range.minimum) / closest_range.step;
        let rounded_steps = steps_from_min.round();
        value = closest_range.minimum + (rounded_steps * closest_range.step);
    }

    // Clamp to the desired range
    value.clamp(closest_range.minimum, closest_range.maximum)
}

pub struct HardwareDeviceDescriptor {
    pub args: soapysdr::Args,
}

impl std::fmt::Debug for HardwareDeviceDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareDeviceDescriptor")
            .field("args", &format_args!("{}", self.args))
            .finish()
    }
}

impl Clone for HardwareDeviceDescriptor {
    fn clone(&self) -> Self {
        Self {
            args: self.args.iter().collect(),
        }
    }
}

pub fn enumerate() -> Result<Vec<HardwareDeviceDescriptor>, soapysdr::Error> {
    let devices = soapysdr::enumerate("")?;
    Ok(devices
        .into_iter()
        .map(|args| HardwareDeviceDescriptor { args })
        .collect())
}

pub struct HardwareDevice {
    rx_channels: Vec<HardwareDeviceRxChannel>,
    tx_channels: Vec<HardwareDeviceTxChannel>,
}

impl HardwareDevice {
    pub fn new(descriptor: HardwareDeviceDescriptor) -> Result<Self, soapysdr::Error> {
        let device = soapysdr::Device::new(descriptor.args)?;
        let num_rx = device.num_channels(soapysdr::Direction::Rx)?;
        let num_tx = device.num_channels(soapysdr::Direction::Tx)?;

        let rx_channels = (0..num_rx)
            .map(|i| HardwareDeviceRxChannel::new(i, device.clone()))
            .collect();

        let tx_channels = (0..num_tx)
            .map(|i| HardwareDeviceTxChannel::new(i, device.clone()))
            .collect();

        Ok(Self {
            rx_channels,
            tx_channels,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HardwareDeviceRxChannelParameters {
    pub active: bool,
    pub sample_rate: f64,
    pub frequency: f64,
    pub bandwidth: f64,
}

struct ActiveHardwareDeviceRxChannel {
    join_handle: JoinHandle<()>,
    control_sender: SyncSender<HardwareDeviceRxChannelParameters>,
}

pub struct HardwareDeviceRxChannel {
    channel_index: usize,
    active: Option<ActiveHardwareDeviceRxChannel>,
    device: soapysdr::Device,
    last_parameters: HardwareDeviceRxChannelParameters,
    sample_rate_range: Vec<soapysdr::Range>,
    frequency_range: Vec<soapysdr::Range>,
    bandwidth_range: Vec<soapysdr::Range>,
    waterfall_receiver: Receiver<WaterfallMessage>,
    waterfall_sender: SyncSender<WaterfallMessage>,
}

impl HardwareDeviceRxChannel {
    fn new(channel_index: usize, device: soapysdr::Device) -> Self {
        let (waterfall_sender, waterfall_receiver) = mpsc::sync_channel(WATERFALL_MESSAGE_CAPACITY);

        let sample_rate_range = device
            .get_sample_rate_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();
        let frequency_range = device
            .frequency_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();
        let bandwidth_range = device
            .bandwidth_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();

        let last_parameters = HardwareDeviceRxChannelParameters {
            active: false,
            sample_rate: device
                .sample_rate(soapysdr::Direction::Rx, channel_index)
                .unwrap(),
            frequency: device
                .frequency(soapysdr::Direction::Rx, channel_index)
                .unwrap(),
            bandwidth: device
                .bandwidth(soapysdr::Direction::Rx, channel_index)
                .unwrap(),
        };

        Self {
            channel_index,
            active: None,
            device,
            last_parameters,
            sample_rate_range,
            frequency_range,
            bandwidth_range,
            waterfall_receiver,
            waterfall_sender,
        }
    }

    pub fn poll(&mut self, parameters: &mut HardwareDeviceRxChannelParameters) {
        // Receive updates from thread
        if let Some(active) = self.active.as_ref()
            && active.join_handle.is_finished()
        {
            self.active
                .take()
                .unwrap()
                .join_handle
                .join()
                .unwrap_or_else(|e| {
                    eprintln!("Radio RX thread panicked: {e:?}");
                })
        }

        // Snap any values in parameters to the nearest valid option
        // Only run snap_to_ranges if the given parameter has changed
        if self.last_parameters.sample_rate != parameters.sample_rate {
            parameters.sample_rate =
                snap_to_ranges(&self.sample_rate_range, parameters.sample_rate);
        }

        if self.last_parameters.frequency != parameters.frequency {
            parameters.frequency = snap_to_ranges(&self.frequency_range, parameters.frequency);
        }

        if self.last_parameters.bandwidth != parameters.bandwidth {
            parameters.bandwidth = snap_to_ranges(&self.bandwidth_range, parameters.bandwidth);
        }

        if parameters.active && !self.last_parameters.active {
            if self.active.is_some() {
                // Update active parameter: it is not valid to set it back to true
                // while we are shutting down (before the thread has terminated)
                parameters.active = false;
            } else {
                // Start new thread
                let device = self.device.clone();
                let channel_index = self.channel_index;
                let waterfall_sender = self.waterfall_sender.clone();
                let (control_sender, control_receiver) =
                    mpsc::sync_channel(CONTROL_MESSAGE_CAPACITY);

                let init_parameters = parameters.clone();
                let device_for_thread = device.clone();
                let join_handle = spawn(move || {
                    let stream = device
                        .rx_stream::<num_complex::Complex32>(&[channel_index])
                        .unwrap();
                    Self::process(
                        stream,
                        device_for_thread,
                        channel_index,
                        control_receiver,
                        waterfall_sender,
                        init_parameters,
                    );
                });

                self.active = Some(ActiveHardwareDeviceRxChannel {
                    join_handle,
                    control_sender,
                });
                self.last_parameters = parameters.clone();
            }
        }

        if &self.last_parameters != parameters {
            if let Some(active) = self.active.as_mut() {
                // Send parameter change message
                active.control_sender.send(parameters.clone()).unwrap();
            }
            self.last_parameters = parameters.clone();
        }
    }

    fn process(
        mut stream: soapysdr::RxStream<num_complex::Complex32>,
        device: soapysdr::Device,
        channel_index: usize,
        control_receiver: Receiver<HardwareDeviceRxChannelParameters>,
        waterfall_sender: SyncSender<WaterfallMessage>,
        mut current_parameters: HardwareDeviceRxChannelParameters,
    ) {
        loop {
            if !current_parameters.active {
                // Shutdown requested
                break;
            }

            // Apply current parameters
            device
                .set_sample_rate(
                    soapysdr::Direction::Rx,
                    channel_index,
                    current_parameters.sample_rate,
                )
                .unwrap();
            device
                .set_frequency(
                    soapysdr::Direction::Rx,
                    channel_index,
                    current_parameters.frequency,
                    (),
                )
                .unwrap();
            device
                .set_bandwidth(
                    soapysdr::Direction::Rx,
                    channel_index,
                    current_parameters.bandwidth,
                )
                .unwrap();

            stream.activate(None).unwrap();
            let mut buffer = vec![num_complex::Complex32::new(0.0, 0.0); STREAM_BUFFER_SIZE];
            let mut waterfall =
                Waterfall::new(current_parameters.sample_rate, WATERFALL_OUTPUT_RATE);

            // Inner loop for data reading
            loop {
                // Check for parameter changes
                if let Ok(new_parameters) = control_receiver.try_recv() {
                    // Deactivate stream to apply new parameters
                    stream.deactivate(None).unwrap();
                    current_parameters = new_parameters;
                    break; // Break to outer loop to reapply parameters
                }

                match stream.read(&mut [&mut buffer], (STREAM_READ_TIMEOUT * 1e6) as i64) {
                    Ok(_) => {
                        let waterfall_sender_clone = waterfall_sender.clone();
                        let center_frequency = current_parameters.frequency;
                        let width = current_parameters.sample_rate;

                        waterfall.process(&buffer, |waterfall_row| {
                            let msg = WaterfallMessage {
                                center_frequency,
                                width,
                                waterfall_row,
                            };
                            let _ = waterfall_sender_clone.try_send(msg);
                        });
                    }
                    Err(e) => {
                        eprintln!("Error reading from stream: {:?}", e);
                    }
                }
            }
        }

        stream.deactivate(None).ok();
    }

    pub fn try_recv_waterfall(&mut self) -> Option<WaterfallMessage> {
        self.waterfall_receiver.try_recv().ok()
    }
}

pub struct HardwareDeviceTxChannel {
    channel_index: usize,
    device: soapysdr::Device,
}

impl HardwareDeviceTxChannel {
    fn new(channel_index: usize, device: soapysdr::Device) -> Self {
        Self {
            channel_index,
            device,
        }
    }
}

pub trait IntoComplexF32 {
    fn into_complex_f32(self) -> num_complex::Complex32;
}

impl IntoComplexF32 for num_complex::Complex<i8> {
    fn into_complex_f32(self) -> num_complex::Complex32 {
        num_complex::Complex32::new(self.re as f32 / 128.0, self.im as f32 / 128.0)
    }
}

impl IntoComplexF32 for num_complex::Complex<i16> {
    fn into_complex_f32(self) -> num_complex::Complex32 {
        num_complex::Complex32::new(self.re as f32 / 32768.0, self.im as f32 / 32768.0)
    }
}

impl IntoComplexF32 for num_complex::Complex<i32> {
    fn into_complex_f32(self) -> num_complex::Complex32 {
        num_complex::Complex32::new(self.re as f32 / 2147483648.0, self.im as f32 / 2147483648.0)
    }
}

impl IntoComplexF32 for num_complex::Complex32 {
    fn into_complex_f32(self) -> num_complex::Complex32 {
        self
    }
}
