//! Station capture backend: reads the Quasar vqe_0 tap over a unix datagram
//! socket instead of opening ALSA directly.
//!
//! The vqe_tap LD_PRELOAD module (see reports/stage-E-vqe-mic.md) forwards the
//! processed VQE output as datagrams of up to 160 mono samples (10 ms @ 16 kHz,
//! s16le). Samples go through the same Speex preprocess + Opus encode pipeline
//! as the ALSA record thread.

use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use anyhow::Result;

use super::opus_codec::OpusEncoder;
use super::speex::Preprocessor;
use super::audio_system::AudioConfig;

/// 10 ms of mono 16 kHz per datagram from the vqe_tap module.
const TAP_CHUNK_SAMPLES: usize = 160;
const TAP_SAMPLE_RATE: u32 = 16000;

pub fn capture_thread(
    config: &AudioConfig,
    socket_path: &str,
    opus_tx: mpsc::Sender<Vec<u8>>,
    running: &AtomicBool,
) -> Result<()> {
    // Убираем осиротевший сокет после падения прошлой сессии, иначе bind
    // вернёт "Address already in use".
    let _ = std::fs::remove_file(socket_path);
    let socket = UnixDatagram::bind(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot bind capture socket {}: {}", socket_path, e))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    log::info!("Station capture: listening on {}", socket_path);

    let mut preprocessor = Preprocessor::new(TAP_CHUNK_SAMPLES, TAP_SAMPLE_RATE)?;
    preprocessor.set_denoise(true);
    preprocessor.set_noise_suppress(-25);
    preprocessor.set_agc(true);
    preprocessor.set_agc_level(24000.0);

    let mut encoder = OpusEncoder::new(
        TAP_SAMPLE_RATE,
        1,
        config.encode_frame_duration_ms,
        config.opus_sample_rate,
        config.opus_channels,
        config.opus_bitrate,
    )?;
    let frame_samples = encoder.input_frame_samples();
    let mut accum: Vec<i16> = Vec::with_capacity(frame_samples * 2);
    let mut buf = vec![0u8; 4096];

    while running.load(Ordering::Relaxed) {
        let len = match socket.recv_from(&mut buf) {
            Ok((len, _)) => len,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                log::warn!("station capture recv error: {}", e);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if len < 2 {
            continue;
        }
        let samples: Vec<i16> = buf[..len]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        // Speex preprocess expects exactly TAP_CHUNK_SAMPLES per call.
        for chunk in samples.chunks(TAP_CHUNK_SAMPLES) {
            if chunk.len() == TAP_CHUNK_SAMPLES {
                let mut frame = chunk.to_vec();
                preprocessor.process(&mut frame);
                accum.extend_from_slice(&frame);
            } else {
                accum.extend_from_slice(chunk);
            }
        }

        while accum.len() >= frame_samples {
            match encoder.encode(&accum[..frame_samples]) {
                Ok(opus_data) => {
                    if !opus_data.is_empty() && opus_tx.blocking_send(opus_data).is_err() {
                        log::warn!("station capture: receiver dropped");
                        return Ok(());
                    }
                }
                Err(e) => log::error!("Opus encode error: {}", e),
            }
            accum.drain(..frame_samples);
        }
    }
    log::info!("Station capture stopped");
    Ok(())
}
