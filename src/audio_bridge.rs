use crate::config::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use crate::audio::{AudioConfig, AudioSystem};

pub enum AudioEvent {
    AudioData(Vec<u8>),
}

/// Downlink audio params declared by the server hello (sample_rate, channels).
/// NetLink writes them on hello; the station playback thread reads them.
pub type PlaybackParams = Arc<RwLock<Option<(u32, u32)>>>;

pub struct AudioBridge {
    _audio_system: Option<AudioSystem>,
    play_tx: mpsc::Sender<Vec<u8>>,
}

impl AudioBridge {
    /// Start the audio backend selected by `audio_mode`:
    /// - "alsa"    — integrated ALSA record/playback threads (host backend);
    /// - "station" — Quasar vqe_0 tap over a unix datagram socket for capture
    ///               and `maind --gst-launch` WAV playback on the station.
    pub fn start(config: &Config, tx: mpsc::Sender<AudioEvent>, playback_params: PlaybackParams) -> anyhow::Result<Self> {
        let audio_config = AudioConfig {
            capture_device: config.capture_device.to_string(),
            playback_device: config.playback_device.to_string(),
            sample_rate: config.hello_sample_rate,
            channels: 2, // ALSA device typically 2 channels
            opus_sample_rate: config.hello_sample_rate,
            opus_channels: config.hello_channels as u32,
            opus_bitrate: 64000,
            encode_frame_duration_ms: 20,
            decode_frame_duration_ms: config.hello_frame_duration,
            stream_format: config.stream_format.as_str().to_string(),
            playback_sample_rate: config.playback_sample_rate,
            playback_channels: config.playback_channels,
            playback_period_size: config.playback_period_size,
        };

        log::info!(
            "AudioBridge: mode={}, capture=\"{}\", playback=\"{}\"",
            config.audio_mode,
            audio_config.capture_device, audio_config.playback_device,
        );

        let (opus_tx, mut opus_rx) = mpsc::channel::<Vec<u8>>(100);
        let (play_tx, play_rx) = mpsc::channel::<Vec<u8>>(400);

        // Forward recorded (encoded) audio as AudioEvent — common to both backends.
        tokio::spawn(async move {
            while let Some(data) = opus_rx.recv().await {
                if tx.send(AudioEvent::AudioData(data)).await.is_err() {
                    break;
                }
            }
        });

        if config.audio_mode == "station" {
            let running = Arc::new(AtomicBool::new(true));
            let socket_path = config.station_capture_socket.clone();
            let wav_dir = std::env::current_dir()?;
            {
                let running = running.clone();
                let cfg = audio_config.clone();
                std::thread::Builder::new()
                    .name("station-capture".into())
                    .spawn(move || {
                        if let Err(e) = crate::audio::station_capture_thread(&cfg, &socket_path, opus_tx, &running) {
                            log::error!("Station capture thread error: {}", e);
                        }
                    })?;
            }
            {
                let running = running.clone();
                let cfg = audio_config.clone();
                std::thread::Builder::new()
                    .name("station-playback".into())
                    .spawn(move || {
                        if let Err(e) = crate::audio::station_playback_thread(&cfg, play_rx, &running, wav_dir, playback_params) {
                            log::error!("Station playback thread error: {}", e);
                        }
                    })?;
            }
            return Ok(Self { _audio_system: None, play_tx });
        }

        let audio_system = AudioSystem::start(audio_config, opus_tx, play_rx)?;

        Ok(Self {
            _audio_system: Some(audio_system),
            play_tx,
        })
    }

    /// Send an Opus packet for playback.
    pub async fn send_audio(&self, data: &[u8]) -> anyhow::Result<()> {
        self.play_tx
            .send(data.to_vec())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send audio for playback: {}", e))
    }
}
