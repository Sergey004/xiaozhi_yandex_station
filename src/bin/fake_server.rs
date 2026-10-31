//! Minimal xiaozhi-protocol test server for the station backend.
//! Two ports (argv: <http_port> <ws_port>, defaults 8001 8002):
//!   http_port: POST /xiaozhi/ota/ -> {} (device treated as activated)
//!   ws_port:   xiaozhi WS: hello handshake, then one canned TTS burst
//!              (2 s of 440 Hz opus @ 24 kHz mono) after the first listen.
//! Built as a static aarch64-musl binary; run on the station and point the
//! client config at ws://127.0.0.1:<ws_port>/xiaozhi/v1/.

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message;

fn gen_tts_packets(sample_rate: u32) -> Vec<Vec<u8>> {
    let mut encoder = opus::Encoder::new(
        sample_rate,
        opus::Channels::Mono,
        opus::Application::Audio,
    )
    .expect("opus encoder");
    encoder.set_bitrate(opus::Bitrate::Bits(32000)).unwrap();
    let frame: usize = (sample_rate as usize / 1000) * 20; // 20 ms
    let total_frames = (sample_rate as usize * 2) / frame; // 2 seconds
    let mut out = Vec::with_capacity(total_frames);
    let mut phase = 0.0f64;
    for _ in 0..total_frames {
        let mut pcm = Vec::with_capacity(frame);
        for _ in 0..frame {
            pcm.push((9000.0 * phase.sin()) as i16);
            phase += 2.0 * std::f64::consts::PI * 440.0 / sample_rate as f64;
        }
        let mut buf = [0u8; 4000];
        let n = encoder.encode(&pcm, &mut buf).expect("opus encode");
        out.push(buf[..n].to_vec());
    }
    out
}

async fn handle_ws(stream: TcpStream, tts: Arc<Vec<Vec<u8>>>, fired: Arc<Mutex<bool>>) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            log::error!("ws accept failed: {}", e);
            return;
        }
    };
    let (mut write, mut read) = ws.split();
    log::info!("WS connected");
    while let Some(Ok(msg)) = read.next().await {
        match msg {
            Message::Text(text) => {
                let v: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("hello") => {
                        let hello = json!({
                            "type": "hello",
                            "version": 1,
                            "transport": "websocket",
                            "session_id": "fake-session",
                            "audio_params": {
                                "format": "opus", "sample_rate": 24000,
                                "channels": 1, "frame_duration": 20
                            }
                        });
                        log::info!("client hello -> server hello");
                        if write.send(Message::Text(hello.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    Some("listen") => {
                        let mut fired = fired.lock().await;
                        if *fired {
                            continue;
                        }
                        *fired = true;
                        drop(fired);
                        log::info!("listen received -> sending TTS burst ({} packets)", tts.len());
                        let start = json!({"type":"tts","session_id":"fake-session","state":"start"});
                        if write.send(Message::Text(start.to_string().into())).await.is_err() {
                            return;
                        }
                        for pkt in tts.iter() {
                            if write.send(Message::Binary(pkt.clone().into())).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                        }
                        let stop = json!({"type":"tts","session_id":"fake-session","state":"stop"});
                        if write.send(Message::Text(stop.to_string().into())).await.is_err() {
                            return;
                        }
                        log::info!("TTS burst done");
                    }
                    other => log::info!("text msg: {:?}", other),
                }
            }
            Message::Binary(data) => log::info!("binary frame {} bytes", data.len()),
            Message::Close(_) => break,
            _ => {}
        }
    }
    log::info!("WS closed");
}

async fn handle_http(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
            break;
        }
    }
    let first = String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_string();
    log::info!("http: {}", first);
    let body = "{}";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let http_port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(8001);
    let ws_port: u16 = std::env::args().nth(2).and_then(|p| p.parse().ok()).unwrap_or(8002);
    let tts = Arc::new(gen_tts_packets(24000));
    log::info!(
        "fake xiaozhi server: http :{} (ota), ws :{} ({} tts packets)",
        http_port, ws_port, tts.len()
    );
    let fired = Arc::new(Mutex::new(false));
    let http_listener = TcpListener::bind(("127.0.0.1", http_port)).await?;
    let ws_listener = TcpListener::bind(("127.0.0.1", ws_port)).await?;
    let ws_task = {
        let tts = tts.clone();
        let fired = fired.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((stream, _)) = ws_listener.accept().await {
                    tokio::spawn(handle_ws(stream, tts.clone(), fired.clone()));
                }
            }
        })
    };
    tokio::spawn(async move {
        let _ = ws_task.await;
    });
    loop {
        if let Ok((stream, _)) = http_listener.accept().await {
            tokio::spawn(handle_http(stream));
        }
    }
}
