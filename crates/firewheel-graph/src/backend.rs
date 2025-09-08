use core::error::Error;
use core::time::Duration;

#[cfg(not(feature = "std"))]
use bevy_platform::prelude::{String, Vec};

use firewheel_core::{node::StreamStatus, StreamInfo};

use crate::processor::FirewheelProcessor;

/// A trait describing an audio backend.
///
/// When an instance is dropped, then it must automatically stop its
/// corresponding audio stream.
///
/// All methods in this trait are only ever invoked from the main
/// thread (the thread where the [`crate::context::FirewheelCtx`]
/// lives).
pub trait AudioBackend: Sized {
    /// The configuration of the audio stream.
    type Config;
    /// An error when starting a new audio stream.
    type StartStreamError: Error;
    /// An error that has caused the audio stream to stop.
    type StreamError: Error;

    /// A type describing an instant in time.
    type Instant: Send + Clone;

    type ProcessorEvent;

    /// Return a list of the available input devices.
    fn available_input_devices() -> Vec<DeviceInfo> {
        Vec::new()
    }
    /// Return a list of the available output devices.
    fn available_output_devices() -> Vec<DeviceInfo> {
        Vec::new()
    }

    /// Start the audio stream with the given configuration, and return
    /// a handle for the audio stream.
    fn start_stream(config: Self::Config) -> Result<(Self, StreamInfo), Self::StartStreamError>;

    /// Send the given processor to the audio thread for processing.
    fn set_processor(&mut self, processor: FirewheelProcessor<Self, Self::ProcessorEvent>);

    /// Poll the status of the running audio stream. Return an error if the
    /// audio stream has stopped for any reason.
    fn poll_status(&mut self) -> Result<(), Self::StreamError>;

    /// Return the amount of time that has elapsed from the instant
    /// [`FirewheelProcessor::process_interleaved`] was last called and now.
    ///
    /// The given `process_timestamp` is the `Self::Instant` that was passed
    /// to the latest call to [`FirewheelProcessor::process_interleaved`].
    /// This can be used to calculate the delay if needed.
    ///
    /// If for any reason the delay could not be determined, return `None`.
    fn delay_from_last_process(&self, process_timestamp: Self::Instant) -> Option<Duration>;
}

/// Information about an audio device.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub num_channels: u16,
    pub is_default: bool,
}

pub struct BackendProcessInfo<B: AudioBackend> {
    pub num_in_channels: usize,
    pub num_out_channels: usize,
    pub frames: usize,
    pub process_timestamp: B::Instant,
    pub duration_since_stream_start: Duration,
    pub input_stream_status: StreamStatus,
    pub output_stream_status: StreamStatus,
    pub dropped_frames: u32,
}
