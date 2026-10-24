use std::sync::Arc;
use std::time::Duration;

use fluxer::http::Http;
use fluxer::prelude::Context;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::queue::Queue;
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
const QUEUE_PAGE: usize = 10;

pub enum PlayerMsg {
    /// Append to the end; start playing if idle. `voice_channel` is the
    /// caller's channel, `None` if we can't see it.
    Enqueue {
        ctx: Context,
        tracks: Vec<Track>,
        /// `!next`: goes into the priority lane, played before the regular queue.
        priority: bool,
        voice_channel: Option<String>,
        text_channel: String,
        reply: oneshot::Sender<Result<Enqueued, JoinError>>,
    },
    Join {
        ctx: Context,
        voice_channel: Option<String>,
        text_channel: String,
        reply: oneshot::Sender<Result<String, JoinError>>,
    },
    Skip { reply: oneshot::Sender<Option<Track>> },
    /// Restarts the current track at another position. Replies with the
    /// track title and the new position in seconds.
    Seek { target: SeekTarget, reply: oneshot::Sender<Result<(String, u64), String>> },
    /// Shuffles the regular queue; replies how many tracks were shuffled.
    Shuffle { reply: oneshot::Sender<usize> },
    /// Empties both lanes of the queue; the current track keeps playing.
    Clear { reply: oneshot::Sender<usize> },
    Queue { reply: oneshot::Sender<Snapshot> },
    /// Stops, clears the queue and leaves voice. Replies whether we were connected.
    Leave { reply: oneshot::Sender<bool> },
    /// Makes `channel_id` this server's music channel and starts its panel.
    SetMusicChannel { channel_id: String },
    /// The bot's own voice state changed on the gateway.
    BotVoiceState { channel_id: Option<String> },
    /// A playback run finished. `run` changes on every seek, so a run
    /// replaced by a seek can't end the track.
    TrackEnded { run: u64, error: Option<String> },
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
    /// The first few upcoming tracks.
    pub upcoming: Vec<Track>,
    pub upcoming_total: usize,
    /// How many of the upcoming tracks, from the front, are priority tracks.
    pub upcoming_priority: usize,
    pub upcoming_secs: u64,
    pub voice_channel: Option<String>,
}

#[derive(Clone)]
pub struct NowPlaying {
    pub track: Track,
    /// Unique per play, so replaying the same song counts as a new track.
    pub generation: u64,
    /// Where in the track the current run started (non-zero after a seek).
    pub offset_secs: u64,
    pub started_at: Instant,
}

impl NowPlaying {
    pub fn position_secs(&self) -> u64 {
        self.offset_secs + self.started_at.elapsed().as_secs()
    }
}

struct Voice {
    channel_id: String,
    transport: Arc<dyn VoiceTransport>,
    joined_at: Instant,
}

struct Playing {
    track: Track,
    generation: u64,
    run: u64,
    offset_secs: u64,
    started_at: Instant,
    task: JoinHandle<()>,
}

impl Playing {
    fn position_secs(&self) -> u64 {
        self.offset_secs + self.started_at.elapsed().as_secs()
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
    generation: u64,
    runs: u64,
    idle_deadline: Option<Instant>,
}

pub fn spawn(guild_id: String, config: Arc<Config>, http: Arc<Http>) -> mpsc::Sender<PlayerMsg> {
    let (tx, rx) = mpsc::channel(64);
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
            let _ = self.snapshot.send_replace(self.view());
        }
    }

    async fn handle(&mut self, msg: PlayerMsg) {
        match msg {
            PlayerMsg::Enqueue { ctx, tracks, priority, voice_channel, text_channel, reply } => {
                self.text_channel = Some(text_channel);
                let result = self.enqueue(&ctx, tracks, priority, voice_channel).await;
                let _ = reply.send(result);
            }
            PlayerMsg::Join { ctx, voice_channel, text_channel, reply } => {
                self.text_channel = Some(text_channel);
                let result = self.ensure_voice(&ctx, voice_channel.as_deref()).await;
                let _ = reply.send(result.map(|()| self.voice.as_ref().unwrap().channel_id.clone()));
                if self.current.is_none() && self.voice.is_some() {
                    self.start_next(true).await;
                }
            }
            PlayerMsg::Skip { reply } => {
                let skipped = self.stop_current();
                if skipped.is_some() {
                    self.start_next(true).await;
                }
                let _ = reply.send(skipped);
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
                if voice.joined_at.elapsed() < JOIN_GRACE || channel_id.as_ref() == Some(&voice.channel_id) {
                    return;
                }
                tracing::warn!(guild = %self.guild_id, ?channel_id, "bot was moved or disconnected from voice");
                if let Some(playing) = self.stop_current() {
                    // Put it back so `!play` / `!join` resumes from this track.
                    self.queue.push_front(playing);
                }
                self.leave().await;
                self.say("I was disconnected from voice. The queue is kept; use `!join` to continue.").await;
            }
            PlayerMsg::TrackEnded { run, error } => {
                let Some(playing) = &self.current else { return };
                if playing.run != run {
                    return;
                }
                let track = self.current.take().unwrap().track;
                if let Some(error) = error {
                    tracing::warn!(guild = %self.guild_id, url = %track.url, %error, "track failed");
                    self.say(&format!("Couldn't play **{}**: {error}", track.title)).await;
                }
                self.start_next(true).await;
            }
        }
    }

    async fn enqueue(
        &mut self,
        ctx: &Context,
        tracks: Vec<Track>,
        priority: bool,
        voice_channel: Option<String>,
    ) -> Result<Enqueued, JoinError> {
        self.ensure_voice(ctx, voice_channel.as_deref()).await?;
        let count = tracks.len();
        let position = self.queue.push(tracks, priority);
        if self.current.is_none() {
            // Announce only when older entries play first; otherwise the
            // command's own reply already says what started.
            self.start_next(position > 1).await;
            if position == 1 {
                return Ok(Enqueued { position: 0, count });
            }
            return Ok(Enqueued { position: position - 1, count });
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
        self.voice = Some(Voice { channel_id, transport: Arc::new(transport), joined_at: Instant::now() });
        self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        Ok(())
    }

    async fn start_next(&mut self, announce: bool) {
        let Some(voice) = &self.voice else { return };
        let Some(track) = self.queue.pop() else {
            self.current = None;
            self.idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
            return;
        };

        let transport = voice.transport.clone();
        self.generation += 1;
        let generation = self.generation;
        let (run, task) = self.spawn_run(transport, &track.url, 0);
        // The music channel's panel already shows what's playing.
        if announce && self.music_channel.is_none() {
            self.say(&format!("Now playing {}", describe(&track))).await;
        }
        self.current = Some(Playing { track, generation, run, offset_secs: 0, started_at: Instant::now(), task });
        self.idle_deadline = None;
    }

    fn spawn_run(&mut self, transport: Arc<dyn VoiceTransport>, url: &str, offset_secs: u64) -> (u64, JoinHandle<()>) {
        self.runs += 1;
        let run = self.runs;
        let task = tokio::spawn(play(self.config.clone(), transport, url.to_string(), offset_secs, run, self.tx.clone()));
        (run, task)
    }

    /// Restarts the current track's pipeline at the new position. Same track,
    /// same `generation`, so the panel edits rather than reposts.
    fn seek(&mut self, target: SeekTarget) -> Result<(String, u64), String> {
        let Some(voice) = &self.voice else { return Err("Nothing is playing.".into()) };
        let transport = voice.transport.clone();
        let Some(playing) = &self.current else { return Err("Nothing is playing.".into()) };

        let position = target.resolve(playing.position_secs());
        if let Some(duration) = playing.track.duration_secs
            && position >= duration
        {
            return Err(format!("That's past the end of the track ({}).", fmt_duration(duration)));
        }

        let url = playing.track.url.clone();
        let (run, task) = self.spawn_run(transport.clone(), &url, position);
        let playing = self.current.as_mut().expect("checked above");
        // Abort first, then clear, so no frame from the old run sneaks in.
        playing.task.abort();
        transport.clear_buffer();
        playing.task = task;
        playing.run = run;
        playing.offset_secs = position;
        playing.started_at = Instant::now();
        Ok((playing.track.title.clone(), position))
    }

    /// Stops the current track at once: aborting the task drops yt-dlp and ffmpeg.
    fn stop_current(&mut self) -> Option<Track> {
        let playing = self.current.take()?;
        playing.task.abort();
        if let Some(voice) = &self.voice {
            voice.transport.clear_buffer();
        }
        Some(playing.track)
    }

    async fn leave(&mut self) {
        self.stop_current();
        self.idle_deadline = None;
        if let Some(voice) = self.voice.take()
            && let Err(e) = voice.transport.leave().await {
                tracing::warn!(guild = %self.guild_id, "leaving voice: {e}");
            }
    }

    fn view(&self) -> Snapshot {
        Snapshot {
            now: self.current.as_ref().map(|p| NowPlaying {
                track: p.track.clone(),
                generation: p.generation,
                offset_secs: p.offset_secs,
                started_at: p.started_at,
            }),
            upcoming: self.queue.iter().take(QUEUE_PAGE).cloned().collect(),
            upcoming_total: self.queue.len(),
            upcoming_priority: self.queue.priority_len(),
            upcoming_secs: self.queue.iter().filter_map(|t| t.duration_secs).sum(),
            voice_channel: self.voice.as_ref().map(|v| v.channel_id.clone()),
        }
    }

    async fn say(&self, text: &str) {
        if let Some(channel) = &self.music_channel {
            music_channel::post_temporary(self.http.clone(), channel.clone(), text.to_string());
            return;
        }
        let Some(channel) = &self.text_channel else { return };
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
async fn play(
    config: Arc<Config>,
    transport: Arc<dyn VoiceTransport>,
    url: String,
    offset_secs: u64,
    run: u64,
    tx: mpsc::Sender<PlayerMsg>,
) {
    let error = stream(&config, transport.as_ref(), &url, offset_secs).await.err();
    let _ = tx.send(PlayerMsg::TrackEnded { run, error }).await;
}

async fn stream(config: &Config, transport: &dyn VoiceTransport, url: &str, offset_secs: u64) -> Result<(), String> {
    let mut audio = AudioStream::open(config, url, offset_secs).map_err(|e| e.to_string())?;
    let mut bytes = vec![0u8; FRAME_SAMPLES * 2];
    let mut samples = vec![0i16; FRAME_SAMPLES];
    let mut frames = 0u64;

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
        transport.push_frame(&samples).await.map_err(|e| format!("voice: {e}"))?;
        frames += 1;
    }

    match audio.error().await {
        Some(error) => Err(error),
        None if frames == 0 => Err("no audio came back".into()),
        None => Ok(()),
    }
}
