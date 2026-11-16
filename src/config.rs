use anyhow::{Context as _, Result, bail};
use std::env;
use std::path::PathBuf;

/// Settings read once at startup from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    /// Fluxer REST base, e.g. `https://api.example.com/v1`.
    pub api_url: String,
    pub prefix: String,
    pub ytdlp_path: String,
    /// Netscape cookie file handed to yt-dlp as `--cookies`, as an absolute
    /// path (yt-dlp streams run in the temp directory).
    pub ytdlp_cookies: Option<PathBuf>,
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
    /// The web player; `None` unless `WEB_PUBLIC_URL` is set.
    pub web: Option<WebConfig>,
    pub jellyfin: Option<JellyfinConfig>,
    /// Web API app credentials. Without them Spotify links still work
    /// through the public embed page.
    pub spotify: Option<SpotifyConfig>,
}

#[derive(Debug, Clone)]
pub struct WebConfig {
    /// Address the HTTP server listens on.
    pub bind: String,
    /// Where people reach the page, e.g. `https://music.example.com`, without
    /// a trailing slash. The OAuth redirect and the WebSocket origin check
    /// are derived from it.
    pub public_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// Encrypts the session cookie. Random per start when unset, which logs
    /// everyone out on restart.
    pub secret: Option<String>,
}

impl WebConfig {
    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }
}

#[derive(Clone)]
pub struct JellyfinConfig {
    pub url: String,
    pub api_key: String,
    /// User whose library and playlists are shown; the first administrator if unset.
    pub user: Option<String>,
}

impl std::fmt::Debug for JellyfinConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JellyfinConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for SpotifyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpotifyConfig")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
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
            ytdlp_cookies: ytdlp_cookies()?,
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
            web: web_config()?,
            jellyfin: jellyfin_config()?,
            spotify: spotify_config()?,
        })
    }

    pub fn is_music_channel(&self, channel_id: &str) -> bool {
        self.music_channels.iter().any(|c| c == channel_id)
    }
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn web_config() -> Result<Option<WebConfig>> {
    let Some(public_url) = var("WEB_PUBLIC_URL") else {
        return Ok(None);
    };
    let public_url = public_url.trim_end_matches('/').to_string();
    url::Url::parse(&public_url)
        .with_context(|| format!("WEB_PUBLIC_URL is not a URL: {public_url}"))?;
    let (Some(client_id), Some(client_secret)) = (
        var("FLUXER_OAUTH_CLIENT_ID"),
        var("FLUXER_OAUTH_CLIENT_SECRET"),
    ) else {
        bail!("WEB_PUBLIC_URL needs FLUXER_OAUTH_CLIENT_ID and FLUXER_OAUTH_CLIENT_SECRET");
    };
    let secret = var("WEB_SECRET");
    if secret.as_ref().is_some_and(|s| s.len() < 32) {
        bail!("WEB_SECRET must be at least 32 characters");
    }
    Ok(Some(WebConfig {
        bind: var("WEB_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()),
        public_url,
        client_id,
        client_secret,
        secret,
    }))
}

fn jellyfin_config() -> Result<Option<JellyfinConfig>> {
    match (var("JELLYFIN_URL"), var("JELLYFIN_API_KEY")) {
        (Some(url), Some(api_key)) => Ok(Some(JellyfinConfig {
            url: url.trim_end_matches('/').to_string(),
            api_key,
            user: var("JELLYFIN_USER"),
        })),
        (None, None) => Ok(None),
        _ => bail!("set both JELLYFIN_URL and JELLYFIN_API_KEY, or neither"),
    }
}

fn spotify_config() -> Result<Option<SpotifyConfig>> {
    match (var("SPOTIFY_CLIENT_ID"), var("SPOTIFY_CLIENT_SECRET")) {
        (Some(client_id), Some(client_secret)) => Ok(Some(SpotifyConfig {
            client_id,
            client_secret,
        })),
        (None, None) => Ok(None),
        _ => bail!("set both SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET, or neither"),
    }
}

fn parse_var<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("{name} is not a valid number: {v}")),
        Err(_) => Ok(default),
    }
}

/// `YTDLP_COOKIES` if set (it must exist), else `cookies.txt` in the working
/// directory if there is one.
fn ytdlp_cookies() -> Result<Option<PathBuf>> {
    let (path, required) = match env::var("YTDLP_COOKIES") {
        Ok(p) if !p.is_empty() => (PathBuf::from(p), true),
        _ => (PathBuf::from("cookies.txt"), false),
    };
    match std::path::absolute(&path) {
        Ok(abs) if abs.is_file() => Ok(Some(abs)),
        _ if required => bail!("YTDLP_COOKIES: {} is not a file", path.display()),
        _ => Ok(None),
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
    Ok(if api.ends_with("/v1") {
        api.to_string()
    } else {
        format!("{api}/v1")
    })
}
