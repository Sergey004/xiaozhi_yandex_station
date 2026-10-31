//! Station playback backend: streams decoded PCM into a persistent
//! `maind --gst-launch` pipeline (stdin → fdsrc → wavparse → alsasink).
//!
//! The WAV header declares an "infinite" data length, so wavparse plays the
//! pipe as it arrives: no chunk files, no spawn gap between phrases, natural
//! backpressure from the pipe. The pipeline starts lazily on the first
//! audio, stays alive across the session (silence is just a starved pipe)
//! and closes after an idle timeout or a downlink-rate change.
//!
//! maind owns the FS2105 amplifier — direct ALSA from this process is not
//! viable (see reports/stage-D-playback.md).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use anyhow::Result;

use super::opus_codec::OpusDecoder;
use super::audio_system::AudioConfig;

use crate::audio_bridge::{AudioEvent, PlayCommand};

const MAIND: &str = "/system/vendor/quasar/maind";
const MAIND_ENV_LD: &str = "/system/vendor/quasar:libs";
const MAIND_DIR: &str = "/system/vendor/quasar";
/// How long a starved (silent) pipeline stays open before it is closed.
const IDLE_CLOSE_SECS: u64 = 30;
/// Silence tail written on tts/stop so the pipeline's internal buffers
/// drain audibly before the mic re-opens.
const TAIL_SILENCE_MS: u32 = 300;
/// Approximate pipeline drain time after the last byte is written.
const DRAIN_WAIT_MS: u64 = 700;
/// Pause before re-spawning maind after a failed playback.
const RESPAWN_BACKOFF_MS: u64 = 2000;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

fn wav_stream_header(sample_rate: u32, channels: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // infinite stream
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
    h.extend_from_slice(&(channels * 2).to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&0xFFFFFFFEu32.to_le_bytes());
    h
}

fn spawn_maind_pipeline() -> Result<Child> {
    Command::new(MAIND)
        .arg("--gst-launch")
        .arg("fdsrc fd=0 ! wavparse ! audioconvert ! audioresample ! alsasink")
        .env("LD_LIBRARY_PATH", MAIND_ENV_LD)
        .current_dir(MAIND_DIR)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn maind --gst-launch: {}", e))
}

/// Persistent streaming player over one maind pipeline.
struct StreamPlayer {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    sample_rate: u32,
    channels: u16,
}

impl StreamPlayer {
    fn start(sample_rate: u32, channels: u16) -> Result<Self> {
        let mut child = spawn_maind_pipeline()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("maind stdin not captured"))?;
        let mut player = StreamPlayer { child, stdin: Some(stdin), sample_rate, channels };
        player.write_header()?;
        Ok(player)
    }

    fn write_header(&mut self) -> Result<()> {
        let header = wav_stream_header(self.sample_rate, self.channels);
        self.stdin
            .as_mut()
            .expect("pipeline stdin closed")
            .write_all(&header)
            .map_err(|e| anyhow::anyhow!("WAV header write failed: {}", e))
    }

    fn write_pcm(&mut self, pcm: &[i16]) -> Result<()> {
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.stdin
            .as_mut()
            .expect("pipeline stdin closed")
            .write_all(&bytes)
            .map_err(|e| anyhow::anyhow!("PCM write failed: {}", e))
    }

    fn write_silence_ms(&mut self, ms: u32) -> Result<()> {
        let samples = (self.sample_rate as usize * ms as usize) / 1000;
        let silence = vec![0i16; samples];
        self.write_pcm(&silence)
    }

    /// Close stdin: the pipeline drains and maind exits.
    fn close(&mut self) {
        if let Some(si) = self.stdin.as_mut() {
            let _ = si.flush();
        }
        self.stdin = None;
        let _ = self.child.wait();
        let _ = self.child.kill();
    }
}

pub fn playback_thread(
    config: &AudioConfig,
    mut rx: mpsc::Receiver<Vec<u8>>,
    running: &AtomicBool,
    _wav_dir: PathBuf,
    playback_params: Arc<RwLock<Option<(u32, u32)>>>,
    mut cmd_rx: mpsc::Receiver<PlayCommand>,
    event_tx: mpsc::Sender<AudioEvent>,
    rt: tokio::runtime::Handle,
) -> Result<()> {
    let mut cur: (u32, u16) = (config.opus_sample_rate, 1);
    let mut decoder: Option<OpusDecoder> = None;
    let mut player: Option<StreamPlayer> = None;
    let mut last_activity = Instant::now();
    let mut pcm: Vec<i16> = Vec::new();
    let mut consecutive_failures: u32 = 0;
    // tts/stop получен, но последние кадры могут прийти позже: ждём их и
    // доигрываем, прежде чем уведомить контроллер о конце проигрывания.
    let mut pending_finish = false;
    log::info!(
        "Station playback: streaming mode (persistent maind pipeline), opus {}Hz/{}ch",
        config.opus_sample_rate,
        config.opus_channels
    );

    while running.load(Ordering::Relaxed) {
        // Per-device downlink params (server hello); a rate change re-spawns
        // the pipeline with the matching header.
        let want = playback_params
            .read()
            .ok()
            .and_then(|g| *g)
            .unwrap_or((config.opus_sample_rate, 1));
        let want = (want.0, want.1 as u16);
        if decoder.is_none() || cur != want {
            if cur != want {
                log::info!("Downlink params: {}Hz/{}ch", want.0, want.1);
                pcm.clear();
            }
            decoder = Some(OpusDecoder::new(
                want.0,
                want.1 as u32,
                config.decode_frame_duration_ms,
                want.0,
                1,
            )?);
            cur = want;
            if let Some(p) = player.as_mut() {
                p.close();
            }
            player = None;
        }

        // Idle close: a starved pipeline is shut down to release the amp.
        let idle_expired = player
            .as_ref()
            .is_some_and(|_| last_activity.elapsed() > Duration::from_secs(IDLE_CLOSE_SECS));
        if idle_expired {
            log::info!("Playback idle {}s, closing pipeline", IDLE_CLOSE_SECS);
            if let Some(p) = player.as_mut() {
                p.close();
            }
            player = None;
        }

        // Wait for a packet or a command (timeout keeps the idle check alive)
        enum Action {
            Timeout,
            Flush,
            Packet(Vec<u8>),
            Closed,
        }
        let action = rt.block_on(async {
            let sleep = tokio::time::sleep(Duration::from_millis(200));
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

        let mut finish_reply = false;
        match action {
            Action::Timeout => {
                if pending_finish && pcm.is_empty() {
                    rt.block_on(async {
                        let _ = event_tx.send(AudioEvent::PlaybackFinished).await;
                    });
                    pending_finish = false;
                    log::info!("Playback finished (drained)");
                }
                continue;
            }
            Action::Flush => {
                finish_reply = true;
                pending_finish = true;
            }
            Action::Closed => {
                log::info!("Playback channel closed");
                break;
            }
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
        }

        // Write everything decoded straight into the pipeline
        if !pcm.is_empty() {
            if player.is_none() {
                if consecutive_failures > 0 {
                    std::thread::sleep(Duration::from_millis(RESPAWN_BACKOFF_MS));
                }
                match StreamPlayer::start(cur.0, cur.1) {
                    Ok(p) => {
                        log::info!(
                            "Streaming pipeline started ({}Hz/{}ch)",
                            cur.0, cur.1
                        );
                        player = Some(p);
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        log::error!(
                            "Pipeline start failed ({}): {}",
                            consecutive_failures, e
                        );
                        pcm.clear();
                        continue;
                    }
                }
            }
            let p = player.as_mut().unwrap();
            if let Err(e) = p.write_pcm(&pcm) {
                consecutive_failures += 1;
                log::error!("Pipeline write failed ({}): {}", consecutive_failures, e);
                p.close();
                player = None;
                pcm.clear();
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    log::error!("Pipeline keeps failing, dropping buffered audio");
                }
                continue;
            }
            consecutive_failures = 0;
            last_activity = Instant::now();
            pcm.clear();
        }

        // tts/stop: дописать хвост тишины (проветрить внутренние буферы
        // пайплайна) и уведомить контроллер после примерного drain.
        if finish_reply {
            if let Some(p) = player.as_mut() {
                let _ = p.write_silence_ms(TAIL_SILENCE_MS);
                last_activity = Instant::now();
            }
            if player.is_some() {
                last_activity = Instant::now();
            }
        }
    }

    if let Some(p) = player.as_mut() {
        p.close();
    }
    log::info!("Station playback stopped");
    Ok(())
}
