//! Parses chat commands and turns player replies into chat text.

use tokio::sync::oneshot;

use crate::core::{Enqueued, JoinError, PlayerMsg, Snapshot, describe};
use crate::media::fmt_duration;
use crate::media::ytdlp::Resolved;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Play(String),
    /// Add to the priority lane, ahead of the regular queue.
    PlayNext(String),
    Skip,
    Queue,
    Clear,
    /// Optional voice channel name, for when we can't see the caller's channel.
    Join(Option<String>),
    Leave,
    Help,
}

pub fn parse(content: &str, prefix: &str) -> Option<Command> {
    let rest = content.trim().strip_prefix(prefix)?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, arg.trim()),
        None => (rest, ""),
    };
    let arg = (!arg.is_empty()).then(|| arg.to_string());
    Some(match name.to_lowercase().as_str() {
        "play" | "p" => Command::Play(arg?),
        "next" | "n" => Command::PlayNext(arg?),
        "skip" | "s" => Command::Skip,
        "queue" | "q" => Command::Queue,
        "clear" => Command::Clear,
        "join" | "j" => Command::Join(arg),
        "leave" | "stop" | "disconnect" => Command::Leave,
        "help" => Command::Help,
        _ => return None,
    })
}

pub fn help(prefix: &str) -> String {
    [
        format!("`{prefix}play <link or search>`: add a YouTube video, playlist or search result"),
        format!("`{prefix}next <link or search>` (`{prefix}n`): add to the priority queue, played before the regular queue"),
        format!("`{prefix}skip`: play the next track"),
        format!("`{prefix}queue`: show what's playing and coming up"),
        format!("`{prefix}clear`: remove all upcoming tracks, priority ones too"),
        format!("`{prefix}join [channel]`: join your voice channel (or the one named)"),
        format!("`{prefix}leave`: stop, clear the queue and leave voice"),
    ]
    .join("\n")
}

/// Sends a message to the player and waits for its answer.
pub async fn ask<T>(
    player: &tokio::sync::mpsc::Sender<PlayerMsg>,
    make: impl FnOnce(oneshot::Sender<T>) -> PlayerMsg,
) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    player.send(make(tx)).await.ok()?;
    rx.await.ok()
}

pub fn join_error(e: &JoinError, prefix: &str) -> String {
    match e {
        JoinError::NotInVoice => format!(
            "Join a voice channel first. If you already are, rejoin it or use `{prefix}join <channel name>`."
        ),
        JoinError::OtherChannel(ch) => format!("I'm already playing in <#{ch}>."),
        JoinError::Failed(e) => format!("Couldn't join voice: {e}"),
    }
}

pub fn enqueued(resolved: &Resolved, outcome: &Enqueued, priority: bool) -> String {
    let lane = if priority { " to the priority queue" } else { "" };
    let mut text = match (&resolved.playlist_title, resolved.tracks.as_slice()) {
        (Some(title), _) => format!("Added {} tracks from **{title}**{lane}", outcome.count),
        (None, [track]) if outcome.position == 0 => format!("Now playing {}", describe(track)),
        (None, [track]) => format!("Added {}{lane} at position {}", describe(track), outcome.position),
        (None, _) => format!("Added {} tracks{lane}", outcome.count),
    };
    if resolved.playlist_title.is_some() {
        let mut notes = Vec::new();
        if resolved.skipped > 0 {
            notes.push(format!("{} unavailable", resolved.skipped));
        }
        if resolved.truncated > 0 {
            notes.push(format!("{} over the limit", resolved.truncated));
        }
        if !notes.is_empty() {
            text.push_str(&format!(" ({})", notes.join(", ")));
        }
        if outcome.position == 0 {
            text.push_str(&format!(". Now playing {}", describe(&resolved.tracks[0])));
        }
    }
    text
}

/// Shown before priority-lane tracks in queue listings.
pub const PRIORITY_MARK: &str = "⏫ ";

pub fn queue(view: &Snapshot) -> String {
    let mut lines = Vec::new();
    match &view.now {
        Some(now) => {
            let (track, elapsed) = (&now.track, now.started_at.elapsed().as_secs());
            let progress = match track.duration_secs {
                Some(d) => format!("{} / {}", fmt_duration(elapsed), fmt_duration(d)),
                None => fmt_duration(elapsed),
            };
            lines.push(format!("Now playing: **{}** [{progress}]", track.title));
        }
        None => lines.push("Nothing is playing.".into()),
    }
    if view.upcoming.is_empty() {
        lines.push("The queue is empty.".into());
    } else {
        for (i, track) in view.upcoming.iter().enumerate() {
            let mark = if i < view.upcoming_priority { PRIORITY_MARK } else { "" };
            lines.push(format!("`{}.` {mark}{} · {}", i + 1, describe(track), track.requested_by));
        }
        let more = view.upcoming_total - view.upcoming.len();
        let mut footer = format!("{} upcoming, {} total", view.upcoming_total, fmt_duration(view.upcoming_secs));
        if more > 0 {
            footer = format!("…and {more} more. {footer}");
        }
        lines.push(footer);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse("!play  never gonna ", "!"), Some(Command::Play("never gonna".into())));
        assert_eq!(parse("!P https://youtu.be/x", "!"), Some(Command::Play("https://youtu.be/x".into())));
        assert_eq!(parse("!skip", "!"), Some(Command::Skip));
        assert_eq!(parse("!n https://youtu.be/x", "!"), Some(Command::PlayNext("https://youtu.be/x".into())));
        assert_eq!(parse("!next some song", "!"), Some(Command::PlayNext("some song".into())));
        assert_eq!(parse("!q", "!"), Some(Command::Queue));
        assert_eq!(parse("!clear", "!"), Some(Command::Clear));
        assert_eq!(parse("!join", "!"), Some(Command::Join(None)));
        assert_eq!(parse("!join Music Room", "!"), Some(Command::Join(Some("Music Room".into()))));
        assert_eq!(parse("!stop", "!"), Some(Command::Leave));
    }

    #[test]
    fn ignores_other_text() {
        assert_eq!(parse("hello", "!"), None);
        assert_eq!(parse("!play", "!"), None);
        assert_eq!(parse("!next", "!"), None);
        assert_eq!(parse("!unknown", "!"), None);
        assert_eq!(parse("?play x", "!"), None);
    }
}
