//! JSON endpoints: who am I, search, Jellyfin browsing and cover art.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::auth::Session;
use crate::media::jellyfin::{self, Kind};
use crate::media::{Track, ytdlp, ytmusic};

const SEARCH_RESULTS: usize = 25;
const JELLYFIN_RESULTS: usize = 60;
/// What a search costs from the user's action budget (it runs yt-dlp or a request).
const SEARCH_COST: f64 = 3.0;

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (status, msg.into())
}

/// The user may use this server's player: the bot is in it and so are they.
pub async fn check_guild(state: &AppState, session: &Session, guild_id: &str) -> ApiResult<String> {
    let name = state
        .bot_guilds()
        .await
        .into_iter()
        .find(|(id, _)| id == guild_id)
        .map(|(_, name)| name);
    let Some(name) = name else {
        return Err(err(StatusCode::NOT_FOUND, "The bot isn't in that server."));
    };
    if !state.is_member(guild_id, &session.user_id).await {
        return Err(err(
            StatusCode::FORBIDDEN,
            "You're not a member of that server.",
        ));
    }
    Ok(name)
}

pub async fn me(State(state): State<AppState>, session: Option<Session>) -> Response {
    let Some(session) = session else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "not logged in"})),
        )
            .into_response();
    };
    let mut guilds = Vec::new();
    for (id, name) in state.bot_guilds().await {
        if state.is_member(&id, &session.user_id).await {
            guilds.push(json!({"id": id, "name": name}));
        }
    }
    Json(json!({
        "user": {"id": session.user_id, "name": session.name},
        "guilds": guilds,
        "jellyfin": state.jellyfin.is_some(),
    }))
    .into_response()
}

/// One row in a search tab. Songs carry a `token` to add them by; Jellyfin
/// rows carry the item `id` instead (albums, playlists and mixes expand
/// server-side when added).
#[derive(Serialize)]
pub struct Row {
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    kind: &'static str,
    title: String,
    subtitle: String,
    duration_secs: Option<u64>,
    thumbnail: Option<String>,
    url: Option<String>,
}

impl Row {
    fn from_track(state: &AppState, track: Track, subtitle: String) -> Self {
        Row {
            kind: "song",
            title: track.title.clone(),
            subtitle,
            duration_secs: track.duration_secs,
            thumbnail: track.thumbnail.clone(),
            url: Some(track.url.clone()),
            id: None,
            token: Some(state.remember_hit(track)),
        }
    }
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    kind: Option<Kind>,
}

pub async fn search(
    State(state): State<AppState>,
    session: Session,
    Path((guild_id, source)): Path<(String, String)>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Vec<Row>>> {
    check_guild(&state, &session, &guild_id).await?;
    if !state.allow(&session.user_id, SEARCH_COST) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "Slow down a little."));
    }
    let q = query.q.trim();
    if q.chars().count() > 200 {
        return Err(err(StatusCode::BAD_REQUEST, "That search is too long."));
    }
    if q.is_empty() && source != "jellyfin" {
        return Ok(Json(Vec::new()));
    }

    let rows = match source.as_str() {
        "youtube" => {
            let tracks = match ytmusic::search(&state.http, q).await {
                Ok(t) if !t.is_empty() => t,
                // YouTube Music changes its page now and then; plain YouTube still works.
                result => {
                    if let Err(e) = result {
                        tracing::warn!("YouTube Music search failed, using yt-dlp: {e:#}");
                    }
                    ytdlp::search(&state.config, "ytsearch", q, SEARCH_RESULTS)
                        .await
                        .map_err(bad_gateway)?
                }
            };
            tracks
                .into_iter()
                .take(SEARCH_RESULTS)
                .map(|t| Row::from_track(&state, t, "YouTube".into()))
                .collect()
        }
        "soundcloud" => ytdlp::search(&state.config, "scsearch", q, SEARCH_RESULTS)
            .await
            .map_err(bad_gateway)?
            .into_iter()
            .map(|t| Row::from_track(&state, t, "SoundCloud".into()))
            .collect(),
        "jellyfin" => {
            let jellyfin = state
                .jellyfin
                .as_ref()
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "Jellyfin isn't set up."))?;
            jellyfin
                .search(query.kind.unwrap_or(Kind::Song), q, JELLYFIN_RESULTS)
                .await
                .map_err(bad_gateway)?
                .into_iter()
                .map(|h| Row {
                    token: None,
                    id: Some(h.id),
                    kind: h.kind,
                    title: h.title,
                    subtitle: h.subtitle,
                    duration_secs: h.duration_secs,
                    thumbnail: h.thumbnail,
                    url: None,
                })
                .collect()
        }
        _ => return Err(err(StatusCode::NOT_FOUND, "Unknown source.")),
    };
    Ok(Json(rows))
}

fn bad_gateway(e: anyhow::Error) -> (StatusCode, String) {
    err(StatusCode::BAD_GATEWAY, format!("Search failed: {e:#}"))
}

/// The songs inside a Jellyfin album, playlist or artist, to browse into.
pub async fn jellyfin_tracks(
    State(state): State<AppState>,
    session: Session,
    Path((guild_id, id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    check_guild(&state, &session, &guild_id).await?;
    if !state.allow(&session.user_id, SEARCH_COST) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "Slow down a little."));
    }
    let jellyfin = state
        .jellyfin
        .as_ref()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Jellyfin isn't set up."))?;
    let (name, tracks) = jellyfin
        .tracks(&id, &session.name)
        .await
        .map_err(bad_gateway)?;
    let rows: Vec<Row> = tracks
        .into_iter()
        .map(|t| {
            let id = match &t.stream {
                crate::media::Stream::Jellyfin(id) => Some(id.clone()),
                crate::media::Stream::Ytdlp => None,
            };
            Row {
                token: None,
                id,
                kind: "song",
                title: t.title,
                subtitle: String::new(),
                duration_secs: t.duration_secs,
                thumbnail: t.thumbnail,
                url: None,
            }
        })
        .collect();
    Ok(Json(json!({"name": name, "rows": rows})))
}

/// Cover art from Jellyfin, which usually isn't reachable from outside.
/// Public, so Fluxer can show it in the music channel panel too.
pub async fn jellyfin_image(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(jellyfin) = &state.jellyfin else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !jellyfin::valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match jellyfin.image(&id).await {
        Ok((kind, bytes)) => (
            [
                (header::CONTENT_TYPE, kind),
                (header::CACHE_CONTROL, "public, max-age=86400".into()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
