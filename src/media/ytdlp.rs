//! yt-dlp for metadata at add time and for the audio stream at play time.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::oneshot;

use super::Track;
use super::source::Source;
use crate::config::Config;
use crate::voice::{CHANNELS, SAMPLE_RATE};

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);
const SEARCH_RESULTS: usize = 5;

/// What a `!play` argument expanded to.
#[derive(Debug)]
pub struct Resolved {
    pub tracks: Vec<Track>,
    /// Set when the input was a playlist.
    pub playlist_title: Option<String>,
    /// Private, deleted, live or too-long entries that were dropped.
    pub skipped: usize,
    /// Entries past the playlist limit.
    pub truncated: usize,
}

#[derive(Debug, Deserialize)]
struct Info {
    #[serde(rename = "_type")]
    kind: Option<String>,
    id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    duration: Option<f64>,
    live_status: Option<String>,
    thumbnail: Option<String>,
    #[serde(default)]
    thumbnails: Vec<Thumbnail>,
    /// Extractor name; `ie_key` on flat playlist entries.
    #[serde(alias = "ie_key")]
    extractor_key: Option<String>,
    #[serde(default)]
    entries: Vec<Info>,
}

#[derive(Debug, Deserialize)]
struct Thumbnail {
    url: String,
}

impl Info {
    fn is_youtube(&self) -> bool {
        self.extractor_key.as_deref().is_some_and(|k| k.starts_with("Youtube"))
    }

    fn is_live(&self) -> bool {
        matches!(self.live_status.as_deref(), Some("is_live" | "is_upcoming"))
    }

    fn is_unavailable(&self) -> bool {
        matches!(self.title.as_deref(), Some("[Private video]" | "[Deleted video]"))
    }

    fn duration_secs(&self) -> Option<u64> {
        self.duration.map(|d| d.round() as u64)
    }

    fn watch_url(&self) -> Option<String> {
        self.webpage_url
            .clone()
            .or_else(|| self.url.clone().filter(|u| u.starts_with("http")))
            .or_else(|| {
                // Flat YouTube entries sometimes carry only the id.
                let id = self.id.as_ref().filter(|_| self.is_youtube())?;
                Some(format!("https://www.youtube.com/watch?v={id}"))
            })
    }

    fn thumbnail_url(&self) -> Option<String> {
        self.thumbnail
            .clone()
            // yt-dlp sorts thumbnails worst to best.
            .or_else(|| self.thumbnails.last().map(|t| t.url.clone()))
            .or_else(|| {
                let id = self.id.as_ref().filter(|_| self.is_youtube())?;
                Some(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
            })
    }

    fn into_track(self, requested_by: &str) -> Option<Track> {
        let url = self.watch_url()?;
        Some(Track {
            thumbnail: self.thumbnail_url(),
            duration_secs: self.duration_secs(),
            title: self.title.unwrap_or_else(|| url.clone()),
            url,
            requested_by: requested_by.to_string(),
        })
    }
}

/// Turns a link or search text into tracks. Fetches metadata only.
/// `playlist_skip` drops that many entries from the start of a playlist
/// (counted as the playlist shows them, unavailable ones included); single
/// videos ignore it.
pub async fn resolve(config: &Config, source: Source, playlist_skip: usize, requested_by: &str) -> Result<Resolved> {
    let (target, is_search) = match source {
        Source::Url(url) => {
            super::source::ensure_public(&url).await?;
            (url, false)
        }
        Source::Search(q) => (format!("ytsearch{SEARCH_RESULTS}:{q}"), true),
    };

    let mut command = Command::new(&config.ytdlp_path);
    // --no-playlist: a watch link with &list= (or a mix) adds just that video.
    command.args(["-J", "--flat-playlist", "--no-playlist", "--no-warnings"]);
    if playlist_skip > 0 && !is_search {
        command.args(["--playlist-items", &format!("{}:", playlist_skip + 1)]);
    }
    let output = tokio::time::timeout(
        RESOLVE_TIMEOUT,
        command
            .args(["--", &target])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("yt-dlp took too long")?
    .context("could not run yt-dlp")?;

    if !output.status.success() {
        bail!("{}", ytdlp_error(&String::from_utf8_lossy(&output.stderr)));
    }
    let info: Info = serde_json::from_slice(&output.stdout).context("unexpected yt-dlp output")?;
    Ok(from_info(info, is_search, requested_by, config))
}

fn from_info(info: Info, is_search: bool, requested_by: &str, config: &Config) -> Resolved {
    let too_long = |i: &Info| i.duration_secs().is_some_and(|d| d > config.max_track_secs);
    let playable = |i: &Info| !i.is_live() && !i.is_unavailable() && !too_long(i);

    if is_search {
        let tracks = info
            .entries
            .into_iter()
            .find(playable)
            .and_then(|e| e.into_track(requested_by))
            .into_iter()
            .collect();
        return Resolved { tracks, playlist_title: None, skipped: 0, truncated: 0 };
    }

    if info.kind.as_deref() != Some("playlist") {
        let skipped = usize::from(!playable(&info));
        let tracks = Some(info)
            .filter(playable)
            .and_then(|i| i.into_track(requested_by))
            .into_iter()
            .collect();
        return Resolved { tracks, playlist_title: None, skipped, truncated: 0 };
    }

    let total = info.entries.len();
    let mut tracks: Vec<Track> = info
        .entries
        .into_iter()
        .filter(playable)
        .filter_map(|e| e.into_track(requested_by))
        .collect();
    let skipped = total - tracks.len();
    let truncated = tracks.len().saturating_sub(config.playlist_limit);
    tracks.truncate(config.playlist_limit);
    Resolved { tracks, playlist_title: info.title, skipped, truncated }
}

/// Picks yt-dlp's `ERROR:` line, falling back to the last line of stderr.
fn ytdlp_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed");
    line.trim_start_matches("ERROR:").trim().to_string()
}

/// A running yt-dlp → ffmpeg pipe that yields raw PCM. Dropping it kills both
/// processes.
pub struct AudioStream {
    pub pcm: ChildStdout,
    _ytdlp: Child,
    _ffmpeg: Child,
    ytdlp_stderr: oneshot::Receiver<String>,
}

impl AudioStream {
    /// yt-dlp downloads (stream URLs expire and are IP-bound, so ffmpeg never
    /// sees one) and ffmpeg decodes to 48 kHz stereo s16le.
    /// `start_secs` > 0 seeks: ffmpeg decodes and drops audio up to that
    /// point, so a seek costs only the download up to it (a few seconds).
    pub fn open(config: &Config, url: &str, start_secs: u64) -> Result<Self> {
        let mut ytdlp = Command::new(&config.ytdlp_path)
            .args(["-f", "bestaudio/best", "--no-playlist", "--quiet", "--no-warnings"])
            // yt-dlp's own HLS downloader (SoundCloud, Twitch, …) writes `--FragN`
            // files to the working directory even when streaming to stdout;
            // ffmpeg streams HLS without touching disk.
            .args(["--downloader", "m3u8:ffmpeg", "-o", "-", "--", url])
            // Anything another downloader still writes lands here, not in the repo.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("could not run yt-dlp")?;

        let ytdlp_out: Stdio = ytdlp.stdout.take().context("yt-dlp: no stdout")?.try_into()?;
        let mut ffmpeg = Command::new(&config.ffmpeg_path)
            .args(["-hide_banner", "-loglevel", "error"])
            .args(["-ss", &start_secs.to_string(), "-i", "pipe:0", "-vn"])
            .args(["-filter:a", &format!("volume={}", f32::from(config.volume) / 100.0)])
            .args(["-f", "s16le", "-ar", &SAMPLE_RATE.to_string(), "-ac", &CHANNELS.to_string()])
            .arg("pipe:1")
            .stdin(ytdlp_out)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("could not run ffmpeg")?;

        // Drain stderr so yt-dlp never blocks on a full pipe; keep it for errors.
        let stderr = ytdlp.stderr.take().context("yt-dlp: no stderr")?;
        let (tx, ytdlp_stderr) = oneshot::channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut text = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "yt-dlp", "{line}");
                text.push_str(&line);
                text.push('\n');
            }
            let _ = tx.send(text);
        });

        Ok(Self {
            pcm: ffmpeg.stdout.take().context("ffmpeg: no stdout")?,
            _ytdlp: ytdlp,
            _ffmpeg: ffmpeg,
            ytdlp_stderr,
        })
    }

    /// After the PCM ends: yt-dlp's error, if it printed one.
    pub async fn error(self) -> Option<String> {
        let stderr = tokio::time::timeout(Duration::from_secs(2), self.ytdlp_stderr).await.ok()?.ok()?;
        stderr.contains("ERROR:").then(|| ytdlp_error(&stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            token: String::new(),
            api_url: String::new(),
            prefix: "!".into(),
            ytdlp_path: "yt-dlp".into(),
            ffmpeg_path: "ffmpeg".into(),
            max_track_secs: 3 * 60 * 60,
            playlist_limit: 2,
            volume: 50,
            music_channels: Vec::new(),
        }
    }

    fn parse(json: &str) -> Info {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn search_takes_first_non_live_hit() {
        let info = parse(r#"{"_type":"playlist","entries":[
            {"_type":"url","id":"live1","title":"Live radio","live_status":"is_live"},
            {"_type":"url","id":"abc","url":"https://www.youtube.com/watch?v=abc","title":"Song","duration":214}
        ]}"#);
        let r = from_info(info, true, "nils", &config());
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].title, "Song");
        assert_eq!(r.tracks[0].duration_secs, Some(214));
    }

    #[test]
    fn playlist_drops_unavailable_and_caps() {
        let info = parse(r#"{"_type":"playlist","title":"Mix","entries":[
            {"id":"a","title":"One","duration":100,"ie_key":"Youtube"},
            {"id":"b","title":"[Private video]"},
            {"id":"c","title":"Two","duration":99999},
            {"id":"d","title":"Three","duration":100,"ie_key":"Youtube"},
            {"id":"e","title":"Four","duration":100,"ie_key":"Youtube"}
        ]}"#);
        let r = from_info(info, false, "nils", &config());
        assert_eq!(r.playlist_title.as_deref(), Some("Mix"));
        let titles: Vec<_> = r.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["One", "Three"]);
        assert_eq!(r.tracks[0].url, "https://www.youtube.com/watch?v=a");
        assert_eq!(r.tracks[0].thumbnail.as_deref(), Some("https://i.ytimg.com/vi/a/hqdefault.jpg"));
        assert_eq!((r.skipped, r.truncated), (2, 1));
    }

    #[test]
    fn other_sites() {
        // A single SoundCloud track: `url` is the raw stream, `webpage_url` the page.
        let track = parse(r#"{"_type":"video","id":"293","webpage_url":"https://soundcloud.com/forss/flickermood",
            "url":"https://playback.example/x.m3u8","title":"Flickermood","duration":213.9,
            "thumbnail":"https://i1.sndcdn.com/a.jpg","extractor_key":"Soundcloud"}"#);
        let r = from_info(track, false, "nils", &config());
        assert_eq!(r.tracks[0].url, "https://soundcloud.com/forss/flickermood");
        assert_eq!(r.tracks[0].thumbnail.as_deref(), Some("https://i1.sndcdn.com/a.jpg"));

        // Flat SoundCloud set entries have a page URL but no title, and no YouTube fallbacks.
        let set = parse(r#"{"_type":"playlist","title":"Soulhack","entries":[
            {"_type":"url_transparent","id":"290","url":"https://soundcloud.com/forss/city-ports","ie_key":"Soundcloud"},
            {"_type":"url","id":"291","ie_key":"Soundcloud"}
        ]}"#);
        let r = from_info(set, false, "nils", &config());
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].title, "https://soundcloud.com/forss/city-ports");
        assert_eq!(r.tracks[0].thumbnail, None);
    }

    #[test]
    fn single_video_and_live_refusal() {
        let video = parse(r#"{"_type":"video","id":"x","webpage_url":"https://www.youtube.com/watch?v=x","title":"Vid","duration":213.4,"live_status":"not_live"}"#);
        let r = from_info(video, false, "nils", &config());
        assert_eq!(r.tracks.len(), 1);
        assert!(r.playlist_title.is_none());

        let live = parse(r#"{"_type":"video","id":"y","title":"Stream","live_status":"is_live"}"#);
        let r = from_info(live, false, "nils", &config());
        assert!(r.tracks.is_empty());
        assert_eq!(r.skipped, 1);
    }

    /// Needs network and yt-dlp/ffmpeg: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn streams_and_resolves_live() {
        use tokio::io::AsyncReadExt;
        let config = config();

        let r = resolve(&config, Source::Search("rick astley never gonna give you up".into()), 0, "t").await.unwrap();
        assert_eq!(r.tracks.len(), 1);

        let playlist = || Source::Url("https://www.youtube.com/playlist?list=PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI".into());
        let config = Config { playlist_limit: 500, ..config };
        let full = resolve(&config, playlist(), 0, "t").await.unwrap();
        let skipped = resolve(&config, playlist(), 10, "t").await.unwrap();
        assert!(full.tracks.len() > 20);
        assert!(full.tracks.iter().any(|t| t.url == skipped.tracks[0].url), "skipped list starts inside the playlist");
        assert!(!full.tracks[..5].iter().any(|t| t.url == skipped.tracks[0].url), "first tracks were skipped");

        let mut audio = AudioStream::open(&config, &r.tracks[0].url, 60).unwrap();
        let mut second = vec![0u8; 48_000 * 2 * 2];
        tokio::time::timeout(Duration::from_secs(30), audio.pcm.read_exact(&mut second))
            .await
            .expect("no PCM within 30 s")
            .unwrap();
        assert!(second.iter().any(|&b| b != 0), "PCM is all silence");
    }

    #[test]
    fn error_line() {
        assert_eq!(ytdlp_error("[youtube] x: Downloading\nERROR: [youtube] x: Video unavailable\n"), "[youtube] x: Video unavailable");
        assert_eq!(ytdlp_error("something odd\n"), "something odd");
    }
}
