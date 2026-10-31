[English](README.md) | Русский

# 小智 на Яндекс Станции — Rust-клиент xiaozhi для Yandex Station Lite 2

Rust-клиент голосового ассистента [xiaozhi](https://github.com/78/xiaozhi-esp32),
портированный на **Яндекс Станцию Лайт 2** (Plum): говорит штатным динамиком,
слушает через **штатный микрофонный тракт Quasar/VQE**, не трогая прошивку.

Fork [`haoyn231/xiaozhi_linux_rs`](https://github.com/haoyn231/xiaozhi_linux_rs)
(оригинальный README: [README_upstream.md](README_upstream.md)). Весь ALSA-бэкенд
хоста, MCP-шлюз и протокол xiaozhi сохранены; добавлен второй аудиобэкенд —
`station` — и протокольные улучшения.

```
        Яндекс Станция Лайт 2                          Ваш сервер
┌─────────────────────────────────────┐      ┌────────────────────────────┐
│  Quasar (штатный, не изменён)       │      │  xiaozhi-совместимый сервер│
│   микрофоны → VQE → канал vqe_0     │      │   ASR (Whisper)            │
│        │ (LD_PRELOAD tap)           │      │   LLM  → TTS (Chatterbox)  │
│        ▼  unix datagrams, 10мс      │      │        → RVC (голос)       │
│  xiaozhi_linux_rs (static aarch64)  │      │   выход 40к → даунлинк 48к │
│   SpeexDSP → Opus 16к ── WebSocket ─┼──────┤                            │
│   декодер 48к ← Opus ◄──────────────┼──────┤  per-device downlink:      │
│        ▼                            │      │   HD-клиентам 48к,         │
│  maind --gst-launch → динамик ──────┘      │   ESP32 и прочим 16к       │
│  (усилитель FS2105 управляется maind)      │                            │
└─────────────────────────────────────┘      └────────────────────────────┘
```

## Почему так, а не иначе

Станция — устройство с чужой прошивкой, и порт построен вокруг трёх её
свойств (проверено на живом железе, см. `docs/` и отчёты):

1. **Ядро aarch64, userspace прошивки — 32-bit armhf.** Значит клиент —
   полностью статический `aarch64-unknown-linux-musl`: ядро исполняет его
   напрямую, ни одна системная библиотека не нужна (opus/speexdsp/alsa-lib
   вшиты).
2. **Усилитель и плеер принадлежат `maind`.** Прямой ALSA-playback из
   стороннего процесса не слышен (амп заглушен в простое), а dmix-сегмент
   maind имеет 32-битную раскладку shm. Поэтому playback = `maind
   --gst-launch`: maind сам управляет усилителем и трактом.
3. **RVC/обработку микрофона делает Quasar.** Канал `vqe_0` (шумодав,
   эхоподавление, AGC) экспортируется LD_PRELOAD-хуком и читается клиентом
   по unix-датаграммам — read-only, Quasar не модифицируется и не
   перезапускается без явного шага деплоя.

## Что добавлено к апстриму

| Модуль | Что делает |
|---|---|
| `src/audio/station_capture.rs` | захват vqe_0: unix-датаграммы (10 мс, mono 16 кГц) → SpeexDSP (шумодав/AGC) → Opus |
| `src/audio/station_playback.rs` | декодер **по параметрам server hello** (любой rate/channels) → буфер 2 с → WAV → `maind --gst-launch` |
| `audio_mode` в конфиге | `alsa` (апстрим-бэкенд) \| `station` — выбор бэкенда, ALSA-путь не тронут |
| `features.downlink_hd` в hello | пер-девайсный даунлинк: сервер шлёт 48к HD-клиентам и 16к остальным (ESP32-совместимо) |
| контракт listen stop/start | mic muting по сигналу сервера (server-side VAD), обработка `llm`/`stt` |
| `bin/fake_server` | мини-сервер протокола xiaozhi (OTA + WS + TTS-бёрст) для сквозного теста без сети |
| `config.toml.example` | шаблон конфига; `config.toml` device-специфичен и в gitignore, build.rs умеет фоллбэк |

## Серверная сторона

Парный репозиторий [`Sergey004/xiaozhi_bridge`](https://github.com/Sergey004/xiaozhi_bridge):
`xiaozhi_server.py` — xiaozhi-совместимый сервер (OTA + WS-мост + API) с пайплайном
**Chatterbox TTS → RVC** (свой голос из референса), автовыравниванием высоты
(медианная f0 референса и генерации → точный полутоновый сдвиг: выход 139 Гц
при цели 142) и per-device даунлинком 48к/16к.

## Сборка и деплой

Требования к хосту: Rust 1.85+, кросс-тулчейн `aarch64-linux-musl` (musl.cc),
pkg-config. Скрипт сборки статических зависимостей — `src-deps/build-static.sh`
(alsa-lib/opus/speexdsp → sysroot), см. отчёты.

```bash
cp config.toml.example config.toml        # свои адреса/токен
# build (env: PKG_CONFIG_LIBDIR=<sysroot>/lib/pkgconfig, PKG_CONFIG_SYSROOT_DIR=<sysroot>,
#        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=<tc>/aarch64-linux-musl-gcc)
cargo build --release --target aarch64-unknown-linux-musl
# ELF64 aarch64, statically linked, ~3.6 МБ
```

На станции (root, SSH): каталог `/data/opt/xiaozhi/` — бинарник +
`xiaozhi_config.json` (`audio_mode: "station"`, `station_capture_socket`,
адрес сервера), запуск вручную `./xiaozhi_linux_rs` из этого каталога.
Корневая ФС остаётся read-only, штатные сервисы (`S99quasar` и пр.) не
трогаются, откат — удаление каталога.

## Статус

- [x] сквозной голосовой цикл: vqe_0 → сервер (STT/LLM/TTS) → динамик
- [x] 48к даунлинк с автовыравниванием высоты голоса
- [x] пер-девайсное согласование (HD-клиент 48к / ESP32 16к)
- [ ] LED-кольцо (sysfs-механика известна: `plum:{r,g,b}:ring1-3`, mute-гард)
- [ ] автозапуск клиента
- [ ] плавность TTS (префетч чанков)

## Благодарности

- [`haoyn231/xiaozhi_linux_rs`](https://github.com/haoyn231/xiaozhi_linux_rs) — база
- [`78/xiaozhi-esp32`](https://github.com/78/xiaozhi-esp32) — протокол
- [`skrashevich/quasar-bridge`](https://github.com/skrashevich/quasar-bridge) — внутренности uniproxy/директив
- [`rison1337/Alisa-ChatGPT-voice-assistant`](https://github.com/rison1337/Alisa-ChatGPT-voice-assistant) — vqe_tap и работа с железом
- сообщество [hackalice.me](https://hackalice.me) — рецепты станции

Лицензия — [MIT](LICENSE) (как у апстрима). Бинарник также линкует ALSA (LGPL),
Opus и SpeexDSP — при дистрибуции соблюдайте их условия.
