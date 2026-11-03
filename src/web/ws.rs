//! One WebSocket per open page: the player's state and queue go out as they
//! change, the page's controls come in and become player messages.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::AppState;
use super::api::check_guild;
use super::auth::Session;
use crate::commands::{self, ask};
use crate::core::spectrum::Levels;
use crate::core::{
    Placement, PlayerMsg, Repeat, SeekTarget, Snapshot, USER_MAX_VOLUME, from_user_volume,
    to_user_volume,
};
use crate::media::{source, ytdlp};

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_ITEMS: usize = 200;
/// Membership is checked again this often while a page stays open.
const RECHECK: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ClientMsg {
    Add {
        items: Vec<AddItem>,
        /// Into the priority lane.
        #[serde(default)]
        next: bool,
        /// Right after this entry (a drop between two rows).
        after: Option<u64>,
        /// At the very top (a drop above the first row).
        #[serde(default)]
        top: bool,
    },
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    PlayEntry {
        id: u64,
    },
    Seek {
        secs: u64,
    },
    Volume {
        percent: u16,
    },
    Repeat {
        mode: Repeat,
    },
    Shuffle,
    Clear,
    Remove {
        ids: Vec<u64>,
    },
    /// Put `id` after `after`; `None` moves it to the top.
    Move {
        id: u64,
        after: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AddItem {
    /// A search result, by the token the search handed out.
    Hit { token: String },
    /// A Jellyfin song, album, playlist or artist; `mix` adds its instant mix instead.
    Jellyfin {
        id: String,
        #[serde(default)]
        mix: bool,
    },
    /// A link or search text, like `!play`.
    Query { text: String },
}

impl ClientMsg {
    fn cost(&self) -> f64 {
        match self {
            ClientMsg::Add { items, .. } => {
                1.0 + items
                    .iter()
                    .filter(|i| !matches!(i, AddItem::Hit { .. }))
                    .count() as f64
                    * 2.0
            }
            _ => 1.0,
        }
    }
}

pub async fn upgrade(
    State(state): State<AppState>,
    session: Session,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Browsers send cookies on cross-site WebSocket requests; the origin tells us who opened it.
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    if origin != Some(expected_origin(&state.web.public_url).as_str()) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    let guild_name = match check_guild(&state, &session, &guild_id).await {
        Ok(name) => name,
        Err(e) => return e.into_response(),
    };
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, state, session, guild_id, guild_name))
}

fn expected_origin(public_url: &str) -> String {
    url::Url::parse(public_url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default()
}

async fn run(
    mut socket: WebSocket,
    state: AppState,
    session: Session,
    guild_id: String,
    guild_name: String,
) {
    let player = state.players.get(&guild_id).await;
    let Some((mut snapshots, mut levels)) =
        ask(&player, |reply| PlayerMsg::Subscribe { reply }).await
    else {
        return;
    };
    // Replies from actions that run in the background (adds can take seconds).
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(32);

    let hello = json!({
        "t": "hello",
        "user": {"id": session.user_id, "name": session.name},
        "guild": {"id": guild_id, "name": guild_name},
        "jellyfin": state.jellyfin.is_some(),
        "max_volume": USER_MAX_VOLUME,
    });
    if send(&mut socket, &hello).await.is_err() {
        return;
    }
    let mut sent_queue = None;
    let first = snapshots.borrow_and_update().clone();
    if push_snapshot(&mut socket, &state, &first, &mut sent_queue)
        .await
        .is_err()
    {
        return;
    }
    let mut recheck = tokio::time::interval(RECHECK);
    recheck.tick().await;

    loop {
        tokio::select! {
            msg = socket.recv() => {
                let text = match msg {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                let msg = match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = send(&mut socket, &error(format!("Bad request: {e}"))).await;
                        continue;
                    }
                };
                if !state.allow(&session.user_id, msg.cost()) {
                    let _ = send(&mut socket, &error("Slow down a little.")).await;
                    continue;
                }
                let (state, session, guild_id, player, out) =
                    (state.clone(), session.clone(), guild_id.clone(), player.clone(), out_tx.clone());
                tokio::spawn(async move {
                    if let Some(reply) = handle(&state, &session, &guild_id, &player, msg).await {
                        let _ = out.send(reply).await;
                    }
                });
            }
            Some(reply) = out_rx.recv() => {
                if send(&mut socket, &reply).await.is_err() { break; }
            }
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let snapshot = snapshots.borrow_and_update().clone();
                if push_snapshot(&mut socket, &state, &snapshot, &mut sent_queue).await.is_err() { break; }
            }
            changed = levels.changed() => {
                if changed.is_err() { break; }
                let l: Levels = *levels.borrow_and_update();
                if send(&mut socket, &json!({"t": "levels", "v": l})).await.is_err() { break; }
            }
            _ = recheck.tick() => {
                if !state.is_member(&guild_id, &session.user_id).await {
                    let _ = send(&mut socket, &error("You're no longer a member of this server.")).await;
                    break;
                }
            }
        }
    }
}

async fn send(socket: &mut WebSocket, value: &Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(value.to_string().into())).await
}

fn error(text: impl Into<String>) -> Value {
    json!({"t": "error", "text": text.into()})
}

fn notice(text: impl Into<String>) -> Value {
    json!({"t": "notice", "text": text.into()})
}

/// Sends the state, and the queue too if it changed since the last send.
async fn push_snapshot(
    socket: &mut WebSocket,
    state: &AppState,
    s: &Snapshot,
    sent_queue: &mut Option<u64>,
) -> Result<(), axum::Error> {
    if *sent_queue != Some(s.queue_version) {
        let entries: Vec<Value> = s
            .queue
            .iter()
            .map(|e| json!({"id": e.id, "track": e.track}))
            .collect();
        send(socket, &json!({"t": "queue", "version": s.queue_version, "entries": entries, "priority": s.upcoming_priority}))
            .await?;
        *sent_queue = Some(s.queue_version);
    }
    let voice = match &s.voice_channel {
        Some(id) => Some(json!({"id": id, "name": channel_name(state, id).await})),
        None => None,
    };
    let now = s.now.as_ref().map(|n| {
        json!({
            "entry_id": n.entry_id,
            "track": n.track,
            "position_secs": n.position_secs(),
            "paused": n.paused,
        })
    });
    send(
        socket,
        &json!({
            "t": "state",
            "now": now,
            "volume": to_user_volume(s.volume, state.config.volume),
            "repeat": s.repeat,
            "voice": voice,
            "queue_secs": s.upcoming_secs,
        }),
    )
    .await
}

async fn channel_name(state: &AppState, channel_id: &str) -> Option<String> {
    let ctx = state.gateway.context().await?;
    ctx.cache
        .channels
        .read()
        .await
        .get(channel_id)
        .and_then(|c| c.name.clone())
}

/// Runs one page action. Returns a message for this page only, if any.
async fn handle(
    state: &AppState,
    session: &Session,
    guild_id: &str,
    player: &mpsc::Sender<PlayerMsg>,
    msg: ClientMsg,
) -> Option<Value> {
    tracing::info!(guild = guild_id, user = %session.name, ?msg, "web");
    let voice_channel = state.gateway.user_voice(guild_id, &session.user_id).await;
    let ctx = state.gateway.context().await;
    let no_ctx = || error("The bot isn't connected to Fluxer right now.");
    let gone = || error("The player stopped unexpectedly.");

    match msg {
        ClientMsg::Add {
            items,
            next,
            after,
            top,
        } => {
            let Some(ctx) = ctx else {
                return Some(no_ctx());
            };
            if items.is_empty() || items.len() > MAX_ITEMS {
                return Some(error(format!(
                    "Add between 1 and {MAX_ITEMS} items at a time."
                )));
            }
            let (tracks, label, problems) = collect_tracks(state, session, items).await;
            if tracks.is_empty() {
                return Some(error(
                    problems
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "Nothing playable found.".into()),
                ));
            }
            let count = tracks.len();
            let placement = match (after, top, next) {
                (Some(id), _, _) => Placement::After(id),
                (None, true, _) => Placement::Front,
                (None, false, true) => Placement::Next,
                (None, false, false) => Placement::End,
            };
            let reply = ask(player, |reply| PlayerMsg::Enqueue {
                ctx,
                tracks,
                placement,
                voice_channel,
                text_channel: None,
                reply,
            })
            .await;
            Some(match reply {
                Some(Ok(_)) => {
                    let mut text = match (count, label) {
                        (1, Some(title)) => format!("Added {title}."),
                        (n, Some(title)) => format!("Added {n} tracks from {title}."),
                        (n, None) => format!("Added {n} tracks."),
                    };
                    if !problems.is_empty() {
                        text.push_str(&format!(" ({} couldn't be added)", problems.len()));
                    }
                    notice(text)
                }
                Some(Err(e)) => error(strip_markdown(&commands::join_error(
                    &e,
                    &state.config.prefix,
                ))),
                None => gone(),
            })
        }
        ClientMsg::Play => {
            let Some(ctx) = ctx else {
                return Some(no_ctx());
            };
            result(
                ask(player, |reply| PlayerMsg::Play {
                    ctx,
                    voice_channel,
                    reply,
                })
                .await,
            )
        }
        ClientMsg::PlayEntry { id } => {
            let Some(ctx) = ctx else {
                return Some(no_ctx());
            };
            result(
                ask(player, |reply| PlayerMsg::PlayEntry {
                    ctx,
                    voice_channel,
                    id,
                    reply,
                })
                .await,
            )
        }
        ClientMsg::Pause => result(ask(player, |reply| PlayerMsg::Pause { reply }).await),
        ClientMsg::Stop => {
            ask(player, |reply| PlayerMsg::Stop { reply }).await;
            None
        }
        ClientMsg::Next => match ask(player, |reply| PlayerMsg::Skip { reply })
            .await
            .flatten()
        {
            Some(_) => None,
            None => Some(error("Nothing is playing.")),
        },
        ClientMsg::Prev => result(ask(player, |reply| PlayerMsg::Previous { reply }).await),
        ClientMsg::Seek { secs } => {
            let target = SeekTarget::To(secs);
            match ask(player, |reply| PlayerMsg::Seek { target, reply }).await {
                Some(Ok(_)) => None,
                Some(Err(e)) => Some(error(e)),
                None => Some(gone()),
            }
        }
        ClientMsg::Volume { percent } => {
            let percent = from_user_volume(percent, state.config.volume);
            let _ = player.send(PlayerMsg::SetVolume { percent }).await;
            None
        }
        ClientMsg::Repeat { mode } => {
            let _ = player.send(PlayerMsg::SetRepeat { mode }).await;
            None
        }
        ClientMsg::Shuffle => {
            let n = ask(player, |reply| PlayerMsg::Shuffle { reply })
                .await
                .unwrap_or(0);
            Some(notice(if n < 2 {
                "Not enough tracks to shuffle.".into()
            } else {
                format!("Shuffled {n} tracks.")
            }))
        }
        ClientMsg::Clear => {
            ask(player, |reply| PlayerMsg::Clear { reply }).await;
            None
        }
        ClientMsg::Remove { ids } => {
            if ids.len() > 5000 {
                return Some(error("Too many at once."));
            }
            ask(player, |reply| PlayerMsg::Remove { ids, reply }).await;
            None
        }
        ClientMsg::Move { id, after } => {
            match ask(player, |reply| PlayerMsg::Move { id, after, reply }).await {
                // The page moved the row already; a refused move snaps back with the next queue.
                Some(false) => Some(json!({"t": "resync"})),
                _ => None,
            }
        }
    }
}

fn result(reply: Option<Result<(), String>>) -> Option<Value> {
    match reply {
        Some(Ok(())) => None,
        Some(Err(e)) => Some(error(strip_markdown(&e))),
        None => Some(error("The player stopped unexpectedly.")),
    }
}

/// Chat texts use `code` and **bold**; the page shows plain text.
fn strip_markdown(text: &str) -> String {
    text.replace("**", "").replace('`', "")
}

/// Turns the requested items into tracks. Returns the tracks, a name for the
/// reply (one track's title, or the album/playlist), and what failed.
async fn collect_tracks(
    state: &AppState,
    session: &Session,
    items: Vec<AddItem>,
) -> (Vec<crate::media::Track>, Option<String>, Vec<String>) {
    let mut tracks = Vec::new();
    let mut label = None;
    let mut problems = Vec::new();
    let single = items.len() == 1;

    for item in items {
        match item {
            AddItem::Hit { token } => match state.hit(&token) {
                Some(mut track) => {
                    track.requested_by = session.name.clone();
                    if single {
                        label = Some(track.title.clone());
                    }
                    tracks.push(track);
                }
                None => problems.push("That search result expired; search again.".into()),
            },
            AddItem::Jellyfin { id, mix } => {
                let Some(jellyfin) = &state.jellyfin else {
                    problems.push("Jellyfin isn't set up.".into());
                    continue;
                };
                let result = if mix {
                    jellyfin.instant_mix(&id, &session.name).await
                } else {
                    jellyfin.tracks(&id, &session.name).await
                };
                match result {
                    Ok((name, found)) => {
                        if single {
                            label = Some(if found.len() == 1 {
                                found[0].title.clone()
                            } else {
                                name
                            });
                        }
                        tracks.extend(found);
                    }
                    Err(e) => problems.push(format!("Jellyfin: {e:#}")),
                }
            }
            AddItem::Query { text } => {
                let (query, skip) = commands::split_playlist_skip(&text);
                if query.is_empty() || query.chars().count() > 2000 {
                    problems.push("Enter a link or search.".into());
                    continue;
                }
                match ytdlp::resolve(&state.config, source::classify(query), skip, &session.name)
                    .await
                {
                    Ok(resolved) if resolved.tracks.is_empty() => {
                        problems.push("Nothing playable found.".into())
                    }
                    Ok(resolved) => {
                        if single {
                            label = resolved
                                .playlist_title
                                .or_else(|| resolved.tracks.first().map(|t| t.title.clone()));
                        }
                        tracks.extend(resolved.tracks);
                    }
                    Err(e) => problems.push(format!("Couldn't load that: {e}")),
                }
            }
        }
    }
    tracks.truncate(state.config.playlist_limit.max(1) * 2);
    (tracks, label, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_page_messages() {
        let m: ClientMsg = serde_json::from_str(r#"{"op":"move","id":4,"after":null}"#).unwrap();
        assert!(matches!(m, ClientMsg::Move { id: 4, after: None }));
        let m: ClientMsg = serde_json::from_str(
            r#"{"op":"add","items":[{"type":"hit","token":"ab"},{"type":"jellyfin","id":"x","mix":true}],"after":7}"#,
        )
        .unwrap();
        assert!(
            matches!(m, ClientMsg::Add { ref items, next: false, after: Some(7), top: false } if items.len() == 2)
        );
        let m: ClientMsg = serde_json::from_str(r#"{"op":"repeat","mode":"queue"}"#).unwrap();
        assert!(matches!(
            m,
            ClientMsg::Repeat {
                mode: Repeat::Queue
            }
        ));
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"volume","percent":-1}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"op":"rm -rf"}"#).is_err());
    }

    #[test]
    fn origin_from_public_url() {
        assert_eq!(
            expected_origin("https://music.example.com"),
            "https://music.example.com"
        );
        assert_eq!(
            expected_origin("http://192.168.1.5:8080"),
            "http://192.168.1.5:8080"
        );
    }
}
