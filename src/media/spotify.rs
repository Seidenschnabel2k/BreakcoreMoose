//! Spotify links. Spotify only supplies metadata (titles, artists, durations);
//! each track is matched to a YouTube video when it starts and played from
//! there.
//!
//! Tracks and albums come from the Web API when `SPOTIFY_CLIENT_ID` and
//! `SPOTIFY_CLIENT_SECRET` are set. Since February 2026 the API returns
//! playlist contents only to a logged-in user who owns the playlist (never
//! to an app token) and has no artist top tracks, so playlists and artists
//! come from the public embed page (`open.spotify.com/embed/...`). That page
//! is also the fallback without credentials or when the API fails. It lists
//! at most about 100 tracks of a playlist.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use super::ytdlp::{self, Resolved};
use super::{Stream, Track, ytmusic};
use crate::config::{Config, SpotifyConfig};

const API: &str = "https://api.spotify.com/v1";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const TIMEOUT: Duration = Duration::from_secs(15);
/// Unplayable rows (local files, episodes) fetched beyond what is needed,
/// so they don't eat into the playlist limit.
const PAGING_SLACK: usize = 20;
/// A YouTube hit counts as the same recording within this many seconds.
const DURATION_TOLERANCE: u64 = 7;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        // Every request goes to a fixed Spotify host; so must redirects,
        // since short links are made by a third party (Branch) and anyone
        // can post one.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && is_spotify_host(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .expect("building the HTTP client")
});
/// App access token and when it expires.
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::const_new(None);
/// Spotify track link → matched YouTube link, so seeks, pauses, loops and
/// repeats don't search again. Lives until restart.
static MATCHES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Playlist,
    Artist,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Album => "album",
            Kind::Playlist => "playlist",
            Kind::Artist => "artist",
        }
    }

    fn from_path(s: &str) -> Option<Self> {
        Some(match s {
            "track" => Kind::Track,
            "album" => Kind::Album,
            "playlist" => Kind::Playlist,
            "artist" => Kind::Artist,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: Kind,
    pub id: String,
}

/// Recognizes `spotify:track:<id>` URIs and `open.spotify.com` links (with
/// `intl-xx/` or `embed/` in front, and any query string). `spotify.link`
/// short links are recognized by [`is_short_link`] and expanded in
/// [`resolve_short_link`].
pub fn parse(input: &str) -> Option<Link> {
    if let Some(rest) = input.strip_prefix("spotify:") {
        // Old playlist URIs: spotify:user:<name>:playlist:<id>.
        let parts: Vec<&str> = rest.split(':').collect();
        let [.., kind, id] = parts.as_slice() else {
            return None;
        };
        return link(kind, id);
    }
    let url = Url::parse(input).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str()? != "open.spotify.com" {
        return None;
    }
    let mut segments = url.path_segments()?.filter(|s| !s.is_empty()).peekable();
    while segments
        .peek()
        .is_some_and(|s| s.starts_with("intl-") || *s == "embed")
    {
        segments.next();
    }
    let parts: Vec<&str> = segments.collect();
    match parts.as_slice() {
        [kind, id, ..] if Kind::from_path(kind).is_some() => link(kind, id),
        // Old playlist links: /user/<name>/playlist/<id>.
        ["user", _, kind, id, ..] => link(kind, id),
        _ => None,
    }
}

fn link(kind: &str, id: &str) -> Option<Link> {
    let valid = !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    Some(Link {
        kind: Kind::from_path(kind)?,
        id: valid.then(|| id.to_string())?,
    })
}

/// `https://spotify.link/...` and `https://spotify.app.link/...`, the share
/// links of the mobile apps.
pub fn is_short_link(input: &str) -> bool {
    Url::parse(input).is_ok_and(|u| {
        is_spotify_host(&u) && matches!(u.host_str(), Some("spotify.link" | "spotify.app.link"))
    })
}

/// https on the default port to Spotify's own hosts.
fn is_spotify_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.host_str().is_some_and(|h| {
            matches!(h, "spotify.link" | "spotify.app.link" | "spotify.com")
                || h.ends_with(".spotify.com")
        })
}

/// Follows a short link to the `open.spotify.com` link it stands for.
async fn resolve_short_link(input: &str) -> Result<Link> {
    let resp = HTTP
        .get(input)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .context("Spotify is unreachable")?;
    let url = resp.url().clone();
    if let Some(link) = parse(url.as_str()) {
        return Ok(link);
    }
    // Some short links land on a page that names the target only in its HTML.
    let html = resp.text().await.unwrap_or_default();
    html.split(['"', '\''])
        .filter(|s| s.starts_with("https://open.spotify.com/"))
        .find_map(parse)
        .context("that short link doesn't lead to a Spotify track, album, playlist or artist")
}

fn web_url(kind: Kind, id: &str) -> String {
    format!("https://open.spotify.com/{}/{id}", kind.path())
}

/// Spotify input to tracks. Fetches metadata only; YouTube is searched when
/// a track starts ([`youtube_url`]). `playlist_skip` drops that many entries
/// from the start of an album, playlist or artist.
pub async fn resolve(
    config: &Config,
    input: &str,
    playlist_skip: usize,
    requested_by: &str,
) -> Result<Resolved> {
    let link = match parse(input) {
        Some(link) => link,
        None if is_short_link(input) => resolve_short_link(input).await?,
        None => bail!("that isn't a Spotify track, album, playlist or artist link"),
    };
    let list = match &config.spotify {
        Some(creds) if matches!(link.kind, Kind::Track | Kind::Album) => match from_api(
            creds,
            &link,
            playlist_skip + config.playlist_limit + PAGING_SLACK,
        )
        .await
        {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!("Spotify API: {e:#}; using the embed page");
                from_embed(&link).await?
            }
        },
        _ => from_embed(&link).await?,
    };
    Ok(into_resolved(
        list,
        &link,
        playlist_skip,
        requested_by,
        config,
    ))
}

/// What a link stands for, before the playlist rules are applied.
#[derive(Debug, Default)]
struct List {
    title: Option<String>,
    entries: Vec<Entry>,
    /// Entries the source has in total, if it says; more than `entries`
    /// when paging stopped early.
    total: Option<usize>,
}

/// One row of an album, playlist or artist. `None` fields are rows that
/// can't be played: podcasts episodes, local files, removed tracks.
#[derive(Debug, Clone, Default, PartialEq)]
struct Entry {
    id: Option<String>,
    name: String,
    artists: Vec<String>,
    duration_ms: Option<u64>,
    image: Option<String>,
}

fn into_resolved(
    list: List,
    link: &Link,
    playlist_skip: usize,
    requested_by: &str,
    config: &Config,
) -> Resolved {
    let is_list = link.kind != Kind::Track;
    let skip = if is_list { playlist_skip } else { 0 };
    let total = list
        .total
        .unwrap_or(list.entries.len())
        .max(list.entries.len());
    let mut skipped = 0;
    let mut tracks = Vec::new();
    for entry in list.entries.into_iter().skip(skip) {
        let too_long = entry
            .duration_ms
            .is_some_and(|ms| ms / 1000 > config.max_track_secs);
        match entry.id.as_deref() {
            Some(id) if !too_long && !entry.name.is_empty() => tracks.push(Track {
                title: match entry.artists.is_empty() {
                    true => entry.name.clone(),
                    false => format!("{} - {}", entry.artists.join(", "), entry.name),
                },
                url: web_url(Kind::Track, id),
                duration_secs: entry.duration_ms.map(|ms| (ms + 500) / 1000),
                thumbnail: entry.image.clone(),
                requested_by: requested_by.to_string(),
                stream: Stream::Spotify,
            }),
            _ => skipped += 1,
        }
    }
    let truncated = total
        .saturating_sub(skip)
        .saturating_sub(skipped)
        .saturating_sub(config.playlist_limit.min(tracks.len()));
    tracks.truncate(config.playlist_limit);
    Resolved {
        tracks,
        playlist_title: list.title.filter(|_| is_list),
        skipped,
        truncated,
    }
}

// ---- Web API ----

async fn token(creds: &SpotifyConfig) -> Result<String> {
    let mut cached = TOKEN.lock().await;
    if let Some((token, expires)) = cached.as_ref()
        && Instant::now() < *expires
    {
        return Ok(token.clone());
    }
    let resp = HTTP
        .post(TOKEN_URL)
        .basic_auth(&creds.client_id, Some(&creds.client_secret))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .context("Spotify login is unreachable")?;
    if !resp.status().is_success() {
        bail!("Spotify refused the client ID/secret ({})", resp.status());
    }
    let doc: Value = resp
        .json()
        .await
        .context("unexpected Spotify login answer")?;
    let token = doc["access_token"]
        .as_str()
        .context("Spotify login answer has no access_token")?
        .to_string();
    // Renew a minute early.
    let lifetime = doc["expires_in"]
        .as_u64()
        .unwrap_or(3600)
        .saturating_sub(60)
        .max(30);
    *cached = Some((
        token.clone(),
        Instant::now() + Duration::from_secs(lifetime),
    ));
    Ok(token)
}

/// GETs an API path (or a full `next` link) and parses the JSON.
async fn get(creds: &SpotifyConfig, path_or_url: &str) -> Result<Value> {
    let url = if path_or_url.starts_with("https://") {
        path_or_url.to_string()
    } else {
        format!("{API}{path_or_url}")
    };
    for retry in [false, true] {
        let resp = HTTP
            .get(&url)
            .bearer_auth(token(creds).await?)
            .send()
            .await
            .context("the Spotify API is unreachable")?;
        match resp.status().as_u16() {
            200 => return resp.json().await.context("unexpected Spotify API answer"),
            // Revoked or expired early: log in again, once.
            401 if !retry => *TOKEN.lock().await = None,
            404 => bail!("Spotify doesn't know that link"),
            429 => bail!("Spotify is rate-limiting the bot, try again later"),
            status => bail!("the Spotify API answered {status}"),
        }
    }
    bail!("Spotify keeps refusing the bot's login")
}

/// A track or album; `need` caps how many album rows are fetched.
async fn from_api(creds: &SpotifyConfig, link: &Link, need: usize) -> Result<List> {
    let id = &link.id;
    if link.kind == Kind::Track {
        let doc = get(creds, &format!("/tracks/{id}")).await?;
        return Ok(List {
            entries: vec![api_entry(&doc, None)],
            ..List::default()
        });
    }
    let doc = get(creds, &format!("/albums/{id}")).await?;
    let image = best_api_image(&doc["images"]);
    let mut list = List {
        title: doc["name"].as_str().map(String::from),
        total: doc["tracks"]["total"].as_u64().map(|n| n as usize),
        entries: Vec::new(),
    };
    let mut page = doc["tracks"].clone();
    loop {
        let rows = page["items"].as_array().into_iter().flatten();
        list.entries
            .extend(rows.map(|t| api_entry(t, image.clone())));
        let Some(next) = page["next"].as_str().filter(|n| n.starts_with(API)) else {
            break;
        };
        if list.entries.len() >= need {
            break;
        }
        match get(creds, next).await {
            Ok(next) => page = next,
            // Keep what came; `total` counts the rest as over the limit.
            Err(e) => {
                tracing::warn!("Spotify API, album page: {e:#}");
                break;
            }
        }
    }
    Ok(list)
}

/// A track object from the API. Episodes and local files get no id.
fn api_entry(t: &Value, album_image: Option<String>) -> Entry {
    let playable = t["type"].as_str().is_none_or(|k| k == "track") && t["is_local"] != true;
    Entry {
        id: t["id"].as_str().filter(|_| playable).map(String::from),
        name: t["name"].as_str().unwrap_or_default().to_string(),
        artists: t["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str())
            .map(String::from)
            .collect(),
        duration_ms: t["duration_ms"].as_u64(),
        image: album_image.or_else(|| best_api_image(&t["album"]["images"])),
    }
}

/// The API lists images largest first.
fn best_api_image(images: &Value) -> Option<String> {
    images[0]["url"].as_str().map(String::from)
}

// ---- Embed page ----

async fn from_embed(link: &Link) -> Result<List> {
    let url = format!(
        "https://open.spotify.com/embed/{}/{}",
        link.kind.path(),
        link.id
    );
    let resp = HTTP
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .context("Spotify is unreachable")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("Spotify doesn't know that link");
    }
    if !resp.status().is_success() {
        bail!("Spotify answered {}", resp.status());
    }
    let html = resp.text().await.context("reading the Spotify page")?;
    parse_embed(&html, link.kind)
}

fn parse_embed(html: &str, kind: Kind) -> Result<List> {
    const START: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let json = html
        .split_once(START)
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(json, _)| json)
        .context("Spotify changed its embed page")?;
    let doc: Value = serde_json::from_str(json).context("Spotify changed its embed page")?;
    let entity = &doc["props"]["pageProps"]["state"]["data"]["entity"];
    if !entity.is_object() {
        bail!("Spotify doesn't know that link");
    }
    let image = best_embed_image(&entity["visualIdentity"]["image"]);

    if kind == Kind::Track {
        return Ok(List {
            entries: vec![Entry {
                id: entity["id"].as_str().map(String::from),
                name: entity["title"].as_str().unwrap_or_default().to_string(),
                artists: entity["artists"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["name"].as_str())
                    .map(String::from)
                    .collect(),
                duration_ms: entity["duration"].as_u64(),
                image,
            }],
            ..List::default()
        });
    }

    // Album rows share the album cover; playlist and artist rows have none.
    let row_image = image.filter(|_| kind == Kind::Album);
    let entries = entity["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| Entry {
            id: row["uri"]
                .as_str()
                .and_then(|uri| uri.strip_prefix("spotify:track:"))
                .map(String::from),
            name: row["title"].as_str().unwrap_or_default().to_string(),
            // "KAROL G,\u{a0}Judeline"
            artists: row["subtitle"]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .map(|a| a.replace('\u{a0}', " ").trim().to_string())
                .filter(|a| !a.is_empty())
                .collect(),
            duration_ms: row["duration"].as_u64(),
            image: row_image.clone(),
        })
        .collect();
    let title = entity["title"].as_str().map(|t| match kind {
        Kind::Artist => format!("{t}: top tracks"),
        _ => t.to_string(),
    });
    Ok(List {
        title,
        entries,
        total: None,
    })
}

fn best_embed_image(images: &Value) -> Option<String> {
    images
        .as_array()?
        .iter()
        .max_by_key(|i| i["maxWidth"].as_u64().unwrap_or(0))?["url"]
        .as_str()
        .map(String::from)
}

// ---- YouTube match ----

/// The YouTube video to play for a Spotify track: the best YouTube Music
/// song hit for "artists - title", else the best plain YouTube hit. "Best"
/// is the highest-ranked hit within a few seconds of Spotify's duration,
/// falling back to the top hit.
pub async fn youtube_url(config: &Config, track: &Track) -> Result<String> {
    if let Some(url) = MATCHES.lock().await.get(&track.url) {
        return Ok(url.clone());
    }
    let want = track.duration_secs;
    let music = ytmusic::search(&HTTP, &track.title)
        .await
        .inspect_err(|e| tracing::debug!("YouTube Music search failed: {e:#}"))
        .unwrap_or_default();
    let found = match pick(&music, want) {
        Some(hit) => hit.url.clone(),
        None => {
            let videos = ytdlp::search(config, "ytsearch", &track.title, 5)
                .await
                .unwrap_or_default();
            pick(&videos, want)
                .or(music.first())
                .or(videos.first())
                .with_context(|| format!("no YouTube match for {}", track.title))?
                .url
                .clone()
        }
    };
    tracing::info!(spotify = %track.url, youtube = %found, "matched {}", track.title);
    MATCHES
        .lock()
        .await
        .insert(track.url.clone(), found.clone());
    Ok(found)
}

/// The first of the top five hits whose length is close to `want`, or the
/// top hit when the length is unknown.
fn pick(hits: &[Track], want: Option<u64>) -> Option<&Track> {
    let Some(want) = want else {
        return hits.first();
    };
    hits.iter().take(5).find(|h| {
        h.duration_secs
            .is_some_and(|d| d.abs_diff(want) <= DURATION_TOLERANCE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_links() {
        let track = |id: &str| {
            Some(Link {
                kind: Kind::Track,
                id: id.into(),
            })
        };
        assert_eq!(
            parse("https://open.spotify.com/track/0DiWol3AO6WpXZgp0goxAV?si=abc"),
            track("0DiWol3AO6WpXZgp0goxAV")
        );
        assert_eq!(
            parse("https://open.spotify.com/intl-de/track/0DiWol3AO6WpXZgp0goxAV"),
            track("0DiWol3AO6WpXZgp0goxAV")
        );
        assert_eq!(
            parse("spotify:track:0DiWol3AO6WpXZgp0goxAV"),
            track("0DiWol3AO6WpXZgp0goxAV")
        );
        assert_eq!(
            parse("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            Some(Link {
                kind: Kind::Playlist,
                id: "37i9dQZF1DXcBWIGoYBM5M".into()
            })
        );
        assert_eq!(
            parse("spotify:user:someone:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            Some(Link {
                kind: Kind::Playlist,
                id: "37i9dQZF1DXcBWIGoYBM5M".into()
            })
        );
        assert_eq!(
            parse("https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi").map(|l| l.kind),
            Some(Kind::Artist)
        );
        assert_eq!(parse("https://open.spotify.com/show/abc"), None);
        assert_eq!(parse("https://evil.example/track/abc"), None);
        assert_eq!(parse("spotify:track:../../x"), None);
        assert_eq!(parse("daft punk"), None);
        assert!(is_short_link("https://spotify.link/AbCdEf"));
        assert!(!is_short_link("https://open.spotify.com/track/x"));
        assert!(!is_short_link("http://spotify.link/AbCdEf"));
        assert!(!is_short_link("https://spotify.link:8080/AbCdEf"));
        assert!(is_spotify_host(
            &Url::parse("https://www.spotify.com/x").unwrap()
        ));
        assert!(!is_spotify_host(
            &Url::parse("https://evilspotify.com/").unwrap()
        ));
        assert!(!is_spotify_host(&Url::parse("https://127.0.0.1/").unwrap()));
    }

    fn embed_page(entity: Value) -> String {
        let doc =
            serde_json::json!({"props": {"pageProps": {"state": {"data": {"entity": entity}}}}});
        format!(r#"<html><script id="__NEXT_DATA__" type="application/json">{doc}</script></html>"#)
    }

    #[test]
    fn parses_embed_lists() {
        let html = embed_page(serde_json::json!({
            "type": "playlist", "title": "Today’s Top Hits",
            "visualIdentity": {"image": [{"url": "small", "maxWidth": 64}, {"url": "big", "maxWidth": 640}]},
            "trackList": [
                {"uri": "spotify:track:3h5T5JypYU7huFiVYhv1dr", "title": "BbY WOW",
                 "subtitle": "KAROL G,\u{a0}Judeline", "duration": 225834},
                {"uri": "spotify:episode:xyz", "title": "A podcast", "subtitle": "Someone", "duration": 1000}
            ]
        }));
        let list = parse_embed(&html, Kind::Playlist).unwrap();
        assert_eq!(list.title.as_deref(), Some("Today’s Top Hits"));
        assert_eq!(list.entries[0].artists, ["KAROL G", "Judeline"]);
        assert_eq!(list.entries[0].image, None);
        assert_eq!(list.entries[1].id, None);

        let list = parse_embed(&html, Kind::Album).unwrap();
        assert_eq!(list.entries[0].image.as_deref(), Some("big"));

        let html = embed_page(serde_json::json!({
            "type": "track", "id": "0DiWol3AO6WpXZgp0goxAV", "title": "One More Time",
            "artists": [{"name": "Daft Punk"}], "duration": 320357
        }));
        let list = parse_embed(&html, Kind::Track).unwrap();
        assert_eq!(list.entries[0].name, "One More Time");
        assert!(parse_embed("<html></html>", Kind::Track).is_err());
    }

    #[test]
    fn reads_api_tracks() {
        let t = serde_json::json!({
            "type": "track", "id": "abc", "name": "Song", "duration_ms": 200000,
            "artists": [{"name": "A"}, {"name": "B"}],
            "album": {"images": [{"url": "big"}, {"url": "small"}]}
        });
        let e = api_entry(&t, None);
        assert_eq!(e.id.as_deref(), Some("abc"));
        assert_eq!(e.artists, ["A", "B"]);
        assert_eq!(e.image.as_deref(), Some("big"));
        let local = serde_json::json!({"type": "track", "id": "x", "is_local": true, "name": "x"});
        assert_eq!(api_entry(&local, None).id, None);
    }

    #[test]
    fn builds_tracks_with_skip_and_limit() {
        let mut config = crate::media::ytdlp::tests::config();
        config.playlist_limit = 2;
        let entry = |id: &str| Entry {
            id: Some(id.into()),
            name: format!("Song {id}"),
            artists: vec!["Artist".into()],
            duration_ms: Some(180_400),
            image: None,
        };
        let list = List {
            title: Some("Mix".into()),
            entries: vec![
                entry("a"),
                Entry::default(),
                entry("b"),
                entry("c"),
                entry("d"),
            ],
            total: None,
        };
        let link = Link {
            kind: Kind::Playlist,
            id: "p".into(),
        };
        let r = into_resolved(list, &link, 1, "nils", &config);
        assert_eq!(r.playlist_title.as_deref(), Some("Mix"));
        assert_eq!(r.skipped, 1);
        assert_eq!(r.truncated, 1);
        assert_eq!(r.tracks.len(), 2);
        assert_eq!(r.tracks[0].title, "Artist - Song b");
        assert_eq!(r.tracks[0].url, "https://open.spotify.com/track/b");
        assert_eq!(r.tracks[0].duration_secs, Some(180));
        assert_eq!(r.tracks[0].stream, Stream::Spotify);
    }

    #[test]
    fn picks_by_duration() {
        let hit = |url: &str, d: u64| Track {
            url: url.into(),
            duration_secs: Some(d),
            ..Track::test("x")
        };
        let hits = [hit("cover", 250), hit("real", 214), hit("live", 300)];
        assert_eq!(pick(&hits, Some(212)).unwrap().url, "real");
        assert!(pick(&hits, Some(100)).is_none());
        assert_eq!(pick(&hits, None).unwrap().url, "cover");
    }

    /// Needs network and yt-dlp: `cargo test -- --ignored`. One test, because
    /// the static HTTP client's pooled connections die with each test's runtime.
    #[tokio::test]
    #[ignore]
    async fn resolves_and_matches_live() {
        let link = parse("https://open.spotify.com/album/2noRn2Aes5aoNVsU6iWThc").unwrap();
        let list = from_embed(&link).await.unwrap();
        assert_eq!(list.title.as_deref(), Some("Discovery"));
        assert!(list.entries.len() >= 14);
        assert!(list.entries[0].artists.iter().any(|a| a == "Daft Punk"));

        let config = crate::media::ytdlp::tests::config();
        for (link, min) in [
            (
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
                2,
            ),
            ("https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi", 2),
        ] {
            let r = resolve(&config, link, 0, "nils").await.unwrap();
            assert!(r.tracks.len() >= min, "{link}: {r:?}");
        }
        let r = resolve(&config, "spotify:track:0DiWol3AO6WpXZgp0goxAV", 0, "nils")
            .await
            .unwrap();
        assert_eq!(r.tracks[0].title, "Daft Punk - One More Time");
        let url = youtube_url(&config, &r.tracks[0]).await.unwrap();
        assert!(url.starts_with("https://www.youtube.com/watch?v="), "{url}");
    }
}
