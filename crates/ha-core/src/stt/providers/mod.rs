//! STT provider implementations.
//!
//! Batch path: `openai` (covers OpenAI Whisper, gpt-4o-transcribe batch,
//! Groq, DashScope/Qwen3-ASR via compatible mode, StepFun, SiliconFlow,
//! whisper.cpp server, faster-whisper-server, FunASR + OpenAI wrapper,
//! sherpa-onnx server).
//!
//! Streaming WS path: Deepgram / AssemblyAI / Azure Speech / iFlytek IAT /
//! Volcengine. Each module exposes `open_stream` returning a `SttStream`
//! handle so [`crate::stt::session::SttSessionManager`] can route by kind
//! without leaking per-provider types.

use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::errors::{SttError, SttResult};
use super::types::TranscriptDelta;

pub mod assemblyai;
pub mod azure;
pub mod deepgram;
pub mod openai;
pub mod volcengine;
pub mod xunfei;

/// WS frame caps shared across every WS provider, matching the
/// MCP client (`mcp/transport.rs`) defaults. Picked to bound the
/// per-message memory a misbehaving / hostile server can force on us.
pub(super) const WS_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub(super) const WS_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Shared mpsc capacity for the per-provider audio uplink and delta
/// downlink channels. ~6s of buffering at 100ms PCM16/16kHz chunks; any
/// real backpressure surfaces to the caller via `mpsc::Sender::send`.
pub(super) const STT_STREAM_CHANNEL_CAPACITY: usize = 64;

/// Common streaming handle returned by every WS provider's `open_stream`:
/// callers push raw audio bytes into `audio_tx` and drain partial / final
/// transcript deltas from `delta_rx`. Dropping `audio_tx` signals EOS so
/// the upstream task can close the WS gracefully.
pub struct SttStream {
    pub audio_tx: mpsc::Sender<Vec<u8>>,
    pub delta_rx: mpsc::Receiver<Result<TranscriptDelta, SttError>>,
}

/// Derive an http(s) "twin" URL from a ws(s) URL so it can flow through
/// `security::ssrf::check_url` (which rejects ws/wss schemes). All five
/// WS STT providers share the same twin step.
pub(super) fn ws_to_https_twin(url: &str, provider_label: &str) -> SttResult<String> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| SttError::Other(format!("Invalid {provider_label} URL: {e}")))?;
    let new_scheme = match parsed.scheme() {
        "wss" => Some("https"),
        "ws" => Some("http"),
        _ => None,
    };
    if let Some(scheme) = new_scheme {
        parsed
            .set_scheme(scheme)
            .map_err(|_| SttError::Other("Failed to derive SSRF twin URL".into()))?;
    }
    Ok(parsed.to_string())
}

/// Build a `WebSocketConfig` with the shared `WS_MAX_*_BYTES` caps and
/// connect via `tokio_tungstenite::connect_async_with_config`. Callers
/// build their own `Request` (so they can attach provider-specific
/// headers) and pass it in directly.
pub(super) async fn ws_connect_with_caps<R: IntoClientRequest + Unpin>(
    request: R,
    provider_label: &str,
) -> SttResult<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(WS_MAX_MESSAGE_BYTES))
        .max_frame_size(Some(WS_MAX_FRAME_BYTES));
    let (ws, _resp) = tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false)
        .await
        .map_err(|e| SttError::Network(format!("{provider_label} WS connect failed: {e}")))?;
    Ok(ws)
}
