use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use fluxer::http::Http;
use fluxer::prelude::Context;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use serde::{Deserialize, Serialize};

use super::queue::{Entry, Placement, Queue};
use super::spectrum::{Analyzer, BANDS, Levels};
use crate::config::Config;
use crate::media::ytdlp::AudioStream;
use crate::media::{Track, fmt_duration};
use crate::music_channel;
use crate::voice::{FRAME_SAMPLES, LiveKitTransport, VoiceTransport};

/// Leave voice after this long with nothing playing.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// No PCM for this long while playing counts as a failed track.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Our own leave/join produces voice state events; ignore them this soon after joining.
const JOIN_GRACE: Duration = Duration::from_secs(5);
/// Tracks kept for "previous".
const HISTORY: usize = 50;
/// "Previous" further into a track than this restarts it instead.
const RESTART_AFTER_SECS: u64 = 3;
pub const MAX_VOLUME: u16 = 200;
/// `!volume` and the web slider end at 100 %, which stands for the `VOLUME` setting.
pub const USER_MAX_VOLUME: u16 = 100;

/// User percent (0–100, where 100 is `base`) to player percent.
pub fn from_user_volume(percent: u16, base: u16) -> u16 {
    ((u32::from(percent.min(USER_MAX_VOLUME)) * u32::from(base) + 50) / 100) as u16
}

/// Player percent to user percent.
pub fn to_user_volume(percent: u16, base: u16) -> u16 {
    let base = u32::from(base.max(1));
    ((u32::from(percent) * 100 + base / 2) / base) as u16
}

pub enum PlayerMsg {
    /// Append to the end; start playing if idle. `voice_channel` is the
    /// caller's channel, `None` if we can't see it.
    Enqueue {
        ctx: Context,
        tracks: Vec<Track>,
        /// `!next` goes into the priority lane, played before the regular queue.
        placement: Placement,
        voice_channel: Option<String>,
        /// Where chat replies go; `None` from the web keeps the last one.
        text_channel: Option<String>,
        reply: oneshot::Sender<Result<Enqueued, JoinError>>,
    },
    Join {
        ctx: Context,
        voice_channel: Option<String>,
        text_channel: Option<String>,
        reply: oneshot::Sender<Result<String, JoinError>>,
    },
    /// Resumes when paused, starts the queue when stopped (joining the
    /// caller's channel if needed), and does nothing while playing.
    Play {
        ctx: Context,
        voice_channel: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Plays this queue entry now; the current track goes to the history.
    PlayEntry {
        ctx: Context,
        voice_channel: Option<String>,
        id: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Pause {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Stops the current track and puts it back at the front of the queue.
    /// Stays in voice. Replies whether something was playing.
    Stop {
        reply: oneshot::Sender<bool>,
    },
    /// Restarts the current track, or goes back to the previous one when
    /// it only just started.
    Previous {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Removes queue entries; replies how many were found.
    Remove {
        ids: Vec<u64>,
        reply: oneshot::Sender<usize>,
    },
    /// Moves an entry after another (`None`: to the front).
    Move {
        id: u64,
        after: Option<u64>,
        reply: oneshot::Sender<bool>,
    },
    /// Percent of the source, 0–[`MAX_VOLUME`]. Takes effect at once.
    SetVolume {
        percent: u16,
    },
    SetRepeat {
        mode: Repeat,
    },
    /// For the web player: the state and the visualizer levels.
    Subscribe {
        reply: oneshot::Sender<(watch::Receiver<Snapshot>, watch::Receiver<Levels>)>,
    },
    Skip {
        reply: oneshot::Sender<Option<Track>>,
    },
    /// Restarts the current track at another position. Replies with the
    /// track title and the new position in seconds.
    Seek {
        target: SeekTarget,
        reply: oneshot::Sender<Result<(String, u64), String>>,
    },
    /// Shuffles the regular queue; replies how many tracks were shuffled.
    Shuffle {
        reply: oneshot::Sender<usize>,
    },
    /// Empties both lanes of the queue; the current track keeps playing.
    Clear {
        reply: oneshot::Sender<usize>,
    },
    Queue {
        reply: oneshot::Sender<Snapshot>,
    },
    /// Stops, clears the queue and leaves voice. Replies whether we were connected.
    Leave {
        reply: oneshot::Sender<bool>,
    },
    /// Makes `channel_id` this server's music channel and starts its panel.
    SetMusicChannel {
        channel_id: String,
    },
    /// The bot's own voice state changed on the gateway.
    BotVoiceState {
        channel_id: Option<String>,
    },
    /// A playback run finished. `run` changes on every seek, so a run
    /// replaced by a seek can't end the track.
    TrackEnded {
        run: u64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekTarget {
    To(u64),
    Forward(u64),
    Back(u64),
}

impl SeekTarget {
    fn resolve(self, position: u64) -> u64 {
        match self {
            SeekTarget::To(s) => s,
            SeekTarget::Forward(s) => position.saturating_add(s),
            SeekTarget::Back(s) => position.saturating_sub(s),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    /// Play the current track again and again.
    Track,
    /// Finished tracks go back to the end of the queue.
    Queue,
}

#[derive(Debug)]
pub struct Enqueued {
    /// 1-based position of the first added track in the upcoming queue;
    /// 0 when it started playing right away.
    pub position: usize,
    pub count: usize,
}

#[derive(Debug)]
pub enum JoinError {
    NotInVoice,
    /// Busy playing in another channel of this server.
    OtherChannel(String),
    Failed(String),
}

/// The player's state as shown to people, published after every change.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub now: Option<NowPlaying>,
    /// Every upcoming entry in play order. Shared, so publishing is cheap.
    pub queue: Arc<Vec<Entry>>,
    /// Changes whenever `queue` does.
    pub queue_version: u64,
    /// How many of the upcoming tracks, from the front, are priority tracks.
    pub upcoming_priority: usize,
    pub upcoming_secs: u64,
    pub voice_channel: Option<String>,
    pub volume: u16,
    pub repeat: Repeat,
}

impl Snapshot {
    pub fn upcoming(&self, n: usize) -> impl Iterator<Item = &Track> {
        self.queue.iter().take(n).map(|e| &e.track)
    }

    pub fn upcoming_total(&self) -> usize {
        self.queue.len()
    }
}

#[derive(Clone)]
pub struct NowPlaying {
    pub entry_id: u64,
    pub track: Track,
    /// Unique per play, so replaying the same song counts as a new track.
    pub generation: u64,
    /// Where in the track the current run started (non-zero after a seek),
    /// or where it was paused.
    pub offset_secs: u64,
    pub started_at: Instant,
    pub paused: bool,
}

impl NowPlaying {
    pub fn position_secs(&self) -> u64 {
        if self.paused {
            self.offset_secs
        } else {
            self.offset_secs + self.started_at.elapsed().as_secs()
        }
    }
}

struct Voice {
    channel_id: String,
    transport: Arc<dyn VoiceTransport>,
    joined_at: Instant,
}

struct Playing {
    entry: Entry,
    generation: u64,
    run: u64,
    offset_secs: u64,
    started_at: Instant,
    /// `None` while paused.
    task: Option<JoinHandle<()>>,
}

impl Playing {
    fn paused(&self) -> bool {
        self.task.is_none()
    }

    fn position_secs(&self) -> u64 {
        if self.paused() {
            self.offset_secs
        } else {
            self.offset_secs + self.started_at.elapsed().as_secs()
        }
    }
}

struct GuildPlayer {
    guild_id: String,
    config: Arc<Config>,
    http: Arc<Http>,
    tx: mpsc::Sender<PlayerMsg>,
    queue: Queue,
    current: Option<Playing>,
    voice: Option<Voice>,
    text_channel: Option<String>,
    /// When set, all the player's messages go here and delete themselves.
    music_channel: Option<String>,
    snapshot: watch::Sender<Snapshot>,
    levels: watch::Sender<Levels>,
    /// Shared with the playback task, so changes are heard at once.
    volume: Arc<AtomicU16>,
    repeat: Repeat,
    /// Finished and skipped tracks, newest last.
    history: VecDeque<Track>,
    /// The published queue, rebuilt only when the queue version changes.
    queue_view: Arc<Vec<Entry>>,
    queue_view_version: u64,
    generation: u64,
    runs: u64,
    idle_deadline: Option<Instant>,
}

pub fn spawn(guild_id: String, config: Arc<Config>, http: Arc<Http>) -> mpsc::Sender<PlayerMsg> {
    let (tx, rx) = mpsc::channel(64);
    let volume = Arc::new(AtomicU16::new(config.volume));
    let player = GuildPlayer {
        guild_id,
        config,
        http,
        tx: tx.clone(),
        queue: Queue::default(),
        current: None,
        voice: None,
        text_channel: None,
        music_channel: None,
        snapshot: watch::Sender::new(Snapshot::default()),
        levels: watch::Sender::new([0; BANDS]),
        volume,
        repeat: Repeat::Off,
        history: VecDeque::new(),
        queue_view: Arc::default(),
        queue_view_version: 0,
        generation: 0,
        runs: 0,
        idle_deadline: None,
    };
    tokio::spawn(player.run(rx));
    tx
}

impl GuildPlayer {
    async fn run(mut self, mut rx: mpsc::Receiver<PlayerMsg>) {
        loop {
            let idle = self.idle_deadline;
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.handle(msg).await,
                    None => break,
                },
                _ = tokio::time::sleep_until(idle.unwrap_or_else(Instant::now)), if idle.is_some() => {
                    self.leave().await;
                    self.say("Left voice after 60 minutes of silence.").await;
                }
            }
            let view = self.view();
            let _ = self.snapshot.send_replace(view);
        }
    }

    async fn handle(&mut self, msg: PlayerMsg) {
        match msg {
            PlayerMsg::Enqueue {
                ctx,
                tracks,
                placement,
                voice_channel,
                text_channel,
                reply,
            } => {
                self.text_channel = text_channel.or(self.text_channel.take());
                let result = self.enqueue(&ctx, tracks, placement, voice_channel).await;
                let _ = reply.send(result);
            }
            PlayerMsg::Join {
                ctx,
                voice_channel,
                text_channel,
                reply,
            } => {
                self.text_channel = text_channel.or(self.text_channel.take());
                let result = self.ensure_voice(&ctx, voice_channel.as_deref()).await;
                let _ =
                    reply.send(result.map(|()| self.voice.as_ref().unwrap().channel_id.clone()));
                if self.current.is_none() && self.voice.is_some() {
                    self.start_next(true).await;
                }
            }
            PlayerMsg::Skip { reply } => {
                let skipped = self.stop_current().map(|e| e.track);
                if let Some(track) = &skipped {
                    self.remember(track.clone());
                    self.start_next(true).await;
                }
                let _ = reply.send(skipped);
            }
            PlayerMsg::Play {
                ctx,
                voice_channel,
                reply,
            } => {
                let _ = reply.send(self.play(&ctx, voice_channel).await);
            }
            PlayerMsg::PlayEntry {
                ctx,
                voice_channel,
                id,
                reply,
            } => {
                let _ = reply.send(self.play_entry(&ctx, voice_channel, id).await);
            }
            PlayerMsg::Pause { reply } => {
                let _ = reply.send(self.pause());
            }
            PlayerMsg::Stop { reply } => {
                let stopped = self.stop_current();
                let was_playing = stopped.is_some();
                if let Some(entry) = stopped {
                    self.queue.push_front(entry);
                    self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
                }
                let _ = reply.send(was_playing);
            }
            PlayerMsg::Previous { reply } => {
                let _ = reply.send(self.previous().await);
            }
            PlayerMsg::Remove { ids, reply } => {
                let _ = reply.send(self.queue.remove(&ids));
            }
            PlayerMsg::Move { id, after, reply } => {
                let _ = reply.send(self.queue.move_after(id, after));
            }
            PlayerMsg::SetVolume { percent } => {
                self.volume
                    .store(percent.min(MAX_VOLUME), Ordering::Relaxed);
            }
            PlayerMsg::SetRepeat { mode } => self.repeat = mode,
            PlayerMsg::Subscribe { reply } => {
                let _ = reply.send((self.snapshot.subscribe(), self.levels.subscribe()));
            }
            PlayerMsg::Seek { target, reply } => {
                let _ = reply.send(self.seek(target));
            }
            PlayerMsg::Clear { reply } => {
                let _ = reply.send(self.queue.clear());
            }
            PlayerMsg::Shuffle { reply } => {
                let _ = reply.send(self.queue.shuffle());
            }
            PlayerMsg::Queue { reply } => {
                let _ = reply.send(self.view());
            }
            PlayerMsg::SetMusicChannel { channel_id } => {
                if self.music_channel.as_ref() == Some(&channel_id) {
                    return;
                }
                tracing::info!(guild = %self.guild_id, channel = %channel_id, "music channel set");
                music_channel::spawn_panel(
                    self.http.clone(),
                    channel_id.clone(),
                    self.config.prefix.clone(),
                    self.snapshot.subscribe(),
                );
                self.music_channel = Some(channel_id);
            }
            PlayerMsg::Leave { reply } => {
                self.queue.clear();
                let was_connected = self.voice.is_some();
                self.leave().await;
                let _ = reply.send(was_connected);
            }
            PlayerMsg::BotVoiceState { channel_id } => {
                let Some(voice) = &self.voice else { return };
                if voice.joined_at.elapsed() < JOIN_GRACE
                    || channel_id.as_ref() == Some(&voice.channel_id)
                {
                    return;
                }
                tracing::warn!(guild = %self.guild_id, ?channel_id, "bot was moved or disconnected from voice");
                if let Some(entry) = self.stop_current() {
                    // Put it back so `!play` / `!join` resumes from this track.
                    self.queue.push_front(entry);
                }
                self.leave().await;
                self.say(
                    "I was disconnected from voice. The queue is kept; use `!join` to continue.",
                )
                .await;
            }
            PlayerMsg::TrackEnded { run, error } => {
                let Some(playing) = &self.current else { return };
                if playing.run != run {
                    return;
                }
                let entry = self.current.take().unwrap().entry;
                if let Some(error) = error {
                    tracing::warn!(guild = %self.guild_id, url = %entry.track.url, %error, "track failed");
                    self.say(&format!("Couldn't play **{}**: {error}", entry.track.title))
                        .await;
                    self.start_next(true).await;
                    return;
                }
                self.remember(entry.track.clone());
                match self.repeat {
                    Repeat::Track => {
                        self.start_entry(entry, false).await;
                        return;
                    }
                    Repeat::Queue => self.queue.push_back(entry),
                    Repeat::Off => {}
                }
                self.start_next(true).await;
            }
        }
    }

    async fn enqueue(
        &mut self,
        ctx: &Context,
        tracks: Vec<Track>,
        placement: Placement,
        voice_channel: Option<String>,
    ) -> Result<Enqueued, JoinError> {
        self.ensure_voice(ctx, voice_channel.as_deref()).await?;
        let count = tracks.len();
        let position = self.queue.push(tracks, placement);
        if self.current.is_none() {
            // Announce only when older entries play first; otherwise the
            // command's own reply already says what started.
            self.start_next(position > 1).await;
            if position == 1 {
                return Ok(Enqueued { position: 0, count });
            }
            return Ok(Enqueued {
                position: position - 1,
                count,
            });
        }
        Ok(Enqueued { position, count })
    }

    /// Makes sure we're in the caller's voice channel, joining or moving if idle.
    async fn ensure_voice(&mut self, ctx: &Context, wanted: Option<&str>) -> Result<(), JoinError> {
        match (&self.voice, wanted) {
            (Some(voice), Some(wanted)) if voice.channel_id != wanted => {
                if self.current.is_some() {
                    return Err(JoinError::OtherChannel(voice.channel_id.clone()));
                }
                self.leave().await;
            }
            (Some(_), _) => return Ok(()),
            (None, None) => return Err(JoinError::NotInVoice),
            (None, Some(_)) => {}
        }
        let channel_id = wanted.expect("checked above").to_string();

        tracing::info!(guild = %self.guild_id, channel = %channel_id, "joining voice");
        let transport = LiveKitTransport::join(ctx, &self.guild_id, &channel_id)
            .await
            .map_err(|e| JoinError::Failed(e.to_string()))?;
        self.voice = Some(Voice {
            channel_id,
            transport: Arc::new(transport),
            joined_at: Instant::now(),
        });
        self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        Ok(())
    }

    async fn start_next(&mut self, announce: bool) {
        if self.voice.is_none() {
            return;
        }
        let Some(entry) = self.queue.pop() else {
            self.current = None;
            self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
            return;
        };
        self.start_entry(entry, announce).await;
    }

    async fn start_entry(&mut self, entry: Entry, announce: bool) {
        let Some(voice) = &self.voice else { return };
        let transport = voice.transport.clone();
        self.generation += 1;
        let generation = self.generation;
        let (run, task) = self.spawn_run(transport, &entry.track, 0);
        // The music channel's panel already shows what's playing.
        if announce && self.music_channel.is_none() {
            self.say(&format!("Now playing {}", describe(&entry.track)))
                .await;
        }
        self.current = Some(Playing {
            entry,
            generation,
            run,
            offset_secs: 0,
            started_at: Instant::now(),
            task: Some(task),
        });
        self.idle_deadline = None;
    }

    fn spawn_run(
        &mut self,
        transport: Arc<dyn VoiceTransport>,
        track: &Track,
        offset_secs: u64,
    ) -> (u64, JoinHandle<()>) {
        self.runs += 1;
        let run = self.runs;
        let task = tokio::spawn(play(
            self.config.clone(),
            transport,
            track.clone(),
            offset_secs,
            run,
            self.tx.clone(),
            self.volume.clone(),
            self.levels.clone(),
        ));
        (run, task)
    }

    /// Restarts the current track's pipeline at the new position. Same track,
    /// same `generation`, so the panel edits rather than reposts. While
    /// paused only the position changes.
    fn seek(&mut self, target: SeekTarget) -> Result<(String, u64), String> {
        let Some(voice) = &self.voice else {
            return Err("Nothing is playing.".into());
        };
        let transport = voice.transport.clone();
        let Some(playing) = &self.current else {
            return Err("Nothing is playing.".into());
        };

        let position = target.resolve(playing.position_secs());
        if let Some(duration) = playing.entry.track.duration_secs
            && position >= duration
        {
            return Err(format!(
                "That's past the end of the track ({}).",
                fmt_duration(duration)
            ));
        }
        if playing.paused() {
            let playing = self.current.as_mut().expect("checked above");
            playing.offset_secs = position;
            return Ok((playing.entry.track.title.clone(), position));
        }

        let track = playing.entry.track.clone();
        let (run, task) = self.spawn_run(transport.clone(), &track, position);
        let playing = self.current.as_mut().expect("checked above");
        // Abort first, then clear, so no frame from the old run sneaks in.
        if let Some(old) = playing.task.replace(task) {
            old.abort();
        }
        transport.clear_buffer();
        playing.run = run;
        playing.offset_secs = position;
        playing.started_at = Instant::now();
        Ok((track.title, position))
    }

    /// Ends the pipeline but keeps the track and its position.
    fn pause(&mut self) -> Result<(), String> {
        let Some(playing) = &mut self.current else {
            return Err("Nothing is playing.".into());
        };
        let position = playing.position_secs();
        let Some(task) = playing.task.take() else {
            return Ok(());
        };
        task.abort();
        playing.offset_secs = position;
        // A TrackEnded already on its way must not end the paused track.
        playing.run = 0;
        if let Some(voice) = &self.voice {
            voice.transport.clear_buffer();
        }
        self.levels.send_replace([0; BANDS]);
        self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        Ok(())
    }

    async fn play(&mut self, ctx: &Context, voice_channel: Option<String>) -> Result<(), String> {
        if let Some(playing) = &self.current {
            if !playing.paused() {
                return Ok(());
            }
            let Some(voice) = &self.voice else {
                return Err("I'm not in a voice channel.".into());
            };
            let transport = voice.transport.clone();
            let (track, offset) = (playing.entry.track.clone(), playing.offset_secs);
            let (run, task) = self.spawn_run(transport, &track, offset);
            let playing = self.current.as_mut().expect("checked above");
            playing.task = Some(task);
            playing.run = run;
            playing.started_at = Instant::now();
            self.idle_deadline = None;
            return Ok(());
        }
        if self.queue.len() == 0 {
            return Err("The playlist is empty.".into());
        }
        self.ensure_voice(ctx, voice_channel.as_deref())
            .await
            .map_err(|e| crate::commands::join_error(&e, &self.config.prefix))?;
        self.start_next(false).await;
        Ok(())
    }

    async fn play_entry(
        &mut self,
        ctx: &Context,
        voice_channel: Option<String>,
        id: u64,
    ) -> Result<(), String> {
        if self.queue.iter().all(|e| e.id != id) {
            return Err("That track is no longer in the playlist.".into());
        }
        if self.voice.is_none() || self.current.is_none() {
            self.ensure_voice(ctx, voice_channel.as_deref())
                .await
                .map_err(|e| crate::commands::join_error(&e, &self.config.prefix))?;
        }
        let entry = self
            .queue
            .take(id)
            .ok_or("That track is no longer in the playlist.")?;
        if let Some(previous) = self.stop_current() {
            self.remember(previous.track);
        }
        self.start_entry(entry, false).await;
        Ok(())
    }

    async fn previous(&mut self) -> Result<(), String> {
        let Some(playing) = &self.current else {
            return Err("Nothing is playing.".into());
        };
        if playing.position_secs() > RESTART_AFTER_SECS || self.history.is_empty() {
            return self.seek(SeekTarget::To(0)).map(|_| ());
        }
        let track = self.history.pop_back().expect("checked above");
        let previous = self.queue.entry(track);
        let current = self.stop_current().expect("checked above");
        self.queue.push_front(current);
        self.start_entry(previous, false).await;
        Ok(())
    }

    fn remember(&mut self, track: Track) {
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(track);
    }

    /// Stops the current track at once: aborting the task drops yt-dlp and ffmpeg.
    fn stop_current(&mut self) -> Option<Entry> {
        let playing = self.current.take()?;
        if let Some(task) = playing.task {
            task.abort();
        }
        if let Some(voice) = &self.voice {
            voice.transport.clear_buffer();
        }
        self.levels.send_replace([0; BANDS]);
        Some(playing.entry)
    }

    async fn leave(&mut self) {
        self.stop_current();
        self.idle_deadline = None;
        if let Some(voice) = self.voice.take()
            && let Err(e) = voice.transport.leave().await
        {
            tracing::warn!(guild = %self.guild_id, "leaving voice: {e}");
        }
    }

    fn view(&mut self) -> Snapshot {
        if self.queue_view_version != self.queue.version() {
            self.queue_view = Arc::new(self.queue.iter().cloned().collect());
            self.queue_view_version = self.queue.version();
        }
        Snapshot {
            now: self.current.as_ref().map(|p| NowPlaying {
                entry_id: p.entry.id,
                track: p.entry.track.clone(),
                generation: p.generation,
                offset_secs: p.offset_secs,
                started_at: p.started_at,
                paused: p.paused(),
            }),
            queue: self.queue_view.clone(),
            queue_version: self.queue_view_version,
            upcoming_priority: self.queue.priority_len(),
            upcoming_secs: self
                .queue_view
                .iter()
                .filter_map(|e| e.track.duration_secs)
                .sum(),
            voice_channel: self.voice.as_ref().map(|v| v.channel_id.clone()),
            volume: self.volume.load(Ordering::Relaxed),
            repeat: self.repeat,
        }
    }

    async fn say(&self, text: &str) {
        if let Some(channel) = &self.music_channel {
            music_channel::post_temporary(self.http.clone(), channel.clone(), text.to_string());
            return;
        }
        let Some(channel) = &self.text_channel else {
            return;
        };
        if let Err(e) = self.http.send_message(channel, text).await {
            tracing::warn!(guild = %self.guild_id, "sending message: {e}");
        }
    }
}

/// `**title** (3:45)`
pub fn describe(track: &Track) -> String {
    match track.duration_secs {
        Some(d) => format!("**{}** ({})", track.title, fmt_duration(d)),
        None => format!("**{}**", track.title),
    }
}

/// Streams one track into voice and reports back when it ends.
#[allow(clippy::too_many_arguments)]
async fn play(
    config: Arc<Config>,
    transport: Arc<dyn VoiceTransport>,
    track: Track,
    offset_secs: u64,
    run: u64,
    tx: mpsc::Sender<PlayerMsg>,
    volume: Arc<AtomicU16>,
    levels: watch::Sender<Levels>,
) {
    let error = stream(
        &config,
        transport.as_ref(),
        &track,
        offset_secs,
        &volume,
        &levels,
    )
    .await
    .err();
    levels.send_replace([0; BANDS]);
    let _ = tx.send(PlayerMsg::TrackEnded { run, error }).await;
}

async fn stream(
    config: &Config,
    transport: &dyn VoiceTransport,
    track: &Track,
    offset_secs: u64,
    volume: &AtomicU16,
    levels: &watch::Sender<Levels>,
) -> Result<(), String> {
    let mut audio = AudioStream::open(config, track, offset_secs)
        .await
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0u8; FRAME_SAMPLES * 2];
    let mut samples = vec![0i16; FRAME_SAMPLES];
    let mut frames = 0u64;
    let mut analyzer = Analyzer::default();

    loop {
        match tokio::time::timeout(STALL_TIMEOUT, audio.pcm.read_exact(&mut bytes)).await {
            Err(_) => return Err("no audio for 10 seconds".into()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Ok(Err(e)) => return Err(format!("reading audio: {e}")),
            Ok(Ok(_)) => {}
        }
        for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *s = i16::from_le_bytes(*b);
        }
        // Before the volume, so the visualizer doesn't shrink when it's turned down.
        if levels.receiver_count() > 0
            && let Some(l) = analyzer.push(&samples)
        {
            levels.send_replace(l);
        }
        apply_volume(&mut samples, volume.load(Ordering::Relaxed));
        transport
            .push_frame(&samples)
            .await
            .map_err(|e| format!("voice: {e}"))?;
        frames += 1;
    }

    match audio.error().await {
        Some(error) => Err(error),
        None if frames == 0 => Err("no audio came back".into()),
        None => Ok(()),
    }
}

/// Scales samples by `percent`, clipping like ffmpeg's volume filter does.
fn apply_volume(samples: &mut [i16], percent: u16) {
    if percent == 100 {
        return;
    }
    let gain = i32::from(percent);
    for s in samples {
        *s = (i32::from(*s) * gain / 100).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_volume, from_user_volume, to_user_volume};

    #[test]
    fn user_volume_is_relative_to_setting() {
        assert_eq!(from_user_volume(100, 50), 50);
        assert_eq!(from_user_volume(50, 50), 25);
        assert_eq!(from_user_volume(150, 50), 50);
        assert_eq!(to_user_volume(50, 50), 100);
        assert_eq!(to_user_volume(25, 50), 50);
        // The player keeps whole percents, so below 100 a user step can land one off.
        for v in 0..=100 {
            assert!(to_user_volume(from_user_volume(v, 70), 70).abs_diff(v) <= 1);
        }
    }

    #[test]
    fn volume_scales_and_clips() {
        let mut s = [1000, -1000, 30000, i16::MIN];
        apply_volume(&mut s, 50);
        assert_eq!(s, [500, -500, 15000, -16384]);
        let mut s = [20000, -20000];
        apply_volume(&mut s, 200);
        assert_eq!(s, [i16::MAX, i16::MIN]);
    }
}
