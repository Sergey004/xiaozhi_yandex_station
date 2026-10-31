use crate::audio_bridge::{AudioBridge, AudioEvent};
use crate::config::Config;
use crate::gui_bridge::{GuiBridge, GuiEvent};
use crate::net_link::{NetCommand, NetEvent};
use crate::protocol::ServerMessage;
use crate::state_machine::SystemState;
use serde_json;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct CoreController {
    state: SystemState,
    current_session_id: Option<String>,
    should_mute_mic: bool,
    config: Config,
    net_tx: mpsc::Sender<NetCommand>,
    audio_bridge: Arc<AudioBridge>,
    gui_bridge: Arc<GuiBridge>,
    led: Option<crate::led::LedRing>,
    /// Момент последней смены состояния (для watchdog залипшего Processing).
    state_changed: std::time::Instant,
    /// Сессия прослушки: микрофон стримится только при активной сессии
    /// (иначе клиент слышит телек/ambient и сервер отвечает на всё подряд).
    session_active: bool,
    /// TTS закончился в сети; ждём фактического конца проигрывания (maind).
    awaiting_playback: bool,
}

impl CoreController {
    pub fn new(
        config: Config,
        net_tx: mpsc::Sender<NetCommand>,
        audio_bridge: Arc<AudioBridge>,
        gui_bridge: Arc<GuiBridge>,
        led: Option<crate::led::LedRing>,
    ) -> Self {
        Self {
            state: SystemState::Idle,
            current_session_id: None,
            should_mute_mic: false,
            config,
            net_tx,
            audio_bridge,
            gui_bridge,
            led,
            state_changed: std::time::Instant::now(),
            session_active: false,
            awaiting_playback: false,
        }
    }

    /// Watchdog: сервер после VAD-конца шлёт listen stop и пайплайн обязан
    /// закончиться tts stop. Если Processing висит дольше 30с (потерянный
    /// ответ) — возвращаемся в idle и сами просим прослушку.
    pub async fn watchdog_tick(&mut self) {
        let stuck = match self.state {
            // сервер потерял tts/stop (зависшая генерация TTS и т.п.)
            SystemState::Processing => self.state_changed.elapsed() > std::time::Duration::from_secs(30),
            SystemState::Speaking => self.state_changed.elapsed() > std::time::Duration::from_secs(90),
            _ => false,
        };
        if stuck {
            log::warn!(
                "State {:?} stuck >threshold, resetting to idle and re-listening",
                self.state
            );
            self.set_state(SystemState::Idle);
            self.should_mute_mic = false;
            if self.session_active {
                self.send_auto_listen_command().await;
            }
        }
    }

    pub fn led_handle(&self) -> Option<crate::led::LedRing> {
        self.led.clone()
    }

    /// Update the internal state and mirror it to the LED ring.
    fn set_state(&mut self, state: SystemState) {
        if self.state == state {
            return;
        }
        self.state = state;
        self.state_changed = std::time::Instant::now();
        if let Some(led) = &self.led {
            use crate::state_machine::SystemState as S;
            use crate::led::*;
            let code = match state {
                S::Idle | S::NetworkError => LED_STATE_IDLE,
                S::Listening => LED_STATE_LISTENING,
                S::Processing => LED_STATE_PROCESSING,
                S::Speaking => LED_STATE_SPEAKING,
            };
            led.set_state(code);
        }
    }

    // 处理来自 NetLink 的事件
    pub async fn handle_net_event(&mut self, event: NetEvent) {
        match event {
            NetEvent::Text(text) => self.process_server_text(text).await,
            NetEvent::Binary(data) => self.process_server_audio(data).await,
            NetEvent::Connected => {
                log::info!("WebSocket Connected");
                if let Err(e) = self.gui_bridge.send_message(r#"{"state": 3}"#).await {
                    log::error!("Failed to send to GUI: {}", e);
                }
            }
            NetEvent::Disconnected => {
                log::info!("WebSocket Disconnected");
                self.set_state(SystemState::NetworkError);
                if let Err(e) = self.gui_bridge.send_message(r#"{"state": 4}"#).await {
                    log::error!("Failed to send to GUI: {}", e);
                }
            }
        }
    }

    // 处理来自服务器的文本消息
    async fn process_server_text(&mut self, text: String) {
        log::info!("Received Text from Server: {}", text);

        let msg: ServerMessage = match serde_json::from_str(&text) {
            Ok(msg) => msg,
            Err(_) => {
                // 可能不是JSON，忽略
                return;
            }
        };

        if let Some(sid) = &msg.session_id {
            if self.current_session_id.as_deref() != Some(sid) {
                log::info!("New Session ID: {}", sid);
                self.current_session_id = Some(sid.clone());
            }
        }

        match msg.msg_type.as_str() {
            "hello" => {
                log::info!("Server Hello received. Session armed (LED idle, mic closed).");
                // Прослушка открывается только командой session start (UDP GUI).
            }
            "iot" => {
                // IoT-команды исполняет серверная сторона; клиенту остаётся лог
                if let Some(cmd) = &msg.command {
                    log::info!("IoT Command (server-side): {}", cmd);
                }
            }
            "tts" => {
                if let Some(state) = &msg.state {
                    if state == "start" || state == "sentence_start" {
                        self.should_mute_mic = true;
                        self.awaiting_playback = false;
                        self.set_state(SystemState::Speaking);
                        log::info!("TTS Started (state={}), muting mic for AEC, sending state 6 to GUI", state);
                        if let Err(e) = self.gui_bridge.send_message(r#"{"state": 6}"#).await {
                            log::error!("Failed to send state 6 to GUI: {}", e);
                        }
                    } else if state == "stop" || state == "sentence_end" {
                        // Сеть закончила; звук может ещё играть через maind.
                        // Ждём PlaybackFinished и только потом открываем прослушку.
                        log::info!("TTS Stopped (state={}), flushing playback tail", state);
                        self.audio_bridge.flush_playback().await;
                        self.awaiting_playback = true;
                    }
                }

                if let Some(t) = msg.text {
                    log::info!("TTS: {}", t);
                    // 仅在开启TTS显示开关时才将文本发送给GUI显示
                    if self.config.enable_tts_display {
                        if let Err(e) = self.gui_bridge.send_message(&text).await {
                            log::error!("Failed to send TTS text to GUI: {}", e);
                        }
                    }
                }
            }
            "stt" => {
                if let Some(text_content) = msg.text {
                    log::info!("STT Result: {}", text_content);
                }
            }
            "llm" => {
                if let Some(text_content) = &msg.text {
                    log::info!("LLM emotion/text: {}", text_content);
                }
            }
            "listen" => {
                // Server-side VAD contract (xiaozhi_bridge): the server decides
                // when speech ends and sends listen/stop; the client must stop
                // streaming until it opens listening again.
                match msg.state.as_deref() {
                    Some("stop") => {
                        self.should_mute_mic = true;
                        self.set_state(SystemState::Processing);
                        log::info!("Server listen stop: muting mic until next listen");
                    }
                    Some("start") => {
                        self.should_mute_mic = false;
                        // Подтверждаем свой listen/start: сервер поднимет флаг
                        // приёма кадров и сбросит VAD (resume после пустого STT)
                        if self.session_active {
                            self.send_auto_listen_command().await;
                        }
                        log::info!("Server listen start: unmuting mic, re-arming VAD");
                    }
                    _ => {}
                }
            }
            other => {
                log::warn!("Unhandled message type: {}", other);
            }
        }
    }

    // 处理来自服务器的音频数据
    async fn process_server_audio(&mut self, data: Vec<u8>) {
        if self.state != SystemState::Speaking {
            self.set_state(SystemState::Speaking);
            if let Err(e) = self.gui_bridge.send_message(r#"{"state": 6}"#).await {
                log::error!("Failed to send to GUI: {}", e);
            }
        }
        if let Err(e) = self.audio_bridge.send_audio(&data).await {
            log::error!("Failed to send to Audio: {}", e);
        }
    }

    // 发送自动监听命令
    async fn send_auto_listen_command(&self) {
        let session_id = self.current_session_id.as_deref().unwrap_or("");
        let listen_cmd = format!(
            r#"{{"session_id":"{}","type":"listen","state":"start","mode":"auto"}}"#,
            session_id
        );
        if let Err(e) = self.net_tx.send(NetCommand::SendText(listen_cmd)).await {
            log::error!("Failed to send loop listen command: {}", e);
        }
    }

    // 处理来自 AudioBridge 的事件
    pub async fn handle_audio_event(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::PlaybackFinished => {
                // Держим зелёный, пока звук реально играет; обновляем watchdog.
                self.state_changed = std::time::Instant::now();
                if !self.awaiting_playback {
                    return;
                }
                self.awaiting_playback = false;
                // single: одна фраза — один цикл (анти-телевизор).
                // continuous: открываем прослушку снова — диалог внутри сессии.
                if self.config.session_mode != "continuous" {
                    self.session_active = false;
                    self.should_mute_mic = true;
                } else {
                    self.should_mute_mic = false;
                }
                if let Err(e) = self.gui_bridge.send_message(r#"{"state": 3}"#).await {
                    log::error!("Failed to send state 3 to GUI: {}", e);
                }
                if self.session_active && self.config.session_mode == "continuous" {
                    self.set_state(SystemState::Listening);
                    self.send_auto_listen_command().await;
                    log::info!("Playback finished, listening (continuous session)");
                    return;
                }
                self.set_state(SystemState::Idle);
                log::info!("Playback finished, one-shot session closed (mic muted)");
                return;
            }
            AudioEvent::AudioData(data) => {
                if self.should_mute_mic || !self.session_active {
                    return;
                }
                if self.state != SystemState::Listening {
                    self.set_state(SystemState::Listening);
                    if let Err(e) = self.gui_bridge.send_message(r#"{"state": 5}"#).await {
                        log::error!("Failed to send to GUI: {}", e);
                    }
                }
                if let Err(e) = self.net_tx.send(NetCommand::SendBinary(data)).await {
                    log::error!("Failed to send audio to NetLink: {}", e);
                }
            }
        }
    }

    // 处理来自 GuiBridge 的事件
    pub async fn handle_gui_event(&mut self, event: GuiEvent) {
        let GuiEvent::Message(msg) = event;
        log::info!("Received Message from GUI: {}", msg);

        // Управление сессией прослушки (UDP с хоста/телефа):
        //   {"type":"session","state":"start"} — открыть сессию
        //   {"type":"session","state":"stop"}  — закрыть (микрофон закрыт)
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
            if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                match v.get("state").and_then(|s| s.as_str()) {
                    Some("start") => return self.session_open().await,
                    Some("stop") => return self.session_close().await,
                    _ => {}
                }
            }
        }

        if let Err(e) = self.net_tx.send(NetCommand::SendText(msg)).await {
            log::error!("Failed to send text to NetLink: {}", e);
        }
    }

    /// Открыть сессию: снять мьют, отправить listen start.
    pub async fn session_open(&mut self) {
        if self.session_active {
            return;
        }
        self.session_active = true;
        self.should_mute_mic = false;
        self.set_state(SystemState::Listening);
        self.send_auto_listen_command().await;
        log::info!("Session opened (listening)");
    }

    /// Закрыть сессию: мьют + listen stop серверу.
    pub async fn session_close(&mut self) {
        if !self.session_active {
            return;
        }
        self.session_active = false;
        self.should_mute_mic = true;
        self.set_state(SystemState::Idle);
        let session_id = self.current_session_id.as_deref().unwrap_or("");
        let stop = format!(
            r#"{{"session_id":"{}","type":"listen","state":"stop"}}"#,
            session_id
        );
        if let Err(e) = self.net_tx.send(NetCommand::SendText(stop)).await {
            log::error!("Failed to send listen stop: {}", e);
        }
        log::info!("Session closed (mic muted)");
    }
}
