//! Decides what a `!play` argument is before anything is fetched.

use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// A YouTube link; yt-dlp decides if it is a video or a playlist.
    YouTube(String),
    Search(String),
    /// A link to some other site. Refused: yt-dlp's generic extractor would
    /// fetch any URL, including ones on the home network.
    Unsupported,
}

const YOUTUBE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
];

pub fn classify(input: &str) -> Source {
    let input = input.trim().trim_start_matches('<').trim_end_matches('>');
    if !input.starts_with("http://") && !input.starts_with("https://") {
        return Source::Search(input.to_string());
    }
    match Url::parse(input) {
        Ok(url) if url.host_str().is_some_and(|h| YOUTUBE_HOSTS.contains(&h)) => {
            Source::YouTube(url.to_string())
        }
        _ => Source::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_links() {
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/playlist?list=PL123",
            "<https://youtu.be/dQw4w9WgXcQ>",
        ] {
            assert!(matches!(classify(link), Source::YouTube(_)), "{link}");
        }
    }

    #[test]
    fn search_and_other_sites() {
        assert_eq!(classify("daft punk one more time"), Source::Search("daft punk one more time".into()));
        assert_eq!(classify("https://example.com/a.mp3"), Source::Unsupported);
        assert_eq!(classify("http://192.168.1.1/"), Source::Unsupported);
        assert_eq!(classify("https://youtube.com.evil.org/watch?v=x"), Source::Unsupported);
    }
}
