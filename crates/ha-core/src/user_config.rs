use anyhow::Result;
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::paths;

// ── Server Mode Tags ─────────────────────────────────────────────

/// `UserConfig::server_mode` value when this install runs its own embedded
/// HTTP server (or no server at all). This is the default — `None` and
/// this string are equivalent at the consumer side.
pub const SERVER_MODE_EMBEDDED: &str = "embedded";

/// `UserConfig::server_mode` value when this install routes through a
/// separate `hope-agent server` running elsewhere. The frontend
/// transport / Web GUI / desktop shell all switch to remote mode when
/// they see this.
pub const SERVER_MODE_REMOTE: &str = "remote";

// ── User Config ──────────────────────────────────────────────────

/// Global user configuration, shared across all Agents.
/// Stored at ~/.hope-agent/user.json
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserConfig {
    /// User's display name / nickname
    #[serde(default)]
    pub name: Option<String>,

    /// Avatar: file path or URL
    #[serde(default)]
    pub avatar: Option<String>,

    /// Gender: "male", "female", or custom text
    #[serde(default)]
    pub gender: Option<String>,

    /// Birthday in "YYYY-MM-DD" format
    #[serde(default)]
    pub birthday: Option<String>,

    /// Role description, e.g. "全栈开发者"
    #[serde(default)]
    pub role: Option<String>,

    /// IANA timezone, e.g. "Asia/Shanghai"
    #[serde(default)]
    pub timezone: Option<String>,

    /// Preferred language, e.g. "zh-CN", "en"
    #[serde(default)]
    pub language: Option<String>,

    /// AI experience level: "expert", "intermediate", "beginner"
    #[serde(default)]
    pub ai_experience: Option<String>,

    /// Response style: "concise", "detailed", or custom text
    #[serde(default)]
    pub response_style: Option<String>,

    /// Free-form extra info the user wants the AI to know
    #[serde(default)]
    pub custom_info: Option<String>,

    // ── Chat behavior settings ──
    /// Whether pending messages auto-send after reply finishes (default: false)
    #[serde(default)]
    pub auto_send_pending: bool,

    /// Whether thinking blocks auto-expand in chat bubbles (default: true)
    #[serde(default = "crate::default_true")]
    pub auto_expand_thinking: bool,

    /// Preferred chat rendering mode: "bubble" or "timeline".
    #[serde(default)]
    pub chat_display_mode: Option<String>,

    // ── Weather / Location settings ──
    // ── Server mode settings ──
    /// Server mode: [`SERVER_MODE_EMBEDDED`] (default) or [`SERVER_MODE_REMOTE`].
    /// Stored as `Option<String>` to preserve `None` semantics on disk
    /// (older configs without the field default to embedded).
    #[serde(default)]
    pub server_mode: Option<String>,

    /// Remote server URL, e.g. "http://192.168.1.100:8420"
    #[serde(default)]
    pub remote_server_url: Option<String>,

    /// API key for authenticating with a remote server
    #[serde(default)]
    pub remote_api_key: Option<String>,

    /// Whether to inject weather info into system prompt (default: true)
    #[serde(default = "crate::default_true")]
    pub weather_enabled: bool,

    /// City name for weather lookup
    #[serde(default)]
    pub weather_city: Option<String>,

    /// Latitude for weather lookup
    #[serde(default)]
    pub weather_latitude: Option<f64>,

    /// Longitude for weather lookup
    #[serde(default)]
    pub weather_longitude: Option<f64>,
}

// ── Persistence ──────────────────────────────────────────────────

/// Load user config from ~/.hope-agent/user.json
/// Returns default if file doesn't exist.
pub fn load_user_config() -> Result<UserConfig> {
    let path = paths::user_config_path()?;
    if !path.exists() {
        return Ok(UserConfig::default());
    }
    let data = std::fs::read_to_string(&path)?;
    let config: UserConfig = serde_json::from_str(&data)?;
    Ok(config)
}

/// Save user config to ~/.hope-agent/user.json
pub fn save_user_config_to_disk(config: &UserConfig) -> Result<()> {
    let path = paths::user_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Autosave the pre-change file so every settings edit is rollback-able.
    crate::backup::snapshot_before_write(&path, "user");

    let data = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, data)?;
    Ok(())
}

// ── System Prompt Context ────────────────────────────────────────

/// Helper: push a line if value is non-empty
fn push_if(lines: &mut Vec<String>, label: &str, val: &Option<String>) {
    if let Some(v) = val {
        if !v.is_empty() {
            lines.push(format!("- {}: {}", label, v));
        }
    }
}

/// Build a user context section for injection into the system prompt.
/// Returns None if no meaningful user info is configured.
pub fn build_user_context(config: &UserConfig) -> Option<String> {
    let mut lines = Vec::new();

    push_if(&mut lines, "Name", &config.name);
    push_if(&mut lines, "Gender", &config.gender);
    if let Some(birthday) = &config.birthday {
        if !birthday.is_empty() {
            lines.push(format!("- Birthday: {}", birthday));
            // Calculate age from birthday
            if let Ok(bd) = chrono::NaiveDate::parse_from_str(birthday, "%Y-%m-%d") {
                let today = user_local_today(&config.timezone);
                let mut age = today.year() - bd.year();
                if today.ordinal() < bd.ordinal() {
                    age -= 1;
                }
                if age >= 0 {
                    lines.push(format!("- Age: {}", age));
                }
                // Check if today is their birthday
                if today.month() == bd.month() && today.day() == bd.day() {
                    lines.push("- 🎂 Today is the user's birthday! Feel free to wish them a happy birthday warmly.".to_string());
                }
            }
        }
    }
    push_if(&mut lines, "Role", &config.role);
    push_if(&mut lines, "AI experience level", &config.ai_experience);
    if let Some(code) = config
        .language
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let name = language_display_name(code);
        lines.push(format!(
            "- Preferred language: {name} ({code}) — reply in this language unless the user explicitly switches"
        ));
    }
    push_if(&mut lines, "Timezone", &config.timezone);
    push_if(&mut lines, "Response style", &config.response_style);

    if let Some(info) = &config.custom_info {
        if !info.is_empty() {
            lines.push(format!("- Additional info: {}", info));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(format!("# User\n\n{}", lines.join("\n")))
    }
}

/// Map a language code (e.g. "zh-CN") to its native display name.
fn language_display_name(code: &str) -> &str {
    match code {
        "zh-CN" => "简体中文",
        "zh-TW" => "繁體中文",
        "en" => "English",
        "ja" => "日本語",
        "ko" => "한국어",
        "es" => "Español",
        "pt" => "Português",
        "ru" => "Русский",
        "ar" => "العربية",
        "tr" => "Türkçe",
        "vi" => "Tiếng Việt",
        "ms" => "Bahasa Melayu",
        other => other,
    }
}

/// Resolve the user's effective IANA timezone name.
///
/// Priority chain:
/// 1. `UserConfig.timezone` (explicitly set by user in profile)
/// 2. `iana-time-zone` auto-detect (host zone — correct for desktop, UTC in Docker)
/// 3. `AppConfig.server.default_timezone` (server operator configured default)
/// 4. `"UTC"` (last resort)
///
/// Use this instead of `chrono::Local` or `iana_time_zone::get_timezone()`
/// directly: in server/Docker mode the host zone is UTC, but the user may be
/// in e.g. Asia/Shanghai.
pub fn effective_timezone() -> String {
    // 1. User explicitly set timezone in profile
    if let Some(tz) = load_user_config()
        .ok()
        .and_then(|cfg| cfg.timezone)
        .filter(|s| !s.trim().is_empty())
    {
        return tz;
    }
    // 2. Auto-detect host timezone (correct on desktop, UTC in Docker)
    if let Ok(tz) = iana_time_zone::get_timezone() {
        if !tz.trim().is_empty() && tz != "UTC" {
            return tz;
        }
    }
    // 3. Server operator's default (e.g. "Asia/Shanghai" in ha-settings.json)
    if let Some(tz) = crate::config::cached_config()
        .server
        .default_timezone
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return tz.to_string();
    }
    // 4. Last resort
    "UTC".to_string()
}

/// Return the current timestamp in the user's effective timezone as RFC3339.
///
/// Use this instead of `chrono::Utc::now().to_rfc3339()` for all **database
/// writes and API responses** so that stored/displayed times are in the user's
/// local timezone. For internal time arithmetic (expiry checks, diff
/// calculations, etc.) continue using `chrono::Utc::now()` directly.
pub fn now_local_rfc3339() -> String {
    now_local_rfc3339_opts(chrono::SecondsFormat::AutoSi)
}

/// Same as `now_local_rfc3339` but with configurable sub-second precision.
/// Used by `util::now_rfc3339()` which needs millisecond precision for
/// lexicographic ordering in DB columns.
pub fn now_local_rfc3339_opts(precision: chrono::SecondsFormat) -> String {
    let tz_name = effective_timezone();
    let now = chrono::Utc::now();
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => now.with_timezone(&tz).to_rfc3339_opts(precision, false),
        Err(_) => now.to_rfc3339_opts(precision, true),
    }
}

/// Return today's date in the user's configured (or auto-detected) timezone.
/// Falls back to UTC when neither is available.
///
/// This replaces `chrono::Local::now().date_naive()` in contexts where the
/// server's local timezone (e.g. a Docker container running UTC) is NOT the
/// user's timezone.
pub fn user_local_today(tz_override: &Option<String>) -> NaiveDate {
    let tz_name = if let Some(tz) = tz_override.as_deref().filter(|s| !s.trim().is_empty()) {
        tz.to_string()
    } else {
        effective_timezone()
    };

    let now = chrono::Utc::now();
    match tz_name.parse::<chrono_tz::Tz>() {
        Ok(tz) => now.with_timezone(&tz).date_naive(),
        Err(_) => now.date_naive(),
    }
}
