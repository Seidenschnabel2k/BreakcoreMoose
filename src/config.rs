use anyhow::{Context as _, Result, bail};
use std::env;

/// Settings read once at startup from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    /// Fluxer REST base, e.g. `https://api.example.com/v1`.
    pub api_url: String,
    pub prefix: String,
    pub ytdlp_path: String,
    pub ffmpeg_path: String,
    /// Single tracks longer than this are refused.
    pub max_track_secs: u64,
    /// Most tracks one playlist link may add.
    pub playlist_limit: usize,
    /// Output volume in percent of the source, 0–200.
    pub volume: u16,
    /// Channels where the bot keeps a now-playing panel, plays bare messages
    /// and deletes everything else. At most one per server.
    pub music_channels: Vec<String>,
}

impl Config {
    pub async fn from_env() -> Result<Self> {
        let token = env::var("FLUXER_TOKEN").context("FLUXER_TOKEN is not set")?;
        let api_url = match env::var("FLUXER_API_URL") {
            Ok(url) => url.trim_end_matches('/').to_string(),
            Err(_) => match env::var("FLUXER_URL") {
                Ok(instance) => discover_api_url(&instance).await?,
                Err(_) => bail!("set FLUXER_URL (your instance) or FLUXER_API_URL"),
            },
        };

        Ok(Self {
            token,
            api_url,
            prefix: env::var("BOT_PREFIX").unwrap_or_else(|_| "!".into()),
            ytdlp_path: env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".into()),
            ffmpeg_path: env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
            max_track_secs: parse_var("MAX_TRACK_SECONDS", 3 * 60 * 60)?,
            playlist_limit: parse_var("PLAYLIST_LIMIT", 500)?,
            volume: match parse_var("VOLUME", 50)? {
                v @ 0..=200 => v,
                v => bail!("VOLUME must be 0–200 (percent), got {v}"),
            },
            music_channels: env::var("MUSIC_CHANNEL_ID")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(String::from)
                .collect(),
        })
    }

    pub fn is_music_channel(&self, channel_id: &str) -> bool {
        self.music_channels.iter().any(|c| c == channel_id)
    }
}

fn parse_var<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(v) => v.parse().map_err(|_| anyhow::anyhow!("{name} is not a valid number: {v}")),
        Err(_) => Ok(default),
    }
}

/// Reads the API base URL from the instance's `/.well-known/fluxer` document.
async fn discover_api_url(instance: &str) -> Result<String> {
    let url = format!("{}/.well-known/fluxer", instance.trim_end_matches('/'));
    let doc: serde_json::Value = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("fetching {url}"))?
        .json()
        .await
        .with_context(|| format!("parsing {url}"))?;

    if doc["features"]["voice_enabled"].as_bool() == Some(false) {
        tracing::warn!("this instance reports voice_enabled = false; joining voice will fail");
    }

    let api = doc["endpoints"]["api"]
        .as_str()
        .with_context(|| format!("{url} has no endpoints.api"))?
        .trim_end_matches('/');
    Ok(if api.ends_with("/v1") { api.to_string() } else { format!("{api}/v1") })
}
