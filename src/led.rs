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

use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
/// Animation frames per second.
const FPS: u32 = 10;

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
    let running = Arc::new(AtomicBool::new(true));
    let running_t = running.clone();
    let state_t = state.clone();
    std::thread::Builder::new()
        .name("led-ring".into())
        .spawn(move || paint_loop(state_t, running_t))?;
    Ok(LedRing { state, running })
}

struct Ownership {
    owned: bool,
    saved_frame: [u8; 9],
    saved_triggers: [String; 9],
}

fn paint_loop(state: Arc<AtomicU8>, running: Arc<AtomicBool>) {
    let mut own = Ownership {
        owned: false,
        saved_frame: [0; 9],
        saved_triggers: ["none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into(), "none".into()],
    };
    let mut last_painted = String::new();
    let mut frame: u64 = 0;

    let mut dbg_timer = std::time::Instant::now();
    while running.load(Ordering::Relaxed) {
        let code = state.load(Ordering::Relaxed).min(LED_STATE_SPEAKING);
        if dbg_timer.elapsed() > Duration::from_secs(5) {
            dbg_timer = std::time::Instant::now();
            log::info!(
                "[LED-DBG] state={} muted={} owned={} thr0={}",
                code,
                mic_muted(),
                own.owned,
                read_trigger(LED_PATHS[0])
            );
        }

        // Ownership on demand: в idle кольцо возвращается прошивке (Алиса
        // рисует свои индикации через led_ring_fb), забираем только на
        // активных состояниях. Физический мьют (инвертирован: lo = mute)
        // тоже отдаёт кольцо штатной индикации.
        if code == LED_STATE_IDLE || mic_muted() {
            release(&mut own, &last_painted);
            last_painted.clear();
            // Heal orphans: a previous run killed with SIGKILL left the
            // triggers detached - hand the ring back to the firmware.
            if !own.owned && read_trigger(LED_PATHS[0]) == "none" {
                for p in LED_PATHS.iter() {
                    let _ = fs::write(format!("{}/{}/trigger", LED_ROOT, p), format!("{}\n", STOCK_TRIGGER));
                }
                log::info!("LED ring: orphaned triggers restored to {}", STOCK_TRIGGER);
            }
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        if !own.owned {
            // Respect an active red firmware warning before acquiring.
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
            // External repaint while owned: adopt it as the frame to restore.
            let cur = read_frame();
            if frame_string(&cur) != last_painted {
                own.saved_frame = cur;
            }
        }

        let color = COLORS[code as usize];
        let scale = if code == LED_STATE_IDLE {
            1.0
        } else {
            0.75 + 0.25 * ((frame as f32 / FPS as f32) * std::f32::consts::PI).sin().abs()
        };
        let f = [
            (color.0 as f32 * scale) as u8,
            (color.1 as f32 * scale) as u8,
            (color.2 as f32 * scale) as u8,
        ];
        // Frame order: r1,g1,b1,r2,g2,b2,r3,g3,b3 — same zone across rings.
        let painted = [f[0], f[1], f[2], f[0], f[1], f[2], f[0], f[1], f[2]];
        paint_frame(&painted);
        last_painted = frame_string(&painted);
        frame += 1;
        std::thread::sleep(Duration::from_millis(1000 / FPS as u64));
    }

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

fn paint_frame(f: &[u8; 9]) {
    for (p, v) in LED_PATHS.iter().zip(f.iter()) {
        let _ = fs::write(format!("{}/{}/brightness", LED_ROOT, p), format!("{}\n", v));
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
