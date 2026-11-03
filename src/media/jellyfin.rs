//! A Jellyfin server as a music source: search, albums, playlists, artists
//! and instant mixes. Audio is read by ffmpeg straight from the server.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use super::{Stream, Track};
use crate::config::JellyfinConfig;

const TIMEOUT: Duration = Duration::from_secs(20);
const ITEM_FIELDS: &str = "ChildCount,ProductionYear";
/// Most tracks one album, playlist, artist or mix adds.
const MAX_TRACKS: usize = 500;
const MIX_SIZE: usize = 100;

pub struct Jellyfin {
    config: JellyfinConfig,
    http: reqwest::Client,
    user_id: OnceCell<String>,
    /// `https://music.example.com`, so queue thumbnails go through our image proxy.
    public_url: Option<String>,
}

/// What a search or listing shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Song,
    Album,
    Playlist,
    Artist,
}

impl Kind {
    fn item_type(self) -> &'static str {
        match self {
            Kind::Song => "Audio",
            Kind::Album => "MusicAlbum",
            Kind::Playlist => "Playlist",
            Kind::Artist => "MusicArtist",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Items {
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Item {
    id: String,
    name: Option<String>,
    #[serde(rename = "Type")]
    kind: Option<String>,
    run_time_ticks: Option<u64>,
    album: Option<String>,
    album_artist: Option<String>,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    image_tags: HashMap<String, String>,
    album_id: Option<String>,
    album_primary_image_tag: Option<String>,
    child_count: Option<u32>,
    production_year: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct User {
    id: String,
    name: String,
    #[serde(default)]
    policy: UserPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct UserPolicy {
    #[serde(default)]
    is_administrator: bool,
}

/// One row in the web player's Jellyfin tab.
#[derive(Debug, Serialize)]
pub struct Hit {
    pub id: String,
    pub kind: &'static str,
    pub title: String,
    /// Artist, album, year or track count, whatever fits the kind.
    pub subtitle: String,
    pub duration_secs: Option<u64>,
    pub thumbnail: Option<String>,
}

impl Item {
    fn duration_secs(&self) -> Option<u64> {
        self.run_time_ticks.map(|t| t / 10_000_000)
    }

    fn artist(&self) -> Option<&str> {
        self.artists
            .first()
            .or(self.album_artist.as_ref())
            .map(String::as_str)
            .filter(|a| !a.is_empty())
    }

    /// The item whose primary image to show: its own, else its album's.
    fn image_item(&self) -> Option<&str> {
        if self.image_tags.contains_key("Primary") {
            return Some(&self.id);
        }
        self.album_id
            .as_deref()
            .filter(|_| self.album_primary_image_tag.is_some())
    }

    fn kind_name(&self) -> &'static str {
        match self.kind.as_deref() {
            Some("Audio") => "song",
            Some("MusicAlbum") => "album",
            Some("Playlist") => "playlist",
            Some("MusicArtist") => "artist",
            _ => "other",
        }
    }
}

/// Jellyfin ids are 32 hex digits; anything else never reaches a URL.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Jellyfin {
    pub fn new(config: JellyfinConfig, public_url: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            config,
            http,
            user_id: OnceCell::new(),
            public_url,
        }
    }

    fn auth_header(&self) -> String {
        format!("MediaBrowser Token=\"{}\"", self.config.api_key)
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.config.url))
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await
            .context("Jellyfin is unreachable")?;
        if !resp.status().is_success() {
            bail!("Jellyfin answered {}", resp.status());
        }
        resp.json().await.context("unexpected Jellyfin response")
    }

    /// The user whose library we browse, looked up once.
    async fn user_id(&self) -> Result<&str> {
        self.user_id
            .get_or_try_init(|| async {
                let users: Vec<User> = self.get("/Users", &[]).await?;
                let user = match &self.config.user {
                    Some(name) => users
                        .iter()
                        .find(|u| u.name.eq_ignore_ascii_case(name) || &u.id == name),
                    None => users
                        .iter()
                        .find(|u| u.policy.is_administrator)
                        .or(users.first()),
                };
                let user = user.with_context(|| match &self.config.user {
                    Some(name) => format!("no Jellyfin user called {name}"),
                    None => "Jellyfin has no users".into(),
                })?;
                tracing::info!(user = %user.name, "using Jellyfin library");
                Ok(user.id.clone())
            })
            .await
            .map(String::as_str)
    }

    async fn items(&self, extra: &[(&str, &str)]) -> Result<Vec<Item>> {
        let user = self.user_id().await?;
        let mut query = vec![
            ("userId", user),
            ("recursive", "true"),
            ("fields", ITEM_FIELDS),
        ];
        query.extend_from_slice(extra);
        Ok(self.get::<Items>("/Items", &query).await?.items)
    }

    /// Searches, or lists the library when `query` is empty: playlists by
    /// name, albums and songs newest first, artists by name.
    pub async fn search(&self, kind: Kind, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let limit = limit.to_string();
        let mut q = vec![
            ("includeItemTypes", kind.item_type()),
            ("limit", limit.as_str()),
        ];
        if !query.trim().is_empty() {
            q.push(("searchTerm", query.trim()));
        } else if matches!(kind, Kind::Album | Kind::Song) {
            q.extend([("sortBy", "DateCreated"), ("sortOrder", "Descending")]);
        } else {
            q.push(("sortBy", "SortName"));
        }
        if kind == Kind::Playlist {
            q.push(("mediaTypes", "Audio"));
        }
        Ok(self.items(&q).await?.iter().map(|i| self.hit(i)).collect())
    }

    /// The tracks of a song, album, playlist or artist, in order. Also
    /// returns the container's name.
    pub async fn tracks(&self, id: &str, requested_by: &str) -> Result<(String, Vec<Track>)> {
        if !valid_id(id) {
            bail!("not a Jellyfin id");
        }
        let item = self
            .items(&[("ids", id)])
            .await?
            .into_iter()
            .next()
            .context("not found in Jellyfin")?;
        let name = item.name.clone().unwrap_or_default();
        let max = MAX_TRACKS.to_string();
        let items = match item.kind.as_deref() {
            Some("Audio") => vec![item],
            Some("MusicAlbum") => {
                self.items(&[
                    ("parentId", id),
                    ("includeItemTypes", "Audio"),
                    ("sortBy", "ParentIndexNumber,IndexNumber,SortName"),
                    ("limit", &max),
                ])
                .await?
            }
            Some("MusicArtist") => {
                self.items(&[
                    ("artistIds", id),
                    ("includeItemTypes", "Audio"),
                    (
                        "sortBy",
                        "ProductionYear,Album,ParentIndexNumber,IndexNumber",
                    ),
                    ("limit", &max),
                ])
                .await?
            }
            Some("Playlist") => {
                let user = self.user_id().await?;
                let path = format!("/Playlists/{id}/Items");
                let q = [("userId", user), ("fields", ITEM_FIELDS), ("limit", &max)];
                self.get::<Items>(&path, &q).await?.items
            }
            other => bail!("can't play a Jellyfin {}", other.unwrap_or("item")),
        };
        Ok((name, self.to_tracks(items, requested_by)))
    }

    /// Jellyfin's instant mix: similar songs seeded by any item.
    pub async fn instant_mix(&self, id: &str, requested_by: &str) -> Result<(String, Vec<Track>)> {
        if !valid_id(id) {
            bail!("not a Jellyfin id");
        }
        let user = self.user_id().await?;
        let seed = self
            .items(&[("ids", id)])
            .await?
            .into_iter()
            .next()
            .context("not found in Jellyfin")?;
        let path = format!("/Items/{id}/InstantMix");
        let size = MIX_SIZE.to_string();
        let items = self
            .get::<Items>(&path, &[("userId", user), ("limit", &size)])
            .await?
            .items;
        let name = format!("Mix: {}", seed.name.unwrap_or_default());
        Ok((name, self.to_tracks(items, requested_by)))
    }

    fn to_tracks(&self, items: Vec<Item>, requested_by: &str) -> Vec<Track> {
        items
            .into_iter()
            .filter(|i| i.kind.as_deref() == Some("Audio"))
            .map(|i| Track {
                title: match i.artist() {
                    Some(artist) => format!("{artist} - {}", i.name.as_deref().unwrap_or("?")),
                    None => i.name.clone().unwrap_or_else(|| "?".into()),
                },
                url: format!("{}/web/#/details?id={}", self.config.url, i.id),
                duration_secs: i.duration_secs(),
                thumbnail: self.thumbnail(&i),
                requested_by: requested_by.to_string(),
                stream: Stream::Jellyfin(i.id),
            })
            .take(MAX_TRACKS)
            .collect()
    }

    fn hit(&self, i: &Item) -> Hit {
        let subtitle = match i.kind_name() {
            "song" => [i.artist(), i.album.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
            "album" => {
                let year = i.production_year.map(|y| y.to_string());
                [i.artist(), year.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
            _ => i
                .child_count
                .map(|n| format!("{n} items"))
                .unwrap_or_default(),
        };
        Hit {
            id: i.id.clone(),
            kind: i.kind_name(),
            title: i.name.clone().unwrap_or_default(),
            subtitle,
            duration_secs: i.duration_secs(),
            thumbnail: i.image_item().map(|id| format!("/img/jellyfin/{id}")),
        }
    }

    fn thumbnail(&self, i: &Item) -> Option<String> {
        let base = self.public_url.as_ref()?;
        Some(format!("{base}/img/jellyfin/{}", i.image_item()?))
    }

    /// Cover art for the image proxy: content type and bytes.
    pub async fn image(&self, id: &str) -> Result<(String, Vec<u8>)> {
        if !valid_id(id) {
            bail!("not a Jellyfin id");
        }
        let resp = self
            .http
            .get(format!("{}/Items/{id}/Images/Primary", self.config.url))
            .header("Authorization", self.auth_header())
            .query(&[("maxHeight", "300"), ("quality", "85")])
            .send()
            .await?
            .error_for_status()?;
        let kind = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.starts_with("image/"))
            .unwrap_or("image/jpeg")
            .to_string();
        Ok((kind, resp.bytes().await?.to_vec()))
    }

    /// What ffmpeg opens to play an item: the URL and the auth header.
    pub fn stream_request(config: &JellyfinConfig, id: &str) -> (String, String) {
        (
            format!("{}/Audio/{id}/stream?static=true", config.url),
            format!(
                "Authorization: MediaBrowser Token=\"{}\"\r\n",
                config.api_key
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jellyfin() -> Jellyfin {
        let config = JellyfinConfig {
            url: "http://jf.lan:8096".into(),
            api_key: "k".into(),
            user: None,
        };
        Jellyfin::new(config, Some("https://music.example.com".into()))
    }

    #[test]
    fn items_become_tracks() {
        let items: Items = serde_json::from_str(
            r#"{"Items":[
                {"Id":"0123456789abcdef0123456789abcdef","Name":"Song","Type":"Audio","RunTimeTicks":2140000000,
                 "Artists":["Band"],"AlbumId":"fedcba9876543210fedcba9876543210","AlbumPrimaryImageTag":"t"},
                {"Id":"11111111111111111111111111111111","Name":"Folder","Type":"Folder"}
            ],"TotalRecordCount":2}"#,
        )
        .unwrap();
        let jf = jellyfin();
        let tracks = jf.to_tracks(items.items, "nils");
        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!(t.title, "Band - Song");
        assert_eq!(t.duration_secs, Some(214));
        assert_eq!(
            t.stream,
            Stream::Jellyfin("0123456789abcdef0123456789abcdef".into())
        );
        assert_eq!(
            t.thumbnail.as_deref(),
            Some("https://music.example.com/img/jellyfin/fedcba9876543210fedcba9876543210")
        );
    }

    #[test]
    fn ids_are_checked() {
        assert!(valid_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_id("../Users"));
        assert!(!valid_id("0123456789abcdef0123456789abcde"));
    }
}
