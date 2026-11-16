pub mod jellyfin;
pub mod source;
pub mod spotify;
pub mod ytdlp;
pub mod ytmusic;

use serde::Serialize;

/// One playable queue item. Only metadata: audio is opened when it starts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track {
    pub title: String,
    /// Link shown to people. For yt-dlp tracks also what yt-dlp plays.
    pub url: String,
    pub duration_secs: Option<u64>,
    pub thumbnail: Option<String>,
    pub requested_by: String,
    #[serde(skip)]
    pub stream: Stream,
}

/// Where the audio comes from at play time.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Stream {
    /// yt-dlp downloads `Track::url`.
    #[default]
    Ytdlp,
    /// ffmpeg reads this item straight from the configured Jellyfin server.
    Jellyfin(String),
    /// `Track::url` is a Spotify track; a YouTube match is searched when it
    /// starts and yt-dlp plays that.
    Spotify,
}

#[cfg(test)]
impl Track {
    pub fn test(title: &str) -> Self {
        Self {
            title: title.into(),
            url: "https://www.youtube.com/watch?v=x".into(),
            duration_secs: Some(214),
            thumbnail: None,
            requested_by: "nils".into(),
            stream: Stream::Ytdlp,
        }
    }
}

/// Formats seconds as `m:ss` or `h:mm:ss`.
pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_duration;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(5), "0:05");
        assert_eq!(fmt_duration(225), "3:45");
        assert_eq!(fmt_duration(3725), "1:02:05");
    }
}
