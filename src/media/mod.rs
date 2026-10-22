pub mod source;
pub mod ytdlp;

/// One playable queue item. Only metadata: audio is opened when it starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub title: String,
    /// Watch URL handed to yt-dlp at play time.
    pub url: String,
    pub duration_secs: Option<u64>,
    pub thumbnail: Option<String>,
    pub requested_by: String,
}

/// Formats seconds as `m:ss` or `h:mm:ss`.
pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
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
