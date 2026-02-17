use crate::waterfall::Waterfall;
use log::{info, warn};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{JoinHandle, spawn};
use std::time::{Duration, Instant};
use std::{mem, thread};

const WATERFALL_MESSAGE_CAPACITY: usize = 64;
const CONTROL_MESSAGE_CAPACITY: usize = 64;
const STREAM_READ_TIMEOUT: f64 = 1.;
const STREAM_BUFFER_DURATION: f64 = 0.001;
const WATERFALL_TARGET_BIN_SIZE: f64 = 5_000.0; // 5 KHz
const WATERFALL_OUTPUT_PERIOD: f64 = 0.005; // 200 waterfall rows per second
const SHUTDOWN_POLLING_PERIOD: f64 = 0.01;

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

pub struct WaterfallMessage {
    pub device_id: HardwareDeviceId,
    pub channel_index: usize,
    pub start_time: Instant,
    pub end_time: Instant,
    pub period: f64,
    pub center_frequency: f64,
    pub width: f64,
    pub waterfall_row: Vec<f32>,
}

type HardwareDeviceId = String;

#[derive(Clone, Debug, Default)]
pub struct HardwareParams {
    pub run: bool,
    pub devices: HashMap<HardwareDeviceId, HardwareDeviceParams>,
    pub enumerate: bool,
}

pub struct Hardware {
    devices: HashMap<HardwareDeviceId, HardwareDevice>,
    waterfall_sender: SyncSender<WaterfallMessage>,
    waterfall_receiver: Receiver<WaterfallMessage>,
}

impl Hardware {
    pub fn new() -> Self {
        let (waterfall_sender, waterfall_receiver) = mpsc::sync_channel(WATERFALL_MESSAGE_CAPACITY);
        Hardware {
            devices: Default::default(),
            waterfall_sender,
            waterfall_receiver,
        }
    }

    pub fn update(&mut self, params: &mut HardwareParams) {
        if params.enumerate {
            self.devices
                .retain(|_, d| matches!(d.active, HardwareState::Active(_)));

            let enumerated_devices = soapysdr::enumerate("").unwrap();
            for enumerated_args in enumerated_devices {
                let enumerated_id = format!("{}", enumerated_args);
                if self.devices.iter().any(|(id, _)| id == &enumerated_id) {
                    continue;
                }
                self.devices.insert(
                    enumerated_id.clone(),
                    HardwareDevice::new(
                        enumerated_id,
                        enumerated_args,
                        self.waterfall_sender.clone(),
                    ),
                );
            }
            params.enumerate = false;
        }

        // Remove any devices from params that are not present
        params.devices.retain(|id, _| self.devices.contains_key(id));

        // Insert devices that are present but missing from params
        for (id, _) in self.devices.iter() {
            if let Entry::Vacant(e) = params.devices.entry(id.to_string()) {
                e.insert(Default::default());
            }
        }
        params.devices.retain(|id, _| self.devices.contains_key(id));

        // Call update on each device
        for (id, device_params) in params.devices.iter_mut() {
            let device = self.devices.get_mut(id).unwrap();
            device.update(device_params, params.run);
        }
    }

    pub fn shutdown(mut self) {
        let polling_period = Duration::from_micros((SHUTDOWN_POLLING_PERIOD * 1e9) as u64);
        let mut params = Default::default();
        while !self
            .devices
            .values()
            .all(|device| matches!(device.active, HardwareState::Inactive))
        {
            self.update(&mut params);
            thread::sleep(polling_period);
        }
    }

    pub fn waterfall_try_recv(&mut self) -> Option<WaterfallMessage> {
        self.waterfall_receiver.try_recv().ok()
    }
}

struct HardwareDevice {
    device_id: HardwareDeviceId,
    args: soapysdr::Args,
    active: HardwareState<ActiveHardwareDevice>,
    waterfall_sender: SyncSender<WaterfallMessage>,
}

enum HardwareState<T> {
    Inactive,
    Active(T),
    ShuttingDown(T),
}

impl<T> HardwareState<T> {
    fn shutdown(&mut self) {
        let Self::Active(val) = mem::replace(self, HardwareState::Inactive) else {
            panic!("Must be in Active state to shut down");
        };
        *self = Self::ShuttingDown(val);
    }
    fn deactivate(&mut self) -> T {
        let Self::ShuttingDown(val) = mem::replace(self, HardwareState::Inactive) else {
            panic!("Must be in ShuttingDown state to deactivate");
        };
        val
    }
}

struct ActiveHardwareDevice {
    rx_channels: Vec<HardwareDeviceRxChannel>,
    tx_channels: Vec<HardwareDeviceTxChannel>,
}

#[derive(Clone, Debug, Default)]
pub struct HardwareDeviceParams {
    pub active: bool,
    pub rx_channels: Vec<HardwareDeviceRxChannelParams>,
    pub tx_channels: Vec<HardwareDeviceTxChannelParams>,
}

impl HardwareDevice {
    fn new(
        device_id: HardwareDeviceId,
        args: soapysdr::Args,
        waterfall_sender: SyncSender<WaterfallMessage>,
    ) -> Self {
        HardwareDevice {
            device_id,
            args,
            active: HardwareState::Inactive,
            waterfall_sender,
        }
    }

    fn update(&mut self, params: &mut HardwareDeviceParams, run: bool) {
        match &self.active {
            // Handle state transitions
            HardwareState::Inactive => {
                if params.active {
                    // Create a new device
                    let device = soapysdr::Device::new(format!("{}", self.args).as_str()).unwrap();
                    let num_rx = device.num_channels(soapysdr::Direction::Rx).unwrap();
                    let num_tx = device.num_channels(soapysdr::Direction::Tx).unwrap();

                    let rx_channels = (0..num_rx)
                        .map(|i| {
                            HardwareDeviceRxChannel::new(
                                self.device_id.clone(),
                                device.clone(),
                                i,
                                self.waterfall_sender.clone(),
                            )
                        })
                        .collect();

                    let tx_channels = (0..num_tx)
                        .map(|i| HardwareDeviceTxChannel::new(i, device.clone()))
                        .collect();

                    self.active = HardwareState::Active(ActiveHardwareDevice {
                        rx_channels,
                        tx_channels,
                    });
                }
            }
            HardwareState::Active(_) => {
                if !params.active {
                    self.active.shutdown();
                }
            }
            HardwareState::ShuttingDown(active) => {
                if active
                    .rx_channels
                    .iter()
                    .all(|channel| matches!(channel.active, HardwareState::Inactive))
                // && active
                //    .tx_channels
                //    .iter()
                //    .all(|channel| matches!(channel.active, HardwareState::Inactive))
                {
                    self.active.deactivate();
                }
            }
        };

        // Update params
        let shutting_down = matches!(self.active, HardwareState::ShuttingDown(_));
        match &mut self.active {
            HardwareState::Inactive => {
                // Make sure params reflects no channels
                params.rx_channels.clear();
                params.tx_channels.clear();
            }
            HardwareState::Active(active) | HardwareState::ShuttingDown(active) => {
                // Remove extra channels from params
                params.rx_channels.truncate(active.rx_channels.len());
                params.tx_channels.truncate(active.tx_channels.len());

                // Add missing channels to params
                for _ in params.rx_channels.len()..active.rx_channels.len() {
                    params.rx_channels.push(Default::default());
                }
                for _ in params.tx_channels.len()..active.tx_channels.len() {
                    params.tx_channels.push(Default::default());
                }

                if shutting_down {
                    // It is not valid for active = true
                    // while we are shutting down
                    params.active = false;
                }

                // Call update on all channels in params
                for (channel, channel_params) in active
                    .rx_channels
                    .iter_mut()
                    .zip(params.rx_channels.iter_mut())
                {
                    if shutting_down {
                        channel_params.active = false;
                    }
                    channel.update(channel_params, run);
                }

                for (channel, channel_params) in active
                    .tx_channels
                    .iter_mut()
                    .zip(params.tx_channels.iter_mut())
                {
                    if shutting_down {
                        channel_params.active = false;
                    }
                    channel.update(channel_params);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HardwareDeviceRxChannelParams {
    pub active: bool,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
    pub bandwidth: Option<f64>,
    pub sample_rate_min: f64,
    pub sample_rate_max: f64,
    pub frequency_min: f64,
    pub frequency_max: f64,
    pub bandwidth_min: f64,
    pub bandwidth_max: f64,
}

struct ActiveHardwareDeviceRxChannel {
    join_handle: JoinHandle<()>,
    control_sender: SyncSender<HardwareDeviceRxChannelControlMessage>,
}

enum HardwareDeviceRxChannelControlMessage {
    SetSampleRate(f64),
    SetFrequency(f64),
    SetBandwidth(f64),
    Shutdown,
}

struct HardwareDeviceRxChannel {
    device_id: HardwareDeviceId,
    channel_index: usize,
    waterfall_sender: SyncSender<WaterfallMessage>,
    active: HardwareState<ActiveHardwareDeviceRxChannel>,
    device: soapysdr::Device,
    sample_rate_range: Vec<soapysdr::Range>,
    frequency_range: Vec<soapysdr::Range>,
    bandwidth_range: Vec<soapysdr::Range>,
    sample_rate_min: f64,
    sample_rate_max: f64,
    frequency_min: f64,
    frequency_max: f64,
    bandwidth_min: f64,
    bandwidth_max: f64,
    sample_rate: f64,
    frequency: f64,
    bandwidth: f64,
}

fn compute_range_min_max(ranges: &[soapysdr::Range]) -> (f64, f64) {
    if ranges.is_empty() {
        return (0.0, 0.0);
    }
    let min = ranges
        .iter()
        .map(|r| r.minimum)
        .fold(f64::INFINITY, f64::min);
    let max = ranges
        .iter()
        .map(|r| r.maximum)
        .fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

impl HardwareDeviceRxChannel {
    fn new(
        device_id: HardwareDeviceId,
        device: soapysdr::Device,
        channel_index: usize,
        waterfall_sender: SyncSender<WaterfallMessage>,
    ) -> Self {
        let sample_rate_range = device
            .get_sample_rate_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();
        let frequency_range = device
            .frequency_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();
        let bandwidth_range = device
            .bandwidth_range(soapysdr::Direction::Rx, channel_index)
            .unwrap_or_default();

        let (sample_rate_min, sample_rate_max) = compute_range_min_max(&sample_rate_range);
        let (frequency_min, frequency_max) = compute_range_min_max(&frequency_range);
        let (bandwidth_min, bandwidth_max) = compute_range_min_max(&bandwidth_range);

        // Set sample_rate and bandwidth to max values
        device
            .set_sample_rate(soapysdr::Direction::Rx, channel_index, sample_rate_max)
            .ok();
        device
            .set_bandwidth(soapysdr::Direction::Rx, channel_index, bandwidth_max)
            .ok();

        // Read current values (which should now be the max values we just set)
        let sample_rate = device
            .sample_rate(soapysdr::Direction::Rx, channel_index)
            .unwrap();
        let frequency = device
            .frequency(soapysdr::Direction::Rx, channel_index)
            .unwrap();
        let bandwidth = device
            .bandwidth(soapysdr::Direction::Rx, channel_index)
            .unwrap();

        Self {
            device_id,
            channel_index,
            active: HardwareState::Inactive,
            device,
            sample_rate_range,
            frequency_range,
            bandwidth_range,
            sample_rate_min,
            sample_rate_max,
            frequency_min,
            frequency_max,
            bandwidth_min,
            bandwidth_max,
            sample_rate,
            frequency,
            bandwidth,
            waterfall_sender,
        }
    }

    fn update(&mut self, params: &mut HardwareDeviceRxChannelParams, run: bool) {
        // Effectively AND the run flag with the channel's active flag
        let should_be_active = params.active && run;

        match &self.active {
            // Handle state transitions
            HardwareState::Inactive => {
                if should_be_active {
                    // Start new thread
                    let device_id = self.device_id.clone();
                    let channel_index = self.channel_index;
                    let device = self.device.clone();
                    let waterfall_sender = self.waterfall_sender.clone();
                    let (control_sender, control_receiver) =
                        mpsc::sync_channel(CONTROL_MESSAGE_CAPACITY);
                    let sample_rate = self.sample_rate;
                    let frequency = self.frequency;
                    let bandwidth = self.bandwidth;
                    let join_handle = spawn(move || {
                        Self::process(
                            device_id,
                            channel_index,
                            device,
                            control_receiver,
                            waterfall_sender,
                            sample_rate,
                            frequency,
                            bandwidth,
                        );
                    });

                    self.active = HardwareState::Active(ActiveHardwareDeviceRxChannel {
                        join_handle,
                        control_sender,
                    });
                }
            }
            HardwareState::Active(active) => {
                if !should_be_active {
                    active
                        .control_sender
                        .send(HardwareDeviceRxChannelControlMessage::Shutdown)
                        .unwrap();
                    self.active.shutdown();
                } else if active.join_handle.is_finished() {
                    self.active.shutdown();
                    let active = self.active.deactivate();
                    match active.join_handle.join() {
                        Err(e) => {
                            eprintln!("Radio RX thread panicked: {e:?}");
                        }
                        Ok(()) => {
                            eprintln!("Radio RX thread terminated unexpectedly");
                        }
                    }
                }
            }
            HardwareState::ShuttingDown(active) => {
                if active.join_handle.is_finished() {
                    let active = self.active.deactivate();
                    active.join_handle.join().unwrap_or_else(|e| {
                        eprintln!("Radio RX thread panicked while shutting down: {e:?}");
                    });
                }
            }
        };

        // Update params:

        // Always write min/max values to params
        params.sample_rate_min = self.sample_rate_min;
        params.sample_rate_max = self.sample_rate_max;
        params.frequency_min = self.frequency_min;
        params.frequency_max = self.frequency_max;
        params.bandwidth_min = self.bandwidth_min;
        params.bandwidth_max = self.bandwidth_max;

        // If params has None values, assign from hardware struct
        if params.sample_rate.is_none() {
            params.sample_rate = Some(self.sample_rate);
        }
        if params.frequency.is_none() {
            params.frequency = Some(self.frequency);
        }
        if params.bandwidth.is_none() {
            params.bandwidth = Some(self.bandwidth);
        }

        if matches!(self.active, HardwareState::ShuttingDown(_)) {
            // It is not valid for active = true
            // while we are shutting down
            if run {
                params.active = false;
            }
        }

        // Snap any values in parameters to the nearest valid option
        // Only run snap_to_ranges if the given parameter has changed
        if self.sample_rate != params.sample_rate.unwrap() {
            params.sample_rate = Some(snap_to_ranges(
                &self.sample_rate_range,
                params.sample_rate.unwrap(),
            ));
        }

        if self.frequency != params.frequency.unwrap() {
            params.frequency = Some(snap_to_ranges(
                &self.frequency_range,
                params.frequency.unwrap(),
            ));
        }

        if self.bandwidth != params.bandwidth.unwrap() {
            params.bandwidth = Some(snap_to_ranges(
                &self.bandwidth_range,
                params.bandwidth.unwrap(),
            ));
        }

        // If parameters have changed, send message
        if params.sample_rate.unwrap() != self.sample_rate {
            self.sample_rate = params.sample_rate.unwrap();
            if let HardwareState::Active(active) = &self.active {
                active
                    .control_sender
                    .send(HardwareDeviceRxChannelControlMessage::SetSampleRate(
                        self.sample_rate,
                    ))
                    .unwrap();
            }
        }
        if params.frequency.unwrap() != self.frequency {
            self.frequency = params.frequency.unwrap();
            if let HardwareState::Active(active) = &self.active {
                active
                    .control_sender
                    .send(HardwareDeviceRxChannelControlMessage::SetFrequency(
                        self.frequency,
                    ))
                    .unwrap();
            }
        }
        if params.bandwidth.unwrap() != self.bandwidth {
            self.bandwidth = params.bandwidth.unwrap();
            if let HardwareState::Active(active) = &self.active {
                active
                    .control_sender
                    .send(HardwareDeviceRxChannelControlMessage::SetBandwidth(
                        self.bandwidth,
                    ))
                    .unwrap();
            }
        }
    }

    fn process(
        device_id: HardwareDeviceId,
        channel_index: usize,
        device: soapysdr::Device,
        control_receiver: Receiver<HardwareDeviceRxChannelControlMessage>,
        waterfall_sender: SyncSender<WaterfallMessage>,
        mut sample_rate: f64,
        mut frequency: f64,
        mut bandwidth: f64,
    ) {
        info!("Started thread for RX channel {channel_index:?} on device {device_id:?}");
        'outer: loop {
            // Apply current parameters
            device
                .set_sample_rate(soapysdr::Direction::Rx, channel_index, sample_rate)
                .unwrap();
            device
                .set_frequency(soapysdr::Direction::Rx, channel_index, frequency, ())
                .unwrap();
            device
                .set_bandwidth(soapysdr::Direction::Rx, channel_index, bandwidth)
                .unwrap();

            let buffer_size = (STREAM_BUFFER_DURATION * sample_rate) as usize;
            let buffer_size = buffer_size.next_power_of_two();
            info!(
                "Channel parameters: sample_rate={sample_rate:?}, frequency={frequency:?}, bandwidth={bandwidth:?}, buffer_size={buffer_size:?}"
            );

            let mut buffer = vec![num_complex::Complex::<i8>::new(0, 0); buffer_size];
            let mut waterfall = Waterfall::new(
                sample_rate,
                WATERFALL_TARGET_BIN_SIZE,
                WATERFALL_OUTPUT_PERIOD,
            );
            info!("Opening stream");
            let mut stream = device
                .rx_stream::<num_complex::Complex<i8>>(&[channel_index])
                .unwrap();
            stream.activate(None).unwrap();
            let mut last_t = Instant::now();

            // Inner loop for data reading
            'inner: loop {
                // Check for parameter changes
                let mut new_parameters = false;
                while let Ok(msg) = control_receiver.try_recv() {
                    match msg {
                        HardwareDeviceRxChannelControlMessage::SetSampleRate(x) => {
                            sample_rate = x;
                            new_parameters = true;
                        }
                        HardwareDeviceRxChannelControlMessage::SetFrequency(x) => {
                            frequency = x;
                            new_parameters = true;
                        }
                        HardwareDeviceRxChannelControlMessage::SetBandwidth(x) => {
                            bandwidth = x;
                            new_parameters = true;
                        }
                        HardwareDeviceRxChannelControlMessage::Shutdown => {
                            break 'outer;
                        }
                    }
                }
                if new_parameters {
                    break 'inner;
                }

                match stream.read(&mut [&mut buffer], (STREAM_READ_TIMEOUT * 1e6) as i64) {
                    Ok(len) => {
                        let t = Instant::now();
                        let waterfall_sender_clone = waterfall_sender.clone();
                        let center_frequency = frequency;
                        let width = sample_rate;
                        let period = waterfall.period();

                        waterfall.process(&buffer[..len], |waterfall_row| {
                            let msg = WaterfallMessage {
                                device_id: device_id.clone(),
                                channel_index,
                                start_time: last_t,
                                end_time: t,
                                period,
                                center_frequency,
                                width,
                                waterfall_row,
                            };
                            waterfall_sender_clone
                                .try_send(msg)
                                .unwrap_or_else(|e| warn!("Dropped waterfall message: {e:?}"));
                            last_t = t;
                        });
                    }
                    Err(e) => {
                        warn!("Error reading from stream: {e:?}");
                    }
                }
            }
            info!("Closing stream");
            stream.deactivate(None).ok();
        }

        info!("Stopping thread for RX channel {channel_index:?} on device {device_id:?}");
    }
}

#[derive(Debug, Clone, Default)]
pub struct HardwareDeviceTxChannelParams {
    pub active: bool,
    pub sample_rate: f64,
    pub frequency: f64,
    pub bandwidth: f64,
}

struct HardwareDeviceTxChannel {
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
    fn update(&mut self, _params: &mut HardwareDeviceTxChannelParams) {}
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
