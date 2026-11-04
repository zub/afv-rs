//! Audio device I/O through `cpal` (feature `cpal`), on top of [`crate::realtime`]: device
//! callbacks only move samples through ring buffers, so decoding, mixing, encoding and network
//! sends never happen on a real-time audio thread.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use tokio::sync::broadcast;

use crate::ConvertError;
use crate::audio::Audio;
use crate::events::Event;
use crate::radio::OutputBus;
use crate::realtime::{self, Bridge, Capture, Format, Playback};

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("no {0} audio device found")]
    NotFound(String),
    #[error("audio device: {0}")]
    Cpal(#[from] cpal::Error),
    #[error("unsupported sample format {0}")]
    Format(SampleFormat),
    #[error(transparent)]
    UnsupportedFormat(#[from] ConvertError),
}

/// Which device to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChoice {
    /// The system default.
    Default,
    /// A device by id or name, as listed by [`input_devices`] and [`output_devices`].
    Named(String),
}

/// The devices to attach. `None` leaves that part unused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceConfig {
    /// Microphone; without one, nothing is transmitted.
    pub input: Option<DeviceChoice>,
    pub headset: Option<DeviceChoice>,
    pub speaker: Option<DeviceChoice>,
}

impl DeviceConfig {
    /// The default microphone and output device, with the output as the headset bus.
    pub fn defaults() -> Self {
        Self { input: Some(DeviceChoice::Default), headset: Some(DeviceChoice::Default), speaker: None }
    }
}

/// A device that [`DeviceChoice::Named`] can refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

fn list(devices: Result<impl Iterator<Item = cpal::Device>, cpal::Error>) -> Result<Vec<DeviceInfo>, DeviceError> {
    Ok(devices?.filter_map(|d| Some(DeviceInfo { id: d.id().ok()?.to_string(), name: d.to_string() })).collect())
}

pub fn input_devices() -> Result<Vec<DeviceInfo>, DeviceError> {
    list(cpal::default_host().input_devices())
}

pub fn output_devices() -> Result<Vec<DeviceInfo>, DeviceError> {
    list(cpal::default_host().output_devices())
}

fn find(choice: &DeviceChoice, input: bool) -> Result<cpal::Device, DeviceError> {
    let host = cpal::default_host();
    let kind = if input { "input" } else { "output" };
    let found = match choice {
        DeviceChoice::Default => {
            if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            }
        }
        DeviceChoice::Named(wanted) => {
            let devices = if input { host.input_devices()? } else { host.output_devices()? };
            devices.into_iter().find(|d| d.id().is_ok_and(|id| id.to_string() == *wanted) || d.to_string() == *wanted)
        }
    };
    found.ok_or_else(|| {
        DeviceError::NotFound(match choice {
            DeviceChoice::Default => format!("default {kind}"),
            DeviceChoice::Named(name) => format!("{kind} {name:?}"),
        })
    })
}

/// Attached devices. Dropping this stops them.
pub struct Devices {
    // Streams first: they must stop calling back before the bridge goes.
    _streams: Vec<cpal::Stream>,
    _bridge: Bridge,
}

/// Opens the devices and starts feeding `audio`.
pub fn attach(audio: &Audio, config: DeviceConfig) -> Result<Devices, DeviceError> {
    let input = config.input.as_ref().map(|c| find(c, true)).transpose()?;
    let input_config = input.as_ref().map(|d| d.default_input_config()).transpose()?;
    let mut outputs = Vec::new();
    for (bus, choice) in [(OutputBus::Headset, &config.headset), (OutputBus::Speaker, &config.speaker)] {
        if let Some(choice) = choice {
            let device = find(choice, false)?;
            let supported = device.default_output_config()?;
            outputs.push((bus, device, supported));
        }
    }
    let format = |c: &cpal::SupportedStreamConfig| Format { sample_rate: c.sample_rate(), channels: c.channels() };
    let formats: Vec<_> = outputs.iter().map(|(bus, _, c)| (*bus, format(c))).collect();
    let (bridge, capture, playback) = realtime::bridge(audio, input_config.as_ref().map(format), &formats)?;

    // Device errors are reported straight to the event stream: the callback that reports them
    // must not wait for the audio engine.
    let events = audio.event_sender();
    let mut streams = Vec::new();
    if let (Some(device), Some(supported), Some(capture)) = (&input, &input_config, capture) {
        streams.push(build_input(device, supported, capture, &events)?);
    }
    for ((_, device, supported), playback) in outputs.iter().zip(playback) {
        streams.push(build_output(device, supported, playback, &events)?);
    }
    for stream in &streams {
        stream.play()?;
    }
    Ok(Devices { _streams: streams, _bridge: bridge })
}

fn report(events: &broadcast::Sender<Event>) -> impl FnMut(cpal::Error) + Send + 'static {
    let events = events.clone();
    move |e| {
        tracing::warn!("audio device: {e}");
        let _ = events.send(Event::DeviceError(e.to_string()));
    }
}

/// Calls `build::<T>(args…)` with `T` the sample type of `format`.
macro_rules! with_sample_type {
    ($format:expr, $build:ident($($arg:expr),*)) => {
        match $format {
            SampleFormat::F32 => $build::<f32>($($arg),*),
            SampleFormat::F64 => $build::<f64>($($arg),*),
            SampleFormat::I16 => $build::<i16>($($arg),*),
            SampleFormat::I32 => $build::<i32>($($arg),*),
            SampleFormat::U16 => $build::<u16>($($arg),*),
            SampleFormat::U8 => $build::<u8>($($arg),*),
            other => Err(DeviceError::Format(other)),
        }
    };
}

fn build_input(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    capture: Capture,
    events: &broadcast::Sender<Event>,
) -> Result<cpal::Stream, DeviceError> {
    fn build<T: SizedSample>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        mut capture: Capture,
        events: &broadcast::Sender<Event>,
    ) -> Result<cpal::Stream, DeviceError>
    where
        f32: FromSample<T>,
    {
        let data = move |data: &[T], _: &cpal::InputCallbackInfo| capture.push_map(data, |s| s.to_sample());
        Ok(device.build_input_stream(config, data, report(events), None)?)
    }
    with_sample_type!(supported.sample_format(), build(device, supported.config(), capture, events))
}

fn build_output(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    playback: Playback,
    events: &broadcast::Sender<Event>,
) -> Result<cpal::Stream, DeviceError> {
    fn build<T: SizedSample + FromSample<f32>>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        mut playback: Playback,
        events: &broadcast::Sender<Event>,
    ) -> Result<cpal::Stream, DeviceError> {
        let data = move |data: &mut [T], _: &cpal::OutputCallbackInfo| playback.fill_map(data, T::from_sample);
        Ok(device.build_output_stream(config, data, report(events), None)?)
    }
    with_sample_type!(supported.sample_format(), build(device, supported.config(), playback, events))
}
