//! AssemblyAI Universal Streaming (v3) WebSocket transcription.
//!
//! `wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&format_turns=true`
//! Auth header: `Authorization: <api_key>` (raw key, no `Bearer` / `Token`).
//!
//! Wire shape:
//! - Upstream: raw PCM16 LE mono frames (16 kHz default) as binary WS
//!   frames. Caller is responsible for resampling — front-end ships a
//!   16 kHz PCM16 AudioWorklet when the configured provider needs it.
//! - Downstream: JSON text frames:
//!   - `{"type":"Begin","id":"...","expires_at":...}` (session bootstrap)
//!   - `{"type":"Turn","turn_order":N,"transcript":"...","end_of_turn":bool,"transcript_confidence":...}`
//!   - `{"type":"Termination",...}`

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use crate::provider::AuthProfile;
use crate::stt::errors::{SttError, SttResult};
use crate::stt::types::{SttModelConfig, SttProviderConfig, TranscriptDelta, TranscriptOptions};

/// Default sample rate when the caller didn't pin one. AssemblyAI
/// Universal Streaming expects 16 kHz PCM16 mono unless otherwise told.
const DEFAULT_SAMPLE_RATE_HZ: u32 = 16_000;

pub async fn open_stream(
    provider: &SttProviderConfig,
    _model: &SttModelConfig,
    profile: &AuthProfile,
    options: &TranscriptOptions,
) -> SttResult<super::SttStream> {
    let base = provider.resolve_base_url(profile).trim_end_matches('/');
    let sample_rate = options
        .sample_rate_hz
        .filter(|hz| *hz > 0)
        .unwrap_or(DEFAULT_SAMPLE_RATE_HZ);
    let mut url = format!("{}/v3/ws?sample_rate={}", base, sample_rate);
    // Universal Streaming `format_turns=true` returns a single text per
    // turn (vs per-word partials); we still get `end_of_turn` so partial
    // vs final classification is preserved.
    url.push_str("&format_turns=true");
    if let Some(lang) = &options.language {
        if !lang.is_empty() {
            url.push_str(&format!("&language_code={}", urlencoding::encode(lang)));
        }
    }

    let https_twin = super::ws_to_https_twin(&url, "AssemblyAI")?;
    provider.check_ssrf(&https_twin).await?;

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| SttError::Other(format!("Invalid AssemblyAI URL: {e}")))?;
    // AssemblyAI takes the raw API key without a `Bearer` / `Token`
    // prefix — that's their documented Universal Streaming auth shape.
    request.headers_mut().insert(
        "Authorization",
        profile
            .api_key
            .parse()
            .map_err(|e| SttError::Other(format!("Bad AssemblyAI auth header: {e}")))?,
    );

    let ws = super::ws_connect_with_caps(request, "AssemblyAI").await?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(super::STT_STREAM_CHANNEL_CAPACITY);
    let (delta_tx, delta_rx) =
        mpsc::channel::<Result<TranscriptDelta, SttError>>(super::STT_STREAM_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        while let Some(chunk) = audio_rx.recv().await {
            if ws_sink.send(Message::Binary(chunk.into())).await.is_err() {
                break;
            }
        }
        // Send the Terminate sentinel and let AssemblyAI deliver its
        // final Turn + Termination frames before closing on its end.
        // Sending Close here races against those final data frames.
        let _ = ws_sink
            .send(Message::Text(r#"{"type":"Terminate"}"#.into()))
            .await;
    });

    let session_id = String::new();
    tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Some(delta) = parse_message(&session_id, &text) {
                        if delta_tx.send(Ok(delta)).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(Message::Binary(_))
                | Ok(Message::Frame(_))
                | Ok(Message::Ping(_))
                | Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    let _ = delta_tx
                        .send(Err(SttError::Network(format!("AssemblyAI WS recv: {e}"))))
                        .await;
                    break;
                }
            }
        }
    });

    Ok(super::SttStream { audio_tx, delta_rx })
}

fn parse_message(session_id: &str, raw: &str) -> Option<TranscriptDelta> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let msg_type = value.get("type").and_then(|v| v.as_str())?;
    if msg_type != "Turn" {
        return None;
    }
    let text = value
        .get("transcript")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return None;
    }
    let is_final = value
        .get("end_of_turn")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confidence = value
        .get("transcript_confidence")
        .and_then(|v| v.as_f64())
        .map(|c| c as f32);
    Some(TranscriptDelta {
        session_id: session_id.to_string(),
        text: text.to_string(),
        is_final,
        start_ms: None,
        end_ms: None,
        confidence,
        language: None,
        accumulated: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_partial_turn() {
        let raw = r#"{
            "type":"Turn",
            "turn_order":1,
            "transcript":"hello world",
            "end_of_turn":false,
            "transcript_confidence":0.92
        }"#;
        let d = parse_message("sess-x", raw).expect("Turn should parse");
        assert_eq!(d.text, "hello world");
        assert!(!d.is_final);
        assert!(d.confidence.unwrap() > 0.9);
    }

    #[test]
    fn parse_marks_final_on_end_of_turn() {
        let raw = r#"{"type":"Turn","transcript":"done","end_of_turn":true}"#;
        let d = parse_message("s", raw).unwrap();
        assert!(d.is_final);
    }

    #[test]
    fn parse_skips_begin_and_termination() {
        assert!(parse_message("s", r#"{"type":"Begin","id":"abc"}"#).is_none());
        assert!(parse_message("s", r#"{"type":"Termination"}"#).is_none());
    }

    #[test]
    fn parse_skips_empty_transcript() {
        let raw = r#"{"type":"Turn","transcript":"","end_of_turn":false}"#;
        assert!(parse_message("s", raw).is_none());
    }
}
