//! Deepgram realtime WebSocket STT.
//!
//! `wss://api.deepgram.com/v1/listen?model=...&interim_results=true&punctuate=true&...`
//! Auth: `Authorization: Token <api_key>`.
//!
//! Wire shape:
//! - Upstream: binary frames carrying raw audio bytes (PCM16 / Opus /
//!   WebM-Opus all accepted — Deepgram auto-detects from the first chunk).
//! - Downstream: JSON text frames `{ "channel": { "alternatives": [{ "transcript": "...", "confidence": ... }] }, "is_final": bool, ... }`.
//!   `speech_final` / `is_final` distinguishes a stable utterance edge from
//!   an interim partial.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use crate::provider::AuthProfile;
use crate::stt::errors::{SttError, SttResult};
use crate::stt::types::{
    SttModelConfig, SttProviderConfig, Transcript, TranscriptDelta, TranscriptOptions,
};

/// Open a Deepgram streaming session. Returns `(audio_tx, delta_rx)`:
/// callers push raw audio bytes into `audio_tx` and drain transcript
/// deltas off `delta_rx`. Closing `audio_tx` (drop) signals end-of-audio;
/// the engine drains the upstream WS until its final delta then closes
/// `delta_rx`.
///
/// The two background tasks (audio → WS upstream / WS downstream → delta_rx)
/// share a `tokio_util::sync::CancellationToken`-equivalent flag via the
/// channels themselves — drop either end and the task notices.
pub async fn open_stream(
    provider: &SttProviderConfig,
    model: &SttModelConfig,
    profile: &AuthProfile,
    options: &TranscriptOptions,
) -> SttResult<super::SttStream> {
    let base = provider.resolve_base_url(profile).trim_end_matches('/');
    let mut url = format!("{}/v1/listen?model={}&interim_results=true", base, model.id);
    if let Some(lang) = &options.language {
        if !lang.is_empty() {
            url.push_str(&format!("&language={}", urlencoding::encode(lang)));
        }
    }
    if options.punctuation.unwrap_or(true) {
        url.push_str("&punctuate=true");
    }
    if options.diarization.unwrap_or(false) {
        url.push_str("&diarize=true");
    }
    if let Some(sr) = options.sample_rate_hz {
        // Deepgram needs encoding+sample_rate when the audio is raw PCM. For
        // container-wrapped audio (WebM/Opus, MP3) it autodetects; we forward
        // sample_rate as a hint either way.
        url.push_str(&format!("&sample_rate={}", sr));
    }

    let https_twin = super::ws_to_https_twin(&url, "Deepgram")?;
    provider.check_ssrf(&https_twin).await?;

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| SttError::Other(format!("Invalid Deepgram URL: {e}")))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {}", profile.api_key)
            .parse()
            .map_err(|e| SttError::Other(format!("Bad auth header: {e}")))?,
    );

    let ws = super::ws_connect_with_caps(request, "Deepgram").await?;
    let (mut ws_sink, mut ws_stream) = ws.split();

    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(super::STT_STREAM_CHANNEL_CAPACITY);
    let (delta_tx, delta_rx) =
        mpsc::channel::<Result<TranscriptDelta, SttError>>(super::STT_STREAM_CHANNEL_CAPACITY);

    // Audio uplink — forward raw bytes from caller into WS binary frames.
    // When the caller drops `audio_tx`, audio_rx.recv() returns None →
    // we send Deepgram's "close stream" sentinel `{"type":"CloseStream"}`
    // and let the downstream task drain remaining final transcripts.
    tokio::spawn(async move {
        while let Some(chunk) = audio_rx.recv().await {
            if ws_sink.send(Message::Binary(chunk.into())).await.is_err() {
                break;
            }
        }
        // Send Deepgram's `CloseStream` sentinel and let the server
        // deliver its final transcript before closing. Sending Close
        // here races against the trailing partial→final frame.
        let _ = ws_sink
            .send(Message::Text(r#"{"type":"CloseStream"}"#.into()))
            .await;
    });

    // Session_id is filled in by the SttSessionManager after open_stream
    // returns — we don't need it on the engine side. Use empty placeholder.
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
                | Ok(Message::Pong(_)) => {
                    // ignore
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    let _ = delta_tx
                        .send(Err(SttError::Network(format!("Deepgram WS recv: {e}"))))
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
    let alt = value.pointer("/channel/alternatives/0")?;
    let text = alt.get("transcript").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        return None;
    }
    let is_final = value
        .get("is_final")
        .and_then(|v| v.as_bool())
        .or_else(|| value.get("speech_final").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    let confidence = alt
        .get("confidence")
        .and_then(|v| v.as_f64())
        .map(|c| c as f32);
    let start = value.get("start").and_then(|v| v.as_f64());
    let duration = value.get("duration").and_then(|v| v.as_f64());
    let start_ms = start.map(|s| (s * 1000.0).max(0.0) as u64);
    let end_ms = match (start, duration) {
        (Some(s), Some(d)) => Some(((s + d) * 1000.0).max(0.0) as u64),
        _ => None,
    };
    Some(TranscriptDelta {
        session_id: session_id.to_string(),
        text: text.to_string(),
        is_final,
        start_ms,
        end_ms,
        confidence,
        language: None,
        accumulated: None,
    })
}

/// Batch path for Deepgram — placeholder so the failover chain stays
/// uniform across providers. Real `/v1/listen` HTTP batch can be wired in
/// when needed.
#[allow(dead_code)]
pub async fn transcribe_batch(
    _provider: &SttProviderConfig,
    _model: &SttModelConfig,
    _profile: &AuthProfile,
    _audio: crate::stt::AudioPayload,
    _options: &TranscriptOptions,
) -> SttResult<Transcript> {
    Err(SttError::Other(
        "Deepgram batch transcription is not implemented; use the streaming session instead".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_partial_transcript() {
        let raw = r#"{
            "channel":{"alternatives":[{"transcript":"hello","confidence":0.97}]},
            "is_final":false,
            "start":1.2,
            "duration":0.4
        }"#;
        let d = parse_message("sess-1", raw).expect("partial should parse");
        assert_eq!(d.text, "hello");
        assert!(!d.is_final);
        assert_eq!(d.start_ms, Some(1200));
        assert_eq!(d.end_ms, Some(1600));
        assert!(d.confidence.unwrap() > 0.9);
        assert_eq!(d.session_id, "sess-1");
    }

    #[test]
    fn parse_marks_final_via_is_final() {
        let raw = r#"{"channel":{"alternatives":[{"transcript":"hi"}]},"is_final":true}"#;
        let d = parse_message("s", raw).unwrap();
        assert!(d.is_final);
    }

    #[test]
    fn parse_falls_back_to_speech_final() {
        let raw = r#"{"channel":{"alternatives":[{"transcript":"bye"}]},"speech_final":true}"#;
        let d = parse_message("s", raw).unwrap();
        assert!(d.is_final);
    }

    #[test]
    fn parse_skips_empty_transcripts() {
        let raw = r#"{"channel":{"alternatives":[{"transcript":""}]},"is_final":false}"#;
        assert!(parse_message("s", raw).is_none());
    }
}
