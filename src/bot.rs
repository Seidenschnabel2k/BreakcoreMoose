//! Fluxer event handler: turns chat messages into player messages and keeps
//! track of who is in which voice channel.

use std::collections::HashMap;
use std::sync::Arc;

use fluxer::prelude::*;
use tokio::sync::RwLock;

use crate::commands::{self, Command, ask};
use crate::config::Config;
use crate::core::{PlayerMsg, Players};
use crate::media::source;
use crate::media::{fmt_duration, ytdlp};
use crate::music_channel;

/// Fluxer's voice channel type.
const VOICE_CHANNEL: u64 = 2;

pub struct Handler {
    config: Arc<Config>,
    players: Players,
    /// guild id → user id → voice channel id, from VOICE_STATE_UPDATE.
    /// fluxer-rust drops GUILD_CREATE voice states, so people who were in
    /// voice before the bot started are unknown until they move.
    voice: RwLock<HashMap<String, HashMap<String, String>>>,
}

impl Handler {
    pub fn new(config: Arc<Config>, players: Players) -> Self {
        Self { config, players, voice: Default::default() }
    }

    async fn user_voice(&self, guild_id: &str, user_id: &str) -> Option<String> {
        self.voice.read().await.get(guild_id)?.get(user_id).cloned()
    }

    async fn run(&self, ctx: &Context, msg: &Message, guild_id: &str, channel_id: &str, cmd: Command) -> String {
        let prefix = &self.config.prefix;
        match cmd {
            Command::Play(query) => self.play(ctx, msg, guild_id, channel_id, &query, false).await,
            Command::PlayNext(query) => self.play(ctx, msg, guild_id, channel_id, &query, true).await,
            Command::Join(name) => {
                let voice_channel = match name {
                    Some(name) => match find_voice_channel(ctx, guild_id, &name).await {
                        Some(id) => Some(id),
                        None => return format!("No voice channel called **{name}**."),
                    },
                    None => self.user_voice(guild_id, &msg.author.id).await,
                };
                let player = self.players.get(guild_id).await;
                let reply = ask(&player, |reply| PlayerMsg::Join {
                    ctx: ctx.clone(),
                    voice_channel,
                    text_channel: channel_id.to_string(),
                    reply,
                })
                .await;
                match reply {
                    Some(Ok(ch)) => format!("Joined <#{ch}>."),
                    Some(Err(e)) => commands::join_error(&e, prefix),
                    None => "The player stopped unexpectedly.".into(),
                }
            }
            Command::Skip => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "Nothing is playing.".into();
                };
                match ask(&player, |reply| PlayerMsg::Skip { reply }).await.flatten() {
                    Some(track) => format!("Skipped **{}**.", track.title),
                    None => "Nothing is playing.".into(),
                }
            }
            Command::Shuffle => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "The queue is empty.".into();
                };
                match ask(&player, |reply| PlayerMsg::Shuffle { reply }).await.unwrap_or(0) {
                    0 | 1 => "Not enough tracks in the queue to shuffle.".into(),
                    n => format!("Shuffled {n} tracks."),
                }
            }
            Command::Seek(None) => {
                format!("Usage: `{prefix}seek 1:30` to jump to a time, or `{prefix}seek +30` / `{prefix}seek -15` to move.")
            }
            Command::Seek(Some(target)) => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "Nothing is playing.".into();
                };
                match ask(&player, |reply| PlayerMsg::Seek { target, reply }).await {
                    Some(Ok((title, position))) => format!("Jumped to {} in **{title}**.", fmt_duration(position)),
                    Some(Err(e)) => e,
                    None => "The player stopped unexpectedly.".into(),
                }
            }
            Command::Clear => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "The queue is already empty.".into();
                };
                match ask(&player, |reply| PlayerMsg::Clear { reply }).await.unwrap_or(0) {
                    0 => "The queue is already empty.".into(),
                    1 => "Removed 1 track from the queue.".into(),
                    n => format!("Removed {n} tracks from the queue."),
                }
            }
            Command::Queue => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "Nothing is playing and the queue is empty.".into();
                };
                match ask(&player, |reply| PlayerMsg::Queue { reply }).await {
                    Some(view) => commands::queue(&view),
                    None => "The player stopped unexpectedly.".into(),
                }
            }
            Command::Leave => {
                let Some(player) = self.players.existing(guild_id).await else {
                    return "I'm not in a voice channel.".into();
                };
                match ask(&player, |reply| PlayerMsg::Leave { reply }).await {
                    Some(true) => "Stopped and left voice.".into(),
                    _ => "I'm not in a voice channel.".into(),
                }
            }
            Command::Help => commands::help(prefix),
        }
    }

    async fn play(
        &self,
        ctx: &Context,
        msg: &Message,
        guild_id: &str,
        channel_id: &str,
        query: &str,
        priority: bool,
    ) -> String {
        let (query, playlist_skip) = commands::split_playlist_skip(query);
        let source = source::classify(query);
        let _ = ctx.http.trigger_typing(channel_id).await;

        let requested_by = msg.member.as_ref().map(|m| m.display_name()).filter(|n| !n.is_empty());
        let requested_by = requested_by.unwrap_or(&msg.author.username);
        let resolved = match ytdlp::resolve(&self.config, source, playlist_skip, requested_by).await {
            Ok(r) => r,
            Err(e) => return format!("Couldn't load that: {e}"),
        };
        if resolved.tracks.is_empty() {
            return match resolved.playlist_title {
                Some(title) if playlist_skip > 0 => {
                    format!("**{title}** has nothing playable after skipping the first {playlist_skip} tracks.")
                }
                Some(title) => format!("Nothing playable in **{title}**."),
                None if resolved.skipped > 0 => "That video is live, private or too long.".into(),
                None => "No results.".into(),
            };
        }

        let voice_channel = self.user_voice(guild_id, &msg.author.id).await;
        let player = self.players.get(guild_id).await;
        let reply = ask(&player, |reply| PlayerMsg::Enqueue {
            ctx: ctx.clone(),
            tracks: resolved.tracks.clone(),
            priority,
            voice_channel,
            text_channel: channel_id.to_string(),
            reply,
        })
        .await;
        match reply {
            Some(Ok(outcome)) => commands::enqueued(&resolved, &outcome, priority, playlist_skip),
            Some(Err(e)) => commands::join_error(&e, &self.config.prefix),
            None => "The player stopped unexpectedly.".into(),
        }
    }
}

async fn find_voice_channel(ctx: &Context, guild_id: &str, name: &str) -> Option<String> {
    let name = name.trim_start_matches('#');
    ctx.cache
        .channels
        .read()
        .await
        .values()
        .find(|c| {
            c.guild_id.as_deref() == Some(guild_id)
                && c.kind == Some(VOICE_CHANNEL)
                && (c.id == name || c.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        })
        .map(|c| c.id.clone())
}

#[async_trait]
impl EventHandler for Handler {
    async fn on_ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.username, "connected to Fluxer");
        // Runs again after a reconnect; the player ignores a repeat.
        for channel_id in &self.config.music_channels {
            let guild_id = match ctx.http.get_channel(channel_id).await {
                Ok(channel) => channel.guild_id,
                Err(e) => {
                    tracing::error!(channel = %channel_id, "MUSIC_CHANNEL_ID: can't read channel: {e}");
                    continue;
                }
            };
            let Some(guild_id) = guild_id else {
                tracing::error!(channel = %channel_id, "MUSIC_CHANNEL_ID: not a server channel");
                continue;
            };
            let player = self.players.get(&guild_id).await;
            let _ = player.send(PlayerMsg::SetMusicChannel { channel_id: channel_id.clone() }).await;
        }
    }

    async fn on_message(&self, ctx: Context, msg: Message) {
        let Some(channel_id) = msg.channel_id.as_deref() else { return };
        if ctx.cache.current_user().await.is_some_and(|u| u.id == msg.author.id) {
            return;
        }
        let in_music_channel = self.config.is_music_channel(channel_id);
        if in_music_channel {
            // Only the panel and the bot's own short-lived replies stay.
            music_channel::delete(&ctx.http, channel_id, &msg.id).await;
        }
        if msg.author.bot == Some(true) {
            return;
        }
        let (Some(guild_id), Some(content)) = (msg.guild_id.as_deref(), msg.content.as_deref()) else {
            return;
        };

        let prefix = &self.config.prefix;
        let cmd = match commands::parse(content, prefix) {
            Some(cmd) => cmd,
            None if !in_music_channel => return,
            None if content.trim().is_empty() => return,
            // A bare message in the music channel is a play request.
            None if !content.trim_start().starts_with(prefix.as_str()) => Command::Play(content.trim().to_string()),
            None => Command::Help,
        };

        tracing::info!(guild = guild_id, user = %msg.author.username, ?cmd, "command");
        let reply = self.run(&ctx, &msg, guild_id, channel_id, cmd).await;
        if in_music_channel {
            music_channel::post_temporary(ctx.http.clone(), channel_id.to_string(), reply);
        } else if let Err(e) = ctx.http.send_message(channel_id, &reply).await {
            tracing::warn!(guild = guild_id, "sending reply: {e}");
        }
    }

    async fn on_voice_state_update(&self, ctx: Context, event: VoiceStateUpdate) {
        let Some(guild_id) = event.guild_id else { return };
        {
            let mut voice = self.voice.write().await;
            let guild = voice.entry(guild_id.clone()).or_default();
            match &event.channel_id {
                Some(ch) => guild.insert(event.user_id.clone(), ch.clone()),
                None => guild.remove(&event.user_id),
            };
        }

        let is_bot = ctx.cache.current_user().await.is_some_and(|u| u.id == event.user_id);
        if is_bot
            && let Some(player) = self.players.existing(&guild_id).await {
                let _ = player.send(PlayerMsg::BotVoiceState { channel_id: event.channel_id }).await;
            }
    }
}
