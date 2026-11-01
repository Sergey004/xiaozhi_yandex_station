//! LED ring control for the Yandex Station (Plum).
//!
//! Drives the 9 sysfs LEDs `plum:{red,green,blue}:ring{1,2,3}` with state
//! colors (idle/listening/processing/speaking), with the safety semantics
//! learned from the community indicator scripts, adapted to this device:
//!
//! - the ring is normally owned by the firmware trigger `led_ring_fb`;
//!   on acquire we detach it and restore it on release;
//! - the physical mic mute switch is read from `/sys/kernel/debug/gpio`;
//!   on this firmware the polarity is INVERTED vs the community reference:
//!   `hi` = mic ON, `lo` = MUTED. While muted we release the ring so the
//!   stock mute indication stays in charge;
//! - if the firmware paints a red warning frame while we own the ring, we
//!   back off and let it show;
//! - on exit the saved frame and triggers are always restored.
//! - session pid file in /tmp marks our ownership for orphan healing.

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// State codes published by the controller (see controller.rs).
pub const LED_STATE_IDLE: u8 = 0;
pub const LED_STATE_LISTENING: u8 = 1;
pub const LED_STATE_PROCESSING: u8 = 2;
pub const LED_STATE_SPEAKING: u8 = 3;

const LED_ROOT: &str = "/sys/class/leds";
/// The firmware ring trigger on Plum; restored when we hand the ring back.
const STOCK_TRIGGER: &str = "led_ring_fb";
const GPIO_DEBUG: &str = "/sys/kernel/debug/gpio";
/// 9 LEDs, painted in file order: (red,green,blue) x ring1..ring3.
const LED_PATHS: [&str; 9] = [
    "plum:red:ring1",
    "plum:green:ring1",
    "plum:blue:ring1",
    "plum:red:ring2",
    "plum:green:ring2",
    "plum:blue:ring2",
    "plum:red:ring3",
    "plum:green:ring3",
    "plum:blue:ring3",
];
/// Animation rate: 60 FPS (16.6ms per frame).
const FPS: u32 = 60;
/// Period for f32 time wrap (LCM of 0.9, 1.2, 1.6 = 14.4s).
const TIME_WRAP_SECS: f64 = 14.4;
/// Mute check interval: 150ms (~6.6 Hz) instead of every frame.
const MUTE_CHECK_MS: u64 = 150;
/// External repaint check interval: 200ms (5 Hz).
const EXTERNAL_CHECK_MS: u64 = 200;
/// Idle/mute loop sleep: 50ms (faster reaction).
const IDLE_SLEEP_MS: u64 = 50;

/// (r,g,b) per zone for each state code; the 9-LED frame repeats the zone
/// color across ring1..ring3.
const COLORS: [(u8, u8, u8); 4] = [
    (24, 24, 24),  // idle: dim white
    (0, 24, 160),  // listening: blue
    (120, 60, 0),  // processing/thinking: amber
    (0, 120, 24),  // speaking: green
];

#[derive(Clone)]
pub struct LedRing {
    state: Arc<AtomicU8>,
    running: Arc<AtomicBool>,
}

impl LedRing {
    /// Publish a new ring state (called from the controller).
    pub fn set_state(&self, code: u8) {
        self.state.store(code, Ordering::Relaxed);
    }

    /// Signal the paint thread to stop; the thread restores the stock frame
    /// and triggers before exiting.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Probe whether the Plum ring LEDs are present on this machine.
pub fn available() -> bool {
    fs::metadata(format!("{}/{}", LED_ROOT, LED_PATHS[0])).is_ok()
}

/// Spawn the LED paint thread.
pub fn spawn(state: Arc<AtomicU8>) -> anyhow::Result<LedRing> {
    if !available() {
        anyhow::bail!("plum ring LEDs not found under {}", LED_ROOT);
    }
    // Create pid file for orphan healing (survives SIGKILL of main process).
    let pid = std::process::id();
    let pid_path = format!("/tmp/xiaozhi_led_ring_{}.pid", pid);
    let _ = fs::write(&pid_path, pid.to_string());

    let running = Arc::new(AtomicBool::new(true));
    let running_t = running.clone();
    let state_t = state.clone();
    std::thread::Builder::new()
        .name("led-ring".into())
        .spawn(move || paint_loop(state_t, running_t, pid_path))?;
    Ok(LedRing { state, running })
}

struct Ownership {
    owned: bool,
    saved_frame: [u8; 9],
    saved_triggers: [String; 9],
}

fn paint_loop(state: Arc<AtomicU8>, running: Arc<AtomicBool>, pid_file: String) {
    let mut own = Ownership {
        owned: false,
        saved_frame: [0; 9],
        saved_triggers: ["none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into()],
    };
    let mut last_painted = String::new();
    let start = Instant::now();

    // Cached mute state, updated every MUTE_CHECK_MS.
    let mut muted = false;
    let mut mute_check = Instant::now();
    // External repaint check timer.
    let mut external_check = Instant::now();

    let mut dbg_timer = Instant::now();
    while running.load(Ordering::Relaxed) {
        let code = state.load(Ordering::Relaxed).min(LED_STATE_SPEAKING);

        // --- Cached mute check (point 1) ---
        if mute_check.elapsed() > Duration::from_millis(MUTE_CHECK_MS) {
            muted = mic_muted();
            mute_check = Instant::now();
        }

        if dbg_timer.elapsed() > Duration::from_secs(5) {
            dbg_timer = Instant::now();
            log::info!(
                "[LED-DBG] state={} muted={} owned={} thr0={}",
                code,
                muted,
                own.owned,
                read_trigger(LED_PATHS[0])
            );
        }

        // Ownership on demand: in idle or muted, return ring to firmware.
        if code == LED_STATE_IDLE || muted {
            release(&mut own, &last_painted);
            last_painted.clear();
            // Heal orphans (point 7): only if our pid file is gone (process dead).
            if !own.owned && read_trigger(LED_PATHS[0]) == "none" && !fs::metadata(&pid_file).is_ok() {
                for p in LED_PATHS.iter() {
                    let _ = fs::write(format!("{}/{}/trigger", LED_ROOT, p), format!("{}\n", STOCK_TRIGGER));
                }
                log::info!("LED ring: orphaned triggers restored to {}", STOCK_TRIGGER);
            }
            std::thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
            continue;
        }

        if !own.owned {
            // Respect an active red firmware warning before acquiring (point 6).
            let cur = read_frame();
            if cur[0] > 0 {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            own.saved_triggers = read_triggers();
            own.saved_frame = cur;
            detach_triggers();
            own.owned = true;
            log::info!("LED ring acquired (state {})", code);
        } else {
            // External repaint check (point 4): only every EXTERNAL_CHECK_MS.
            if external_check.elapsed() > Duration::from_millis(EXTERNAL_CHECK_MS) {
                let cur = read_frame();
                let cur_str = frame_string(&cur);
                if cur_str != last_painted {
                    // Point 6: if firmware painted red while we own, yield.
                    if cur[0] > 0 {
                        log::info!("LED ring: firmware red frame detected, yielding ownership");
                        release(&mut own, &last_painted);
                        last_painted.clear();
                        external_check = Instant::now();
                        continue;
                    }
                    own.saved_frame = cur;
                    last_painted = cur_str;
                }
                external_check = Instant::now();
            }
        }

        // --- Time base: f64 with modulo 14.4s (point 2) ---
        let t = (start.elapsed().as_secs_f64() % TIME_WRAP_SECS) as f32;
        let color = COLORS[code as usize];
        let mut f = [0u8; 9];
        let tau = std::f32::consts::TAU;
        match code {
            LED_STATE_LISTENING => {
                // Spinner: glow runs across 3 positions (900ms/rev),
                // adjacent positions phased by 120°.
                let period = 0.9f32;
                for pos in 0..3 {
                    let ang = (t % period) / period * tau - pos as f32 * (tau / 3.0);
                    let glow = ((ang.sin() + 1.0) / 2.0).powi(2);
                    f[pos * 3] = (color.0 as f32 * glow) as u8;
                    f[pos * 3 + 1] = (color.1 as f32 * glow) as u8;
                    f[pos * 3 + 2] = (color.2 as f32 * glow) as u8;
                }
            }
            LED_STATE_PROCESSING => {
                // Pulse: 1.2s cycle.
                let p = 0.5 + 0.5 * (t * tau / 1.2).sin();
                for ring in 0..3 {
                    f[ring * 3] = (color.0 as f32 * p) as u8;
                    f[ring * 3 + 1] = (color.1 as f32 * p) as u8;
                    f[ring * 3 + 2] = (color.2 as f32 * p) as u8;
                }
            }
            LED_STATE_SPEAKING => {
                // Breathing (point 3): smooth sine, 1.6s cycle.
                // 0.55 + 0.45 * (0.5 + 0.5 * sin) = 0.775 + 0.225 * sin
                let scale = 0.775 + 0.225 * (t * tau / 1.6).sin();
                for ring in 0..3 {
                    f[ring * 3] = (color.0 as f32 * scale) as u8;
                    f[ring * 3 + 1] = (color.1 as f32 * scale) as u8;
                    f[ring * 3 + 2] = (color.2 as f32 * scale) as u8;
                }
            }
            _ => {
                for ring in 0..3 {
                    f[ring * 3] = color.0;
                    f[ring * 3 + 1] = color.1;
                    f[ring * 3 + 2] = color.2;
                }
            }
        }

        // Point 4: write only changed channels.
        paint_frame_delta(&f, &last_painted);
        last_painted = frame_string(&f);
        std::thread::sleep(Duration::from_millis(1000 / FPS as u64));
    }

    // Cleanup pid file.
    let _ = fs::remove_file(&pid_file);
    release(&mut own, &last_painted);
    log::info!("LED ring released");
}

fn frame_string(f: &[u8; 9]) -> String {
    f.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_frame() -> [u8; 9] {
    let mut out = [0u8; 9];
    for (i, p) in LED_PATHS.iter().enumerate() {
        out[i] = fs::read_to_string(format!("{}/{}/brightness", LED_ROOT, p))
            .unwrap_or_else(|_| "0".into())
            .trim()
            .parse()
            .unwrap_or(0);
    }
    out
}

fn read_triggers() -> [String; 9] {
    let mut out: [String; 9] = Default::default();
    for (i, p) in LED_PATHS.iter().enumerate() {
        out[i] = read_trigger(p);
    }
    out
}

fn read_trigger(p: &str) -> String {
    let t = fs::read_to_string(format!("{}/{}/trigger", LED_ROOT, p))
        .unwrap_or_else(|_| "none".into());
    let t = t.trim().to_string();
    match (t.find('['), t.find(']')) {
        (Some(a), Some(b)) if b > a => t[a + 1..b].to_string(),
        _ => "none".to_string(),
    }
}

fn detach_triggers() {
    for p in LED_PATHS.iter() {
        let _ = fs::write(format!("{}/{}/trigger", LED_ROOT, p), "none\n");
    }
}

fn restore_triggers(saved: &[String; 9]) {
    for (p, saved_trig) in LED_PATHS.iter().zip(saved.iter()) {
        // Writing brightness=0 detaches a trigger; only re-attach when the
        // trigger is still ours ('none') and the firmware had one.
        if saved_trig != "none" && read_trigger(p) == "none" {
            let _ = fs::write(format!("{}/{}/trigger", LED_ROOT, p), format!("{}\n", saved_trig));
        }
    }
}

/// Write only LEDs whose brightness changed (point 4).
fn paint_frame_delta(new: &[u8; 9], last_painted: &str) {
    if last_painted.is_empty() {
        // First frame: write all.
        for (p, v) in LED_PATHS.iter().zip(new.iter()) {
            let _ = fs::write(format!("{}/{}/brightness", LED_ROOT, p), format!("{}\n", v));
        }
        return;
    }
    let prev: Vec<u8> = last_painted
        .split(' ')
        .filter_map(|s| s.parse().ok())
        .collect();
    if prev.len() != 9 {
        // Fallback: write all.
        for (p, v) in LED_PATHS.iter().zip(new.iter()) {
            let _ = fs::write(format!("{}/{}/brightness", LED_ROOT, p), format!("{}\n", v));
        }
        return;
    }
    for (i, (p, &v)) in LED_PATHS.iter().zip(new.iter()).enumerate() {
        if prev[i] != v {
            let _ = fs::write(format!("{}/{}/brightness", LED_ROOT, p), format!("{}\n", v));
        }
    }
}

fn release(own: &mut Ownership, last_painted: &str) {
    if !own.owned {
        return;
    }
    let cur = frame_string(&read_frame());
    // Only restore if the user/firmware did not repaint over our frame.
    if cur == last_painted {
        paint_frame(&own.saved_frame);
    }
    restore_triggers(&own.saved_triggers);
    own.owned = false;
}

fn paint_frame(f: &[u8; 9]) {
    for (p, v) in LED_PATHS.iter().zip(f.iter()) {
        let _ = fs::write(format!("{}/{}/brightness", LED_ROOT, p), format!("{}\n", v));
    }
}

/// Read the physical mic mute switch state (Plum: INVERTED polarity —
/// `hi` = mic ON, `lo` = MUTED; confirmed on firmware 3.210.369).
fn mic_muted() -> bool {
    let Ok(content) = fs::read_to_string(GPIO_DEBUG) else {
        return false; // no debugfs: assume unmuted, keep painting
    };
    for line in content.lines() {
        if let Some(idx) = line.find("mute-switch") {
            // e.g. " gpio-505 (                    |mute-switch         ) in  hi"
            return line[idx..].contains(" lo ");
        }
    }
    false
}
