//! Station playback backend: decodes incoming Opus to PCM, buffers ~2 s
//! phrases into WAV files and plays them through the stock media daemon
//! (`maind --gst-launch`), which owns the speaker amplifier on the station.
//!
//! Direct ALSA playback is not viable on the station: the dmix shm segment
//! belongs to the 32-bit maind and its layout differs on aarch64, and the
//! FS2105 amplifier is managed by maind itself (see reports/stage-D-playback.md).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use anyhow::Result;

use crate::audio_bridge::{AudioEvent, PlayCommand};

use super::opus_codec::OpusDecoder;
use super::audio_system::AudioConfig;

const MAIND: &str = "/system/vendor/quasar/maind";
const MAIND_ENV_LD: &str = "/system/vendor/quasar:libs";
const MAIND_DIR: &str = "/system/vendor/quasar";
/// Seconds of PCM (at the decode rate) buffered before one gst-launch call.
const FLUSH_SECONDS: f32 = 2.0;
const WAV_SLOTS: u32 = 2;

fn write_wav(path: &std::path::Path, samples: &[i16], rate: u32) -> Result<()> {
    let data_len = samples.len() * 2;
    let mut f = std::fs::File::create(path)?;
    use std::io::Write;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len as u32).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 2).to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&(data_len as u32).to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

fn play_with_maind(wav: &std::path::Path) -> Result<()> {
    let pipeline = format!(
        "filesrc location={} ! wavparse ! audioconvert ! audioresample ! alsasink",
        wav.display()
    );
    let status = Command::new(MAIND)
        .arg("--gst-launch")
        .arg(&pipeline)
        .env("LD_LIBRARY_PATH", MAIND_ENV_LD)
        .current_dir(MAIND_DIR)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn maind --gst-launch: {}", e))?;
    if !status.success() {
        anyhow::bail!("maind --gst-launch exited with {}", status);
    }
    Ok(())
}

pub fn playback_thread(
    config: &AudioConfig,
    mut rx: mpsc::Receiver<Vec<u8>>,
    running: &AtomicBool,
    wav_dir: PathBuf,
    playback_params: Arc<RwLock<Option<(u32, u32)>>>,
    mut cmd_rx: mpsc::Receiver<PlayCommand>,
    event_tx: mpsc::Sender<AudioEvent>,
    rt: tokio::runtime::Handle,
) -> Result<()> {
    // Downlink params come from the server hello (sample_rate, channels).
    // Fall back to the local hello config until the server hello arrives.
    let mut cur: (u32, u32) = (config.opus_sample_rate, config.opus_channels);
    let mut decoder: Option<OpusDecoder> = None;
    let mut pcm: Vec<i16> = Vec::new();
    let mut seq: u32 = 0;
    let mut playing = false;
    log::info!(
        "Station playback: maind --gst-launch, flush {}ms, opus {}Hz/{}ch (awaiting server hello)",
        (FLUSH_SECONDS * 1000.0) as u32,
        config.opus_sample_rate,
        config.opus_channels
    );

    while running.load(Ordering::Relaxed) {
        // Pick up server-declared downlink params (re-create decoder on change)
        let want = playback_params
            .read()
            .ok()
            .and_then(|g| *g)
            .unwrap_or((config.opus_sample_rate, config.opus_channels));
        if decoder.is_none() || cur != want {
            if cur != want {
                log::info!("Downlink params: {}Hz/{}ch", want.0, want.1);
                pcm.clear();
            }
            decoder = Some(OpusDecoder::new(
                want.0,
                want.1,
                config.decode_frame_duration_ms,
                want.0,
                1,
            )?);
            cur = want;
        }

        // Ждём пакет или команду (с таймаутом, чтобы проверять running-флаг)
        enum Action {
            Timeout,
            Flush,
            Packet(Vec<u8>),
            Closed,
        }
        let action = rt.block_on(async {
            let sleep = tokio::time::sleep(std::time::Duration::from_millis(200));
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => Action::Timeout,
                cmd = cmd_rx.recv() => match cmd {
                    Some(PlayCommand::Flush) => Action::Flush,
                    None => Action::Closed,
                },
                pkt = rx.recv() => match pkt {
                    Some(data) => Action::Packet(data),
                    None => Action::Closed,
                },
            }
        });

        let mut got_cmd = false;
        match action {
            Action::Timeout => continue, // вернуться в начало (running/params)
            Action::Flush => got_cmd = true,
            Action::Packet(data) => {
                if let Some(d) = decoder.as_mut() {
                    match d.decode(&data) {
                        Ok(samples) => pcm.extend_from_slice(&samples),
                        Err(e) => log::error!("Audio decode error: {}", e),
                    }
                }
                while let Ok(data) = rx.try_recv() {
                    if let Some(d) = decoder.as_mut() {
                        match d.decode(&data) {
                            Ok(samples) => pcm.extend_from_slice(&samples),
                            Err(e) => log::error!("Audio decode error: {}", e),
                        }
                    }
                }
            }
            Action::Closed => {
                log::info!("Playback channel closed");
                break;
            }
        }

        let flush_samples = (cur.0 as f32 * FLUSH_SECONDS) as usize;
        let tail = got_cmd && !pcm.is_empty();
        if pcm.len() >= flush_samples || tail {
            seq = seq % WAV_SLOTS + 1;
            let wav = wav_dir.join(format!("tts_{}.wav", seq));
            if let Err(e) = write_wav(&wav, &pcm, cur.0) {
                log::error!("WAV write failed: {}", e);
            } else if let Err(e) = play_with_maind(&wav) {
                log::error!("maind playback failed: {}", e);
            }
            pcm.clear();
        }
        if got_cmd {
            // maind закончил — уведомить контроллер (он откроет прослушку)
            rt.block_on(async {
                let _ = event_tx.send(AudioEvent::PlaybackFinished).await;
            });
        }
    }
    log::info!("Station playback stopped");
    Ok(())
}
