# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

`Fluxer Music Bot Build Plan.md` is the source of truth for scope, architecture and decisions. Built so far is the first slice: playback of anything yt-dlp supports (single tracks, playlists, YouTube search) with `play` (a number after a playlist link skips that many tracks, via yt-dlp `--playlist-items`), `next` (priority queue), `skip`, `seek` (restarts the pipe with ffmpeg `-ss`; a per-run id keeps the panel's track `generation` unchanged), `queue`, `shuffle` (regular lane only), `clear`, `pause`, `resume`, `volume` (runtime, 0–200 %), `loop off|track|queue`, `join`, `leave`, `web`, `help`, plus a music channel (`MUSIC_CHANNEL_ID`).

There is also a web player (`src/web/`, page in `web/`). It is styled like a late-90s desktop media player: a main window, a playlist editor with drag-and-drop reorder, multi-select and delete, and a search window with YouTube Music, SoundCloud and Jellyfin tabs. Jellyfin supports songs, albums, playlists, artists and instant mixes. Login is Fluxer OAuth2 (PKCE, `identify` only), and only members of a server may open its player. Nothing is persisted yet (no SQLite; web sessions live in an encrypted cookie), and there is no Spotify support. License: GPL-2.0.

## Commands

```sh
cargo build
cargo test                                   # unit tests, offline
cargo test parses_commands                   # one test by name
cargo test -- --ignored                      # live yt-dlp → ffmpeg and YouTube Music tests, need network
cargo clippy --all-targets
FLUXER_URL=https://chat.example.com FLUXER_TOKEN=... cargo run
docker build --pull -t breakcore-moose . && docker run -d --restart unless-stopped --env-file .env -v "$PWD/cookies.txt:/home/bot/cookies.txt" breakcore-moose
```

The `Dockerfile` builds on `rust:*-slim-trixie` and runs on `debian:trixie-slim` with a static ffmpeg, Deno and the standalone `yt-dlp_linux`. yt-dlp and Deno track their latest releases, so `docker build --pull` refreshes them. ffmpeg stays pinned. Inside the container the bot can't update yt-dlp itself (it runs as a non-root user).

yt-dlp can log in with a Netscape cookie file (`cookies.txt`, export it from a logged-in browser or with `yt-dlp --cookies-from-browser firefox --cookies cookies.txt`). If one is configured, every yt-dlp call gets `--cookies <file>`; without one the bot still works, only without login. The file is gitignored and dockerignored so it never ends up in the image (which is pushed to Docker Hub). Mount it at run time: `docker run ... -v "$PWD/cookies.txt:/home/bot/cookies.txt" breakcore-moose`. The container runs as uid 10001, and yt-dlp writes refreshed cookies back, so the mounted file must be writable by that uid.

Runtime needs `yt-dlp`, `ffmpeg` and `deno` on PATH. Settings are environment variables, also read from `.env` (see `.env.example` and `src/config.rs`): `FLUXER_TOKEN` (required), then `FLUXER_URL` (the API base is discovered from `/.well-known/fluxer`) or `FLUXER_API_URL` (overrides discovery), plus `MUSIC_CHANNEL_ID`, `VOLUME` (starting volume in percent; `!volume` and the web player change it at runtime, and the web slider goes 0–100 % where 100 % is `VOLUME`), `BOT_PREFIX`, `YTDLP_PATH`, `YTDLP_COOKIES` (cookie file; defaults to `cookies.txt` in the working directory if present), `FFMPEG_PATH`, `MAX_TRACK_SECONDS` and `PLAYLIST_LIMIT`. The web player is on only when `WEB_PUBLIC_URL` is set, and then it needs `FLUXER_OAUTH_CLIENT_ID` and `FLUXER_OAUTH_CLIENT_SECRET`; optional are `WEB_BIND` (default `0.0.0.0:8080`) and `WEB_SECRET` (cookie key; without it logins end on restart). Jellyfin needs `JELLYFIN_URL` and `JELLYFIN_API_KEY`, with optional `JELLYFIN_USER`. Log level is set with `RUST_LOG`.

**LiveKit pins:** `Cargo.toml` pins the whole livekit/libwebrtc crate set with `=` versions. Newer patch releases of the livekit sub-crates fail to compile against each other. Bump the set together, and keep `livekit` equal to what fluxer-rust resolves to.

## Code map

- `bot.rs` is the fluxer-rust `EventHandler`. It parses chat, resolves tracks and talks to the player. Its `Gateway` (shared with the web server) holds the newest `Context` and the user voice channels from `VOICE_STATE_UPDATE`, so the web can join voice too.
- `core/player.rs` is one `GuildPlayer` task per server. It owns the queue (`core/queue.rs`: entries with stable ids, and a priority lane, filled by `!next`, that plays out before the regular lane), the voice connection, playback, pause (which ends the pipe and restarts it at the position, like seek), volume (scaled per sample, so it changes instantly), repeat and a short history for "previous". All replies come back over oneshot channels. `core/spectrum.rs` turns the PCM into band levels for the web visualizer, published on a second `watch` channel.
- `music_channel.rs` runs a panel task per music channel. It watches the player's `Snapshot` (a `watch` channel, published after every player message) and edits one embed. When the track changes (a new `generation`), it deletes the old embed and posts a new one. It also deletes every message in the channel on startup. In the music channel, `bot.rs` deletes every message that isn't the bot's own, treats unprefixed text as `play`, and posts replies that delete themselves after 10 s.
- `media/jellyfin.rs` browses Jellyfin. Jellyfin tracks have `Stream::Jellyfin(id)`, and ffmpeg reads them straight from the server (no yt-dlp, no `ensure_public`, since the URL comes from config). `media/ytmusic.rs` searches songs through YouTube Music's own `youtubei/v1/search` endpoint, because yt-dlp's YouTube Music search returns channels. From some networks (VPN or datacenter IPv4) it returns nothing, and the web search then falls back to `ytsearch`.
- `web/` is the axum server. `auth.rs` handles Fluxer OAuth2 and the session cookie (`PrivateCookieJar`). `api.rs` serves `/api/me`, search, Jellyfin drill-in and the public `/img/jellyfin/{id}` cover proxy. `ws.rs` runs one WebSocket per page: it checks the Origin, sends `state`/`queue`/`levels`, and turns page messages into `PlayerMsg`s. Search results are cached server-side behind random tokens, so the page never sends a URL for yt-dlp to fetch. Every action is rate-limited per user, and membership is rechecked every 5 minutes.
- The page in `web/` (`index.html`, `app.js`, `style.css`) is plain JS without a build step; `include_str!` embeds it into the binary. Drag and drop is its own pointer-event code: rows drag with the mouse, and on touch from the ≡ handle or after a long press.
- `media/` decides what an input is (`source.rs`: any http(s) link goes to yt-dlp, anything else is a YouTube search, and links that resolve to loopback, private or link-local addresses are refused, because yt-dlp's generic extractor would fetch them), fetches metadata (`ytdlp::resolve`) and runs the PCM pipe (`ytdlp::AudioStream`).
- `voice/` holds the `VoiceTransport` trait and its LiveKit implementation.
- The `core` module name shadows the built-in `core` crate, so refer to it as `crate::core`.

**Known fluxer-rust gaps:** its `Guild` model drops GUILD_CREATE voice states. So users who were already in voice when the bot started aren't seen until they rejoin, and `!join <channel name>` is the workaround. Its `Context` (gateway sender, live rooms) is rebuilt on every gateway reconnect. If the `gateway/bot` lookup fails, it silently falls back to the public `gateway.fluxer.app`. Its `Embed` serializes unset fields as `null`.

## What is being built

A music bot for a self-hosted Fluxer instance (a Discord-like chat platform), built as **one Rust binary** (stable, edition 2024, tokio). That binary runs the Fluxer bot, the audio player and an axum web dashboard, so chat commands and the web page always edit the same queue. Audio is streamed from YouTube with yt-dlp and never saved to disk. Spotify links supply only metadata and are matched to YouTube videos.

Planned layout (from the plan): `src/{bot,commands,music_channel,core,media,voice,storage,web}`. The plan's `fluxer/` module is `bot.rs` here, because a module named `fluxer` would clash with the fluxer-rust library name. The plan's Svelte + SortableJS + Vite front end was replaced by plain JS in `web/`, so the Docker build needs no Node. Still planned: `migrations/` (sqlx) and `docker-compose.yml`.

## Architectural invariants

These rules span several modules and are easy to break by accident:

- **One serialized queue per server.** Every change, from chat or the web, is a message on an mpsc channel to that server's single `GuildPlayer` tokio task. Never mutate queue state from anywhere else. The task owns a state machine (idle → connecting → playing ⇄ paused → idle, plus recovering) and publishes snapshots on a `watch` channel, which dashboards subscribe to.
- **Entries have stable ids.** Reorders use the entry id with "move X after Y" (`move {entryId, afterEntryId, baseVersion}`), never an index. Duplicate songs are separate entries.
- **All voice code sits behind the `VoiceTransport` trait** (`join`, `push_frame`, `clear_buffer`, `leave`, plus ready and disconnected events). Fluxer plans to replace LiveKit with its own voice system, so nothing outside `voice/` should touch LiveKit.
- **Don't use fluxer-rust's `play_music`.** Join with `ctx.join_voice`, publish our own `NativeAudioSource` track on the exposed LiveKit `room`, and unpublish the library's default track. Pin the `livekit` crate to the same version fluxer-rust uses (0.7.x), so only one copy of native WebRTC is built.
- **Audio pipeline:** `yt-dlp -f bestaudio -o -` → ffmpeg → 48 kHz stereo s16 PCM → 20 ms frames (960 samples per channel) → `capture_frame`. Pipe yt-dlp into ffmpeg; never hand ffmpeg a stream URL, because those URLs expire and are tied to the requesting IP. Spawn both processes with `kill_on_drop(true)`.
- **Resolve lazily.** At add time, fetch only metadata (`yt-dlp -J` / `--flat-playlist -J`). Open audio streams when a track starts. Match Spotify tracks to YouTube when they are about 3 places from playing, and cache the matches in SQLite (`track_matches`).
- **yt-dlp, Deno and ffmpeg stay external binaries** run through `tokio::process`, so yt-dlp can be updated daily without a rebuild. Don't port them to Rust or wrap them in crates.
- **Spotify:** call the Web API directly with reqwest. Wrapper crates broke on the Feb 2026 endpoint renames. Auth is the authorization code flow with a stored refresh token, and the redirect URI must use `127.0.0.1`, not `localhost`. Playlists the user doesn't own fall back to the public embed page.
- **Wrap fluxer-rust (0.2.x) in a thin adapter.** It is young and mostly undocumented. The base URLs come from the instance's `/.well-known/fluxer`.
- **Music channel cleanup** deletes every message except the panel. The user chose this over the plan's narrower rule. Nothing outside the configured music channels is ever deleted.
- **Persistence:** SQLite via sqlx. The in-memory player is the live truth, and each server's queue is written to disk at most once per second.

## Build and deploy notes

- Use Debian-based Docker images, not Alpine, because `livekit` links native WebRTC. The build stage needs `pkg-config`, `libglib2.0-dev` and `libva-dev`. BuildKit cache mounts stand in for cargo-chef. A `docker-compose.yml` doesn't exist yet.
- Settings come from environment variables via serde into one typed config (`config.rs`).
- Logging uses `tracing` with JSON output and `tracing-appender` for rotating files.
- Build order: voice playback first (Phase 0 checks the risky unknowns), then the chat bot, then persistence, then the dashboard.

Vinto (another Fluxer music bot) is source-available for private use only. Read it for ideas, but don't copy its code.
