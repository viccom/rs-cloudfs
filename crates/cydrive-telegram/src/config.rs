//! Telegram transport configuration: baseline public client credentials,
//! session stem, and the transport-side config record.

/// Public Android client credentials mirrored from the Python baseline
/// (config.py:12-13, contract 3) — these are public app credentials, not secrets.
pub const DEFAULT_API_ID: i32 = 6;
pub const DEFAULT_API_HASH: &str = "eb06d4abfb49dc3eeb1aeb98ae0f581e";
/// Default session file stem, contract 3 (`cynet_bot_session`).
pub const DEFAULT_SESSION_STEM: &str = "cynet_bot_session";

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
    pub chat_id: i64,
    pub session_path: std::path::PathBuf,
}

impl Default for TransportConfig {
    // Public baseline credentials, empty bot token, chat 0, session file
    // `<stem>.session` relative to the working directory (Python baseline
    // keeps its session file in the CWD as well).
    fn default() -> Self {
        Self {
            api_id: DEFAULT_API_ID,
            api_hash: DEFAULT_API_HASH.to_owned(),
            bot_token: String::new(),
            chat_id: 0,
            session_path: format!("{DEFAULT_SESSION_STEM}.session").into(),
        }
    }
}
