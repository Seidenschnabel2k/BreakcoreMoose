//! The music channel: one panel embed showing the current track and queue,
//! and nothing else. Everyone else's messages are deleted, and the bot's own
//! replies delete themselves.

use std::sync::Arc;
use std::time::Duration;

use fluxer::http::Http;
use fluxer::prelude::{Embed, EmbedBuilder, GetMessagesQuery, MessageCreatePayload};
use tokio::sync::watch;

use crate::commands::PRIORITY_MARK;
use crate::core::Snapshot;
use crate::media::fmt_duration;

/// How long the bot's replies stay up in the music channel.
pub const REPLY_TTL: Duration = Duration::from_secs(10);
/// Wait this long after a change before redrawing, so a burst of changes
/// (a playlist add, several skips) becomes one edit.
const DEBOUNCE: Duration = Duration::from_millis(1500);
/// Startup cleanup stops after this many pages of 100 messages.
const PURGE_PAGES: usize = 10;
const UP_NEXT: usize = 10;
const TITLE_MAX: usize = 80;
/// Longest embed field value the API accepts.
const FIELD_MAX: usize = 1024;
const COLOR: u64 = 0xE0457B;

/// Posts `text` to the music channel and deletes it after [`REPLY_TTL`].
pub fn post_temporary(http: Arc<Http>, channel_id: String, text: String) {
    tokio::spawn(async move {
        match http.send_message(&channel_id, &text).await {
            Ok(msg) => {
                tokio::time::sleep(REPLY_TTL).await;
                delete(&http, &channel_id, &msg.id).await;
            }
            Err(e) => tracing::warn!(channel = %channel_id, "sending reply: {e}"),
        }
    });
}

pub async fn delete(http: &Http, channel_id: &str, message_id: &str) {
    if let Err(e) = http.delete_message(channel_id, message_id).await {
        tracing::warn!(channel = %channel_id, "deleting message (does the bot have Manage Messages?): {e}");
    }
}

/// Runs the panel for one channel until the player goes away.
pub fn spawn_panel(
    http: Arc<Http>,
    channel_id: String,
    prefix: String,
    rx: watch::Receiver<Snapshot>,
) {
    tokio::spawn(run_panel(http, channel_id, prefix, rx));
}

struct Panel {
    message_id: String,
    /// The track instance this panel was posted for; `None` for the idle panel.
    generation: Option<u64>,
    /// Serialized embed, to skip edits that change nothing.
    rendered: String,
}

async fn run_panel(
    http: Arc<Http>,
    channel_id: String,
    prefix: String,
    mut rx: watch::Receiver<Snapshot>,
) {
    purge(&http, &channel_id).await;
    let mut panel: Option<Panel> = None;

    loop {
        let snapshot = rx.borrow_and_update().clone();
        let generation = snapshot.now.as_ref().map(|n| n.generation);
        let embed = render(&snapshot, &prefix);
        let rendered = serde_json::to_string(&embed).unwrap_or_default();

        panel = match panel.take() {
            Some(p) if p.generation == generation && p.rendered == rendered => Some(p),
            Some(p) if p.generation == generation => {
                let payload = MessageCreatePayload {
                    embeds: Some(vec![embed.clone()]),
                    ..Default::default()
                };
                match http
                    .edit_message_advanced(&channel_id, &p.message_id, &payload)
                    .await
                {
                    Ok(_) => Some(Panel { rendered, ..p }),
                    // Someone deleted it; post a fresh one.
                    Err(_) => post(&http, &channel_id, embed, generation, rendered).await,
                }
            }
            // New track (or the queue ran out): the old panel goes, a new one is posted.
            old => {
                if let Some(p) = old {
                    delete(&http, &channel_id, &p.message_id).await;
                }
                post(&http, &channel_id, embed, generation, rendered).await
            }
        };

        if rx.changed().await.is_err() {
            break;
        }
        tokio::time::sleep(DEBOUNCE).await;
    }

    if let Some(p) = panel {
        delete(&http, &channel_id, &p.message_id).await;
    }
}

async fn post(
    http: &Http,
    channel_id: &str,
    embed: Embed,
    generation: Option<u64>,
    rendered: String,
) -> Option<Panel> {
    match http.send_embed(channel_id, None, vec![embed.clone()]).await {
        Ok(msg) => Some(Panel {
            message_id: msg.id,
            generation,
            rendered,
        }),
        Err(e) => {
            tracing::warn!(channel = %channel_id, "posting panel: {e}");
            tracing::debug!(channel = %channel_id, embed = %serde_json::to_string(&embed).unwrap_or_default(), "rejected panel");
            None
        }
    }
}

/// Deletes everything in the channel, so the panel starts out alone.
async fn purge(http: &Http, channel_id: &str) {
    let mut deleted = 0;
    for _ in 0..PURGE_PAGES {
        let query = GetMessagesQuery {
            limit: Some(100),
            ..Default::default()
        };
        let messages = match http.get_messages(channel_id, query).await {
            Ok(m) if !m.is_empty() => m,
            Ok(_) => break,
            Err(e) => {
                tracing::warn!(channel = %channel_id, "reading messages for cleanup: {e}");
                break;
            }
        };
        let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
        let bulk_ok = ids.len() >= 2
            && http
                .bulk_delete_messages(channel_id, ids.clone())
                .await
                .is_ok();
        if !bulk_ok {
            // Bulk delete can refuse old messages; fall back to one by one.
            for id in &ids {
                if http.delete_message(channel_id, id).await.is_err() {
                    tracing::warn!(channel = %channel_id, "cleanup stopped: can't delete messages (needs Manage Messages)");
                    return;
                }
            }
        }
        deleted += ids.len();
    }
    if deleted > 0 {
        tracing::info!(channel = %channel_id, deleted, "cleaned up music channel");
    }
}

fn truncate(title: &str) -> String {
    if title.chars().count() <= TITLE_MAX {
        return title.to_string();
    }
    let cut: String = title.chars().take(TITLE_MAX - 1).collect();
    format!("{cut}…")
}

/// Joins as many queue lines as fit in one embed field, plus an "…and N
/// more" line for the rest of `total`. Fluxer rejects the whole message
/// if a field value is longer than [`FIELD_MAX`] characters.
fn fit_lines(lines: Vec<String>, total: usize) -> String {
    // Room for "\n…and 99999 more".
    const MORE_RESERVE: usize = 20;
    let mut out = String::new();
    let mut len = 0;
    let mut shown = 0;
    for line in lines {
        let line_len = line.chars().count() + usize::from(shown > 0);
        if len + line_len > FIELD_MAX - MORE_RESERVE {
            break;
        }
        if shown > 0 {
            out.push('\n');
        }
        out.push_str(&line);
        len += line_len;
        shown += 1;
    }
    let more = total.saturating_sub(shown);
    if more > 0 {
        out.push_str(&format!("\n…and {more} more"));
    }
    out
}

pub fn render(s: &Snapshot, prefix: &str) -> Embed {
    let voice = match &s.voice_channel {
        Some(ch) => format!("<#{ch}>"),
        None => "not connected".into(),
    };

    let Some(now) = &s.now else {
        let mut description = String::from(
            "Nothing is playing.\n\nPost a link (YouTube, SoundCloud, Bandcamp, …), a playlist or a search here to start.",
        );
        if s.upcoming_total() > 0 {
            description.push_str(&format!(
                "\n{} tracks are waiting; use `{prefix}join` to continue.",
                s.upcoming_total()
            ));
        }
        return EmbedBuilder::new()
            .title("Idle")
            .description(description)
            .color(COLOR)
            .field("Voice", voice, true)
            .footer(format!("{prefix}help for commands"), None)
            .build();
    };

    let track = &now.track;
    let mut builder = EmbedBuilder::new()
        .author(
            if now.paused { "Paused" } else { "Now playing" },
            None,
            None,
        )
        .title(truncate(&track.title))
        .url(track.url.clone())
        .color(COLOR)
        .field(
            "Length",
            track
                .duration_secs
                .map(fmt_duration)
                .unwrap_or_else(|| "?".into()),
            true,
        )
        .field("Requested by", track.requested_by.clone(), true)
        .field("Voice", voice, true);
    if let Some(thumb) = &track.thumbnail {
        builder = builder.thumbnail(thumb.clone());
    }

    let up_next = if s.queue.is_empty() {
        "Nothing queued. Post a link or search to add one.".to_string()
    } else {
        let lines: Vec<String> = s
            .upcoming(UP_NEXT)
            .enumerate()
            .map(|(i, t)| {
                let mark = if i < s.upcoming_priority {
                    PRIORITY_MARK
                } else {
                    ""
                };
                match t.duration_secs {
                    Some(d) => format!(
                        "`{}.` {mark}[{}](<{}>) ({})",
                        i + 1,
                        truncate(&t.title),
                        t.url,
                        fmt_duration(d)
                    ),
                    None => format!("`{}.` {mark}{}", i + 1, truncate(&t.title)),
                }
            })
            .collect();
        fit_lines(lines, s.upcoming_total())
    };
    builder = builder.field(format!("Up next ({})", s.upcoming_total()), up_next, false);

    builder
        .footer(
            format!(
                "{} queued · {} total · {prefix}skip · {prefix}clear · {prefix}leave",
                s.upcoming_total(),
                fmt_duration(s.upcoming_secs)
            ),
            None,
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Entry, NowPlaying};
    use crate::media::Track;
    use tokio::time::Instant;

    fn track(title: &str) -> Track {
        Track {
            thumbnail: Some("https://i.ytimg.com/vi/x/hqdefault.jpg".into()),
            ..Track::test(title)
        }
    }

    #[test]
    fn idle_panel() {
        let embed = render(&Snapshot::default(), "!");
        assert_eq!(embed.title.as_deref(), Some("Idle"));
        assert!(embed.description.unwrap().contains("Post a link"));
    }

    #[test]
    fn playing_panel_lists_queue() {
        let snapshot = Snapshot {
            now: Some(NowPlaying {
                entry_id: 1,
                track: track("Song"),
                generation: 1,
                offset_secs: 0,
                started_at: Instant::now(),
                paused: false,
            }),
            queue: std::sync::Arc::new(
                (1..=12)
                    .map(|i| Entry {
                        id: i + 1,
                        track: track(&format!("Next {i}")),
                    })
                    .collect(),
            ),
            upcoming_priority: 1,
            upcoming_secs: 12 * 214,
            voice_channel: Some("123".into()),
            ..Default::default()
        };
        let embed = render(&snapshot, "!");
        assert_eq!(embed.title.as_deref(), Some("Song"));
        let fields = embed.fields.unwrap();
        let up_next = fields.iter().find(|f| f.name == "Up next (12)").unwrap();
        assert!(up_next.value.starts_with(
            "`1.` ⏫ [Next 1](<https://www.youtube.com/watch?v=x>) (3:34)\n`2.` [Next 2]"
        ));
        assert!(up_next.value.ends_with("…and 2 more"));
        assert!(
            fields
                .iter()
                .any(|f| f.name == "Voice" && f.value == "<#123>")
        );
    }

    #[test]
    fn up_next_fits_in_a_field() {
        let lines: Vec<String> = (1..=10)
            .map(|i| {
                format!(
                    "`{i}.` [{}](<http://jf.lan:8096/web/#/details?id={}>) (3:34)",
                    "a".repeat(80),
                    "f".repeat(32)
                )
            })
            .collect();
        let value = fit_lines(lines, 30);
        assert!(value.chars().count() <= FIELD_MAX);
        let shown = value.lines().count() - 1;
        assert!(value.ends_with(&format!("…and {} more", 30 - shown)));
    }

    #[test]
    fn long_titles_are_cut() {
        let long = "a".repeat(200);
        assert_eq!(truncate(&long).chars().count(), TITLE_MAX);
        assert_eq!(truncate("short"), "short");
    }
}
