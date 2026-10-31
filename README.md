# Xiaozhi on Yandex Station — a Rust xiaozhi client for Yandex Station Lite 2

[Русский](README_ru.md) | English

A Rust voice-assistant client for the [xiaozhi](https://github.com/78/xiaozhi-esp32)
protocol, ported to the **Yandex Station Lite 2** (codename *Plum*): it speaks
through the **stock speaker** and listens through the **stock Quasar/VQE
microphone pipeline**, without modifying the firmware.

A fork of [`haoyn231/xiaozhi_linux_rs`](https://github.com/haoyn231/xiaozhi_linux_rs)
(original README: [README_upstream.md](README_upstream.md)). The host ALSA
backend, the MCP gateway and the xiaozhi protocol are untouched; a second
audio backend — `station` — plus protocol improvements are added on top.

```
        Yandex Station Lite 2                        Your server
┌─────────────────────────────────────┐      ┌────────────────────────────┐
│  Quasar (stock, unmodified)         │      │  xiaozhi-protocol server   │
│   mics → VQE → vqe_0 channel        │      │   ASR (Whisper)            │
│        │ (LD_PRELOAD tap)           │      │   LLM  → TTS (Chatterbox)  │
│        ▼  unix datagrams, 10ms      │      │        → RVC (voice clone) │
│  xiaozhi_linux_rs (static aarch64)  │      │   40k output → 48k link    │
│   SpeexDSP → Opus 16k ── WebSocket ─┼──────┤                            │
│   decoder 48k ← Opus ◄──────────────┼──────┤  per-device downlink:      │
│        ▼                            │      │   HD clients 48k,          │
│  maind --gst-launch → speaker ──────┘      │   ESP32 and others 16k     │
│  (FS2105 amp is owned by maind)            │                            │
└─────────────────────────────────────┘      └────────────────────────────┘
```

## Why it is built this way

The Station ships third-party firmware, and the port is shaped by three of
its properties (verified on live hardware):

1. **aarch64 kernel, 32-bit armhf userspace.** The client is therefore a
   fully static `aarch64-unknown-linux-musl` binary: the kernel executes it
   directly and no system library is needed (opus/speexdsp/alsa-lib are
   linked in).
2. **The amplifier and player belong to `maind`.** Direct ALSA playback from
   a foreign process is silent (the amp is muted while idle) and maind's
   dmix segment uses a 32-bit shm layout. So playback goes through
   `maind --gst-launch`: maind drives its own amp and audio path.
3. **Quasar owns mic processing.** The `vqe_0` channel (noise suppression,
   AEC, AGC) is exported by an LD_PRELOAD hook and read by the client over
   unix datagrams — read-only; Quasar is not modified and not restarted
   outside an explicit deployment step.

## What is added on top of upstream

| Module | What it does |
|---|---|
| `src/audio/station_capture.rs` | vqe_0 capture: unix datagrams (10 ms, mono 16 kHz) → SpeexDSP (denoise/AGC) → Opus |
| `src/audio/station_playback.rs` | decoder built **from the server hello** (any rate/channels) → 2 s buffer → WAV → `maind --gst-launch` |
| `audio_mode` config | `alsa` (upstream backend) \| `station` — backend switch; the ALSA path is untouched |
| `features.downlink_hd` in hello | per-device downlink: the server sends 48k to HD clients and 16k to the rest (ESP32-compatible) |
| listen stop/start contract | mic muting on the server-side VAD signal, `llm`/`stt` message handling |
| `bin/fake_server` | a minimal xiaozhi-protocol test server (OTA + WS + TTS burst) for network-free end-to-end tests |
| `config.toml.example` | config template; `config.toml` is device-specific and gitignored, build.rs falls back to the example |

## Server side

The companion repo [`Sergey004/xiaozhi_bridge`](https://github.com/Sergey004/xiaozhi_bridge):
`xiaozhi_server.py` — an all-in-one xiaozhi server (OTA + WS bridge + API)
with a **Chatterbox TTS → RVC** pipeline (your own cloned voice), automatic
pitch alignment (median f0 of the reference vs the generated audio → exact
semitone shift: output lands at 139 Hz against a 142 Hz target) and
per-device 48k/16k downlink.

## Build & deploy

Host requirements: Rust 1.85+, an `aarch64-linux-musl` cross toolchain
(musl.cc), pkg-config. The static-dependency build script is
`src-deps/build-static.sh` (alsa-lib/opus/speexdsp → sysroot); see the
reports for details.

```bash
cp config.toml.example config.toml        # your addresses/token
# build (env: PKG_CONFIG_LIBDIR=<sysroot>/lib/pkgconfig, PKG_CONFIG_SYSROOT_DIR=<sysroot>,
#        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=<tc>/aarch64-linux-musl-gcc)
cargo build --release --target aarch64-unknown-linux-musl
# ELF64 aarch64, statically linked, ~3.6 MB
```

On the Station (root, SSH): the `/data/opt/xiaozhi/` directory holds the
binary + `xiaozhi_config.json` (`audio_mode: "station"`,
`station_capture_socket`, server address); run `./xiaozhi_linux_rs` manually
from that directory. The root filesystem stays read-only, stock services
(`S99quasar` etc.) are untouched, and rollback is deleting the directory.

## Status

- [x] full voice loop: vqe_0 → server (STT/LLM/TTS) → speaker
- [x] 48k downlink with automatic voice-pitch alignment
- [x] per-device rate negotiation (HD client 48k / ESP32 16k)
- [ ] LED ring (sysfs mechanics known: `plum:{r,g,b}:ring1-3`, mute guard)
- [ ] client autostart
- [ ] TTS smoothness (chunk prefetch)

## Credits

- [`haoyn231/xiaozhi_linux_rs`](https://github.com/haoyn231/xiaozhi_linux_rs) — the base
- [`78/xiaozhi-esp32`](https://github.com/78/xiaozhi-esp32) — the protocol
- [`skrashevich/quasar-bridge`](https://github.com/skrashevich/quasar-bridge) — uniproxy internals
- [`rison1337/Alisa-ChatGPT-voice-assistant`](https://github.com/rison1337/Alisa-ChatGPT-voice-assistant) — vqe_tap and hardware research
- the [hackalice.me](https://hackalice.me) community — Station recipes

License: [MIT](LICENSE) (same as upstream). The binary also links ALSA (LGPL),
Opus and SpeexDSP — respect their terms when redistributing.
