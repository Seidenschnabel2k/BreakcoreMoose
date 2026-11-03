//! Song search on YouTube Music. yt-dlp's YouTube Music search returns
//! channels rather than songs, so this asks the site's own search endpoint
//! with its "songs" filter. Playback still goes through yt-dlp. From some
//! networks it answers every search with "No results"; callers fall back
//! to a plain YouTube search then.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::{Stream, Track};

const URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const CLIENT_VERSION: &str = "1.20251015.03.00";
/// The "Songs" filter of the search page.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D";

pub async fn search(http: &reqwest::Client, query: &str) -> Result<Vec<Track>> {
    let body = json!({
        "context": {"client": {"clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en"}},
        "query": query,
        "params": SONGS_FILTER,
    });
    let resp = http
        .post(URL)
        .header("Origin", "https://music.youtube.com")
        .header("User-Agent", "Mozilla/5.0")
        .timeout(Duration::from_secs(15))
        .json(&body)
        .send()
        .await
        .context("YouTube Music is unreachable")?;
    if !resp.status().is_success() {
        bail!("YouTube Music answered {}", resp.status());
    }
    let doc: Value = resp
        .json()
        .await
        .context("unexpected YouTube Music response")?;
    let mut tracks = Vec::new();
    collect(&doc, &mut tracks);
    Ok(tracks)
}

/// Finds every song row anywhere in the response.
fn collect(v: &Value, out: &mut Vec<Track>) {
    match v {
        Value::Object(map) => {
            if let Some(row) = map.get("musicResponsiveListItemRenderer") {
                out.extend(row_to_track(row));
                return;
            }
            map.values().for_each(|v| collect(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

fn column_text(row: &Value, i: usize) -> Option<String> {
    let runs = row["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
        .as_array()?;
    Some(runs.iter().filter_map(|r| r["text"].as_str()).collect())
}

fn row_to_track(row: &Value) -> Option<Track> {
    let id = row["playlistItemData"]["videoId"].as_str()?;
    let title = column_text(row, 0)?;
    // "Artist • Album • 3:45"
    let details = column_text(row, 1).unwrap_or_default();
    let parts: Vec<&str> = details.split(" • ").collect();
    let duration_secs = parts.last().and_then(|d| parse_duration(d));
    let artist = parts
        .first()
        .filter(|_| parts.len() > 1)
        .filter(|a| !a.is_empty());
    let thumbnail = row["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]
        .as_array()
        .and_then(|t| t.last())
        .and_then(|t| t["url"].as_str())
        .map(String::from);
    Some(Track {
        title: match artist {
            Some(artist) => format!("{artist} - {title}"),
            None => title,
        },
        url: format!("https://www.youtube.com/watch?v={id}"),
        duration_secs,
        thumbnail,
        requested_by: String::new(),
        stream: Stream::Ytdlp,
    })
}

/// `3:45` or `1:02:03`.
fn parse_duration(s: &str) -> Option<u64> {
    let mut secs = 0u64;
    for part in s.trim().split(':') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_song_rows() {
        let doc = json!({"contents": {"tabs": [{"sections": [{"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": "wU26xVT_vBU"},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "One More Time"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Daft Punk"}, {"text": " • "}, {"text": "Discovery"}, {"text": " • "}, {"text": "5:21"}]}}}
            ],
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [{"url": "small"}, {"url": "big"}]}}}
        }}, {"musicResponsiveListItemRenderer": {"flexColumns": []}}]}]}});
        let mut tracks = Vec::new();
        collect(&doc, &mut tracks);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Daft Punk - One More Time");
        assert_eq!(tracks[0].url, "https://www.youtube.com/watch?v=wU26xVT_vBU");
        assert_eq!(tracks[0].duration_secs, Some(321));
        assert_eq!(tracks[0].thumbnail.as_deref(), Some("big"));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("1.2M plays"), None);
    }

    /// Needs network: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn searches_live() {
        let tracks = search(&reqwest::Client::new(), "daft punk one more time")
            .await
            .unwrap();
        // Some addresses (VPNs, datacenters) get "No results" for everything;
        // the web search then falls back to plain YouTube via yt-dlp.
        assert!(
            tracks.len() >= 5,
            "no YouTube Music results from this network"
        );
        assert!(tracks.iter().any(|t| t.title.contains("Daft Punk")));
    }
}
