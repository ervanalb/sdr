use crate::id_factory::IdFactory;
use log::{info, warn};
use num_complex::Complex;
use rayon::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{JoinHandle, spawn};
use chrono::{DateTime, Utc};
use std::{mem, thread};

const STREAM_CREATION_MESSAGE_CAPACITY: usize = 64;
const STREAM_MESSAGE_CAPACITY: usize = 64;
const CONTROL_MESSAGE_CAPACITY: usize = 64;
const STREAM_READ_TIMEOUT: f64 = 1.;
const STREAM_CHUNK_PERIOD: f64 = 0.005; // 200 per second
const SHUTDOWN_POLLING_PERIOD: f64 = 0.01;

// IDS //

pub type StreamId = usize;
pub type HardwareDeviceId = String;

// DESCRIPTORS //

#[derive(Clone, Debug)]
pub struct ReceiveStreamDescriptor {
    pub device_id: HardwareDeviceId,
    pub stream_index: usize,
    pub frequency: f64,
    pub sample_rate: f64,
    pub start_time: DateTime<Utc>,
}

// RESULTS //

#[derive(Debug)]
pub struct HardwareResult {
    pub receive_streams: BTreeMap<StreamId, ReceiveStreamResult>,
}

#[derive(Debug)]
pub struct ReceiveStreamResult {
    pub descriptor: Arc<ReceiveStreamDescriptor>,
    pub data: Vec<ReceiveStreamChunk>,
}

pub struct ReceiveStreamChunk {
    pub iq_data: Vec<Complex<f32>>,
    pub time: DateTime<Utc>,
}

impl std::fmt::Debug for ReceiveStreamChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiveStreamChunk")
            .field("iq_data.len()", &self.iq_data.len())
            .field("time", &self.time)
            .finish()
    }
}

// PARAMS //

#[derive(Clone, Debug)]
pub struct HardwareParams {
    pub run: bool,
    pub devices: HashMap<HardwareDeviceId, HardwareDeviceParams>,
    pub enumerate: bool,
}

impl Default for HardwareParams {
    fn default() -> Self {
        Self {
            run: false,
            devices: Default::default(),
            enumerate: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HardwareDeviceParams {
    pub active: bool,
    pub rx_streams: Vec<HardwareDeviceRxStreamParams>,
    pub tx_streams: Vec<HardwareDeviceTxStreamParams>,
}

impl Default for HardwareDeviceParams {
    fn default() -> Self {
        Self {
            active: true,
            rx_streams: Default::default(),
            tx_streams: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HardwareDeviceRxStreamParams {
    pub active: bool,
    pub sample_rate: Option<f64>,
    pub frequency: Option<f64>,
    pub bandwidth: Option<f64>,
    pub gains: HashMap<String, GainParams>,
    pub sample_rate_min: f64,
    pub sample_rate_max: f64,
    pub frequency_min: f64,
    pub frequency_max: f64,
    pub bandwidth_min: f64,
    pub bandwidth_max: f64,
}

impl Default for HardwareDeviceRxStreamParams {
    fn default() -> Self {
        Self {
            active: true,
            sample_rate: None,
            frequency: None,
            bandwidth: None,
            gains: Default::default(),
            sample_rate_min: 0.,
            sample_rate_max: 0.,
            frequency_min: 0.,
            frequency_max: 0.,
            bandwidth_min: 0.,
            bandwidth_max: 0.,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GainParams {
    pub value: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Default)]
pub struct HardwareDeviceTxStreamParams {
    pub active: bool,
    pub sample_rate: f64,
    pub frequency: f64,
    pub bandwidth: f64,
}

// HARDWARE //

pub struct Hardware {
    devices: HashMap<HardwareDeviceId, HardwareDevice>,
    receive_stream_creation_sender:
        SyncSender<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
    receive_stream_creation_receiver:
        Receiver<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
    receive_streams: Vec<ReceiveStream>,
    receive_stream_id_factory: IdFactory,
}

impl Hardware {
    pub fn new() -> Self {
        let (receive_stream_creation_sender, receive_stream_creation_receiver) =
            mpsc::sync_channel(STREAM_CREATION_MESSAGE_CAPACITY);
        Hardware {
            devices: Default::default(),
            receive_stream_creation_sender,
            receive_stream_creation_receiver,
            receive_streams: vec![],
            receive_stream_id_factory: IdFactory::default(),
        }
    }

    pub fn update(&mut self, params: &mut HardwareParams) -> HardwareResult {
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
                        self.receive_stream_creation_sender.clone(),
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

        // Create new streams
        while let Ok((descriptor, receiver)) = self.receive_stream_creation_receiver.try_recv() {
            self.receive_streams.push(ReceiveStream::new(
                self.receive_stream_id_factory.create(),
                descriptor,
                receiver,
            ));
        }

        // Process all streams, and remove disconnected ones
        // This is a somewhat convoluted way to achieve a parallel .retain() operation.
        let empty_vec = Vec::with_capacity(self.receive_streams.len());
        let old_receive_streams = mem::replace(&mut self.receive_streams, empty_vec);
        self.receive_streams.par_extend(
            old_receive_streams
                .into_par_iter()
                .filter_map(|mut stream| stream.process().then_some(stream)),
        );

        // Collect all received data into a HardwareResult
        HardwareResult {
            receive_streams: self
                .receive_streams
                .iter_mut()
                .map(|s| (s.stream_id, s.result()))
                .collect(),
        }
    }

    pub fn shutdown(mut self) {
        let polling_period = std::time::Duration::from_secs_f64(SHUTDOWN_POLLING_PERIOD);
        let mut params = HardwareParams {
            enumerate: false,
            ..Default::default()
        };
        self.update(&mut params);
        for device in &mut params.devices.values_mut() {
            device.active = false;
        }
        self.update(&mut params);
        while !self
            .devices
            .values()
            .all(|device| matches!(device.active, HardwareState::Inactive))
        {
            thread::sleep(polling_period);
            self.update(&mut params);
        }
    }
}

struct HardwareDevice {
    device_id: HardwareDeviceId,
    args: soapysdr::Args,
    active: HardwareState<ActiveHardwareDevice>,
    receive_stream_creation_sender:
        SyncSender<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
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
    rx_streams: Vec<HardwareDeviceRxStream>,
    tx_streams: Vec<HardwareDeviceTxStream>,
}

impl HardwareDevice {
    fn new(
        device_id: HardwareDeviceId,
        args: soapysdr::Args,
        receive_stream_creation_sender: SyncSender<(
            ReceiveStreamDescriptor,
            Receiver<ReceiveStreamChunk>,
        )>,
    ) -> Self {
        HardwareDevice {
            device_id,
            args,
            active: HardwareState::Inactive,
            receive_stream_creation_sender,
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

                    let rx_streams = (0..num_rx)
                        .map(|i| {
                            HardwareDeviceRxStream::new(
                                self.device_id.clone(),
                                device.clone(),
                                i,
                                self.receive_stream_creation_sender.clone(),
                            )
                        })
                        .collect();

                    let tx_streams = (0..num_tx)
                        .map(|i| HardwareDeviceTxStream::new(i, device.clone()))
                        .collect();

                    self.active = HardwareState::Active(ActiveHardwareDevice {
                        rx_streams,
                        tx_streams,
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
                    .rx_streams
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
                params.rx_streams.clear();
                params.tx_streams.clear();
            }
            HardwareState::Active(active) | HardwareState::ShuttingDown(active) => {
                // Remove extra streams from params
                params.rx_streams.truncate(active.rx_streams.len());
                params.tx_streams.truncate(active.tx_streams.len());

                // Add missing streams to params
                for _ in params.rx_streams.len()..active.rx_streams.len() {
                    params.rx_streams.push(Default::default());
                }
                for _ in params.tx_streams.len()..active.tx_streams.len() {
                    params.tx_streams.push(Default::default());
                }

                if shutting_down {
                    // It is not valid for active = true
                    // while we are shutting down
                    params.active = false;
                }

                // Call update on all streams in params
                for (stream, stream_params) in active
                    .rx_streams
                    .iter_mut()
                    .zip(params.rx_streams.iter_mut())
                {
                    if shutting_down {
                        stream_params.active = false;
                    }
                    stream.update(stream_params, run);
                }

                for (stream, stream_params) in active
                    .tx_streams
                    .iter_mut()
                    .zip(params.tx_streams.iter_mut())
                {
                    if shutting_down {
                        stream_params.active = false;
                    }
                    stream.update(stream_params);
                }
            }
        }
    }
}

struct ActiveHardwareDeviceRxStream {
    join_handle: JoinHandle<()>,
    control_sender: SyncSender<HardwareDeviceRxStreamControlMessage>,
}

enum HardwareDeviceRxStreamControlMessage {
    SetSampleRate(f64),
    SetFrequency(f64),
    SetBandwidth(f64),
    SetGain(String, f64),
    Shutdown,
}

#[derive(Clone, Debug)]
struct HardwareGain {
    value: f64,
    range: soapysdr::Range,
}

struct HardwareDeviceRxStream {
    device_id: HardwareDeviceId,
    stream_index: usize,
    stream_creation_sender: SyncSender<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
    active: HardwareState<ActiveHardwareDeviceRxStream>,
    device: soapysdr::Device,
    sample_rate_range: Vec<soapysdr::Range>,
    frequency_range: Vec<soapysdr::Range>,
    bandwidth_range: Vec<soapysdr::Range>,
    gains: HashMap<String, HardwareGain>,
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

struct ReceiveStream {
    stream_id: StreamId,
    receiver: Receiver<ReceiveStreamChunk>,
    descriptor: Arc<ReceiveStreamDescriptor>,
    data: Vec<ReceiveStreamChunk>,
}

impl ReceiveStream {
    fn new(
        stream_id: StreamId,
        descriptor: ReceiveStreamDescriptor,
        receiver: Receiver<ReceiveStreamChunk>,
    ) -> Self {
        ReceiveStream {
            stream_id,
            descriptor: Arc::new(descriptor),
            receiver,
            data: vec![],
        }
    }

    fn process(&mut self) -> bool {
        loop {
            match self.receiver.try_recv() {
                Ok(msg) => {
                    self.data.push(msg);
                }
                Err(TryRecvError::Empty) => {
                    return true; // Stream is still connected, keep it
                }
                Err(TryRecvError::Disconnected) => {
                    return !self.data.is_empty(); // Stream is disconnected, remove it if the queue is empty
                }
            }
        }
    }

    fn result(&mut self) -> ReceiveStreamResult {
        ReceiveStreamResult {
            descriptor: self.descriptor.clone(),
            data: mem::replace(&mut self.data, vec![]),
        }
    }
}

impl HardwareDeviceRxStream {
    fn new(
        device_id: HardwareDeviceId,
        device: soapysdr::Device,
        stream_index: usize,
        stream_creation_sender: SyncSender<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
    ) -> Self {
        let sample_rate_range = device
            .get_sample_rate_range(soapysdr::Direction::Rx, stream_index)
            .unwrap();
        let frequency_range = device
            .frequency_range(soapysdr::Direction::Rx, stream_index)
            .unwrap();
        let bandwidth_range = device
            .bandwidth_range(soapysdr::Direction::Rx, stream_index)
            .unwrap();

        let (sample_rate_min, sample_rate_max) = compute_range_min_max(&sample_rate_range).unwrap();
        let (frequency_min, frequency_max) = compute_range_min_max(&frequency_range).unwrap();
        let (bandwidth_min, bandwidth_max) = compute_range_min_max(&bandwidth_range).unwrap();

        // List available gain elements
        let gain_elements = device
            .list_gains(soapysdr::Direction::Rx, stream_index)
            .unwrap_or_default();

        // Get gain ranges for each element
        let gains: HashMap<_, _> = gain_elements
            .into_iter()
            .map(|gain_name| {
                let range = device
                    .gain_element_range(soapysdr::Direction::Rx, stream_index, gain_name.as_str())
                    .unwrap();
                let gain = device
                    .gain_element(soapysdr::Direction::Rx, stream_index, gain_name.as_str())
                    .unwrap();
                (gain_name, HardwareGain { value: gain, range })
            })
            .collect();

        // Set sample_rate and bandwidth to max values
        device
            .set_sample_rate(soapysdr::Direction::Rx, stream_index, sample_rate_max)
            .ok();
        device
            .set_bandwidth(soapysdr::Direction::Rx, stream_index, bandwidth_max)
            .ok();

        // Read current values (which should now be the max values we just set)
        let sample_rate = device
            .sample_rate(soapysdr::Direction::Rx, stream_index)
            .unwrap();
        let frequency = device
            .frequency(soapysdr::Direction::Rx, stream_index)
            .unwrap();
        let bandwidth = device
            .bandwidth(soapysdr::Direction::Rx, stream_index)
            .unwrap();

        Self {
            device_id,
            stream_index,
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
            gains,
            stream_creation_sender,
        }
    }

    fn update(&mut self, params: &mut HardwareDeviceRxStreamParams, run: bool) {
        // Effectively AND the run flag with the stream's active flag
        let should_be_active = params.active && run;

        match &self.active {
            // Handle state transitions
            HardwareState::Inactive => {
                if should_be_active {
                    // Start new thread
                    let device_id = self.device_id.clone();
                    let stream_index = self.stream_index;
                    let device = self.device.clone();
                    let stream_creation_sender = self.stream_creation_sender.clone();
                    let (control_sender, control_receiver) =
                        mpsc::sync_channel(CONTROL_MESSAGE_CAPACITY);
                    let sample_rate = self.sample_rate;
                    let frequency = self.frequency;
                    let bandwidth = self.bandwidth;
                    let gains = self.gains.clone();
                    let join_handle = spawn(move || {
                        Self::process(
                            device_id,
                            stream_index,
                            device,
                            control_receiver,
                            stream_creation_sender,
                            sample_rate,
                            frequency,
                            bandwidth,
                            gains,
                        );
                    });

                    self.active = HardwareState::Active(ActiveHardwareDeviceRxStream {
                        join_handle,
                        control_sender,
                    });
                }
            }
            HardwareState::Active(active) => {
                if !should_be_active {
                    active
                        .control_sender
                        .send(HardwareDeviceRxStreamControlMessage::Shutdown)
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
                    .send(HardwareDeviceRxStreamControlMessage::SetSampleRate(
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
                    .send(HardwareDeviceRxStreamControlMessage::SetFrequency(
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
                    .send(HardwareDeviceRxStreamControlMessage::SetBandwidth(
                        self.bandwidth,
                    ))
                    .unwrap();
            }
        }

        // Retain only gain elements in params that are present in the hardware
        params.gains.retain(|k, _| self.gains.contains_key(k));
        // Update gains in params & populate any missing ones
        for (gain_name, hw_gain) in &mut self.gains {
            match params.gains.entry(gain_name.to_string()) {
                Entry::Occupied(mut e) => {
                    let params = e.get_mut();
                    // Always set min & max
                    params.min = hw_gain.range.minimum;
                    params.max = hw_gain.range.maximum;
                    // If value has changed, snap it
                    if params.value != hw_gain.value {
                        let snapped_value = snap_to_range(&hw_gain.range, params.value);
                        params.value = snapped_value;

                        // After snapping, if value has changed, send message to the hardware
                        if params.value != hw_gain.value {
                            hw_gain.value = params.value;
                            if let HardwareState::Active(active) = &self.active {
                                active
                                    .control_sender
                                    .send(HardwareDeviceRxStreamControlMessage::SetGain(
                                        gain_name.to_string(),
                                        hw_gain.value,
                                    ))
                                    .unwrap();
                            }
                        }
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(GainParams {
                        value: hw_gain.value,
                        min: hw_gain.range.minimum,
                        max: hw_gain.range.maximum,
                    });
                }
            }
        }
    }

    fn process(
        device_id: HardwareDeviceId,
        stream_index: usize,
        device: soapysdr::Device,
        control_receiver: Receiver<HardwareDeviceRxStreamControlMessage>,
        stream_creation_sender: SyncSender<(ReceiveStreamDescriptor, Receiver<ReceiveStreamChunk>)>,
        mut sample_rate: f64,
        mut frequency: f64,
        mut bandwidth: f64,
        gains: HashMap<String, HardwareGain>,
    ) {
        info!("Started thread for RX stream {stream_index:?} on device {device_id:?}");
        // Apply initial parameters
        device
            .set_frequency(soapysdr::Direction::Rx, stream_index, frequency, ())
            .unwrap();

        for (gain_name, gain) in &gains {
            device
                .set_gain_element(
                    soapysdr::Direction::Rx,
                    stream_index,
                    gain_name.as_str(),
                    gain.value,
                )
                .unwrap();
        }
        device
            .set_bandwidth(soapysdr::Direction::Rx, stream_index, bandwidth)
            .unwrap();

        'outer: loop {
            // Apply current parameters
            device
                .set_sample_rate(soapysdr::Direction::Rx, stream_index, sample_rate)
                .unwrap();

            info!("Opening stream");
            let mut stream = device
                .rx_stream::<num_complex::Complex<f32>>(&[stream_index])
                .unwrap();
            stream.activate(None).unwrap();

            'middle: loop {
                let start_time = Utc::now();

                let (stream_sender, receiver) = mpsc::sync_channel(STREAM_MESSAGE_CAPACITY);
                stream_creation_sender
                    .send((
                        ReceiveStreamDescriptor {
                            device_id: device_id.clone(),
                            stream_index,
                            frequency,
                            sample_rate,
                            start_time,
                        },
                        receiver,
                    ))
                    .unwrap();

                let buffer_size = (sample_rate * STREAM_CHUNK_PERIOD).round() as usize;
                info!(
                    "Hardware channel parameters: sample_rate={sample_rate:?}, frequency={frequency:?}, bandwidth={bandwidth:?}, buffer_size={buffer_size:?}"
                );
                let mut buffer = vec![num_complex::Complex::<f32>::ZERO; buffer_size];
                let mut buffer_ix = 0;

                // Inner loop for data reading
                'inner: loop {
                    {
                        // Check for parameter changes
                        let mut restart_stream = false;
                        let mut new_params = false; // Params change that doesn't require stream reboot
                        while let Ok(msg) = control_receiver.try_recv() {
                            match msg {
                                HardwareDeviceRxStreamControlMessage::SetSampleRate(x) => {
                                    sample_rate = x;
                                    restart_stream = true;
                                }
                                HardwareDeviceRxStreamControlMessage::SetFrequency(x) => {
                                    frequency = x;
                                    device
                                        .set_frequency(
                                            soapysdr::Direction::Rx,
                                            stream_index,
                                            frequency,
                                            (),
                                        )
                                        .unwrap();
                                    new_params = true;
                                }
                                HardwareDeviceRxStreamControlMessage::SetBandwidth(x) => {
                                    bandwidth = x;
                                    device
                                        .set_bandwidth(
                                            soapysdr::Direction::Rx,
                                            stream_index,
                                            bandwidth,
                                        )
                                        .unwrap();
                                    new_params = true;
                                }
                                HardwareDeviceRxStreamControlMessage::SetGain(
                                    gain_name,
                                    gain_value,
                                ) => {
                                    device
                                        .set_gain_element(
                                            soapysdr::Direction::Rx,
                                            stream_index,
                                            gain_name.as_str(),
                                            gain_value,
                                        )
                                        .unwrap();
                                    new_params = true;
                                }
                                HardwareDeviceRxStreamControlMessage::Shutdown => {
                                    break 'outer;
                                }
                            }
                        }
                        if restart_stream {
                            break 'middle;
                        } else if new_params {
                            break 'inner;
                        }
                    }

                    let stream_read = stream.read(
                        &mut [&mut buffer[buffer_ix..]],
                        (STREAM_READ_TIMEOUT * 1e6) as i64,
                    );
                    let time = Utc::now();

                    match stream_read {
                        Ok(len) => {
                            buffer_ix += len;
                            if buffer_ix >= buffer.len() {
                                let iq_data = mem::replace(
                                    &mut buffer,
                                    vec![num_complex::Complex::<f32>::ZERO; buffer_size],
                                );
                                stream_sender
                                    .send(ReceiveStreamChunk { iq_data, time })
                                    .unwrap();
                                buffer_ix = 0;
                            }
                        }
                        Err(e) => {
                            warn!("Error reading from stream: {e:?}");
                            break 'middle; // Reboot the stream
                        }
                    }
                } // 'inner
            } // 'middle
            info!("Closing stream");
            stream.deactivate(None).ok();
        } // 'outer

        info!("Stopping thread for RX stream {stream_index:?} on device {device_id:?}");
    }
}

struct HardwareDeviceTxStream {
    _stream_index: usize,
    _device: soapysdr::Device,
}

impl HardwareDeviceTxStream {
    fn new(stream_index: usize, device: soapysdr::Device) -> Self {
        Self {
            _stream_index: stream_index,
            _device: device,
        }
    }
    fn update(&mut self, _params: &mut HardwareDeviceTxStreamParams) {}
}

// HELPER FUNCTIONS //

fn compute_range_min_max(ranges: &[soapysdr::Range]) -> Option<(f64, f64)> {
    if ranges.is_empty() {
        return None;
    }
    let min = ranges
        .iter()
        .map(|r| r.minimum)
        .fold(f64::INFINITY, f64::min);
    let max = ranges
        .iter()
        .map(|r| r.maximum)
        .fold(f64::NEG_INFINITY, f64::max);
    Some((min, max))
}

fn snap_to_range(range: &soapysdr::Range, mut value: f64) -> f64 {
    // Snap to the nearest discrete step if stepsize is non-zero
    if range.step > 0.0 {
        let steps_from_min = (value - range.minimum) / range.step;
        let rounded_steps = steps_from_min.round();
        value = range.minimum + (rounded_steps * range.step);
    }

    // Clamp to the desired range
    value.clamp(range.minimum, range.maximum)
}

fn snap_to_ranges(ranges: &[soapysdr::Range], value: f64) -> f64 {
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

    snap_to_range(closest_range, value)
}
