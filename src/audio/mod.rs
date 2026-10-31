//! audio - Audio capture, playback, and codec library
//!
//! Replaces the external C++ sound_app process with an integrated Rust library.
//! Uses ALSA for audio I/O, Opus for encoding/decoding, and SpeexDSP
//! for noise suppression, AGC, and resampling.

mod alsa_device;
mod audio_system;
mod opus_codec;
mod play;
mod record;
mod speex;
mod station_capture;
mod station_playback;
pub mod stream_decoder;

pub use audio_system::{AudioConfig, AudioSystem};
pub use station_capture::capture_thread as station_capture_thread;
pub use station_playback::playback_thread as station_playback_thread;
pub use stream_decoder::StreamDecoder;
