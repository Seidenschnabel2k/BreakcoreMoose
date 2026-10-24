# Fluxer Music Bot: Build Plan

Oct 19, 2026 · @Nils

Music bot with website for interaction, also controllable form chat.

bot gets bound to a channel.\
the bot cleans up after itself and commands from user.

## Summary

Build one Rust service that runs the Fluxer bot, the music player and the dashboard web server, so chat and web always edit the same queue. Start with the riskiest piece: getting a bot to play audio in a Fluxer voice channel.

**What done looks like**

- `!play <link or search>` appends to the end of the queue and starts playback when nothing is playing.
- bot gets bound to a channel in this channel  also play when only a link is posted.
- the bot cleans up after itself and commands from user in this channel.
- Audio comes from YouTube through yt-dlp and is streamed, never saved to disk.
- Spotify tracks, albums and playlists play by finding the same song on YouTube.
- YouTube and Spotify playlist links load into the queue in their original order.
- A web page shows the queue live and lets listeners reorder it by drag and drop.

**Setup** (from your answers under Decisions)

- Rust (current stable, edition 2024) on tokio, with the [fluxer-rust](https://crates.io/crates/fluxer-rust) crate. The bot, the player and the web server run as one binary.
- Runs in Docker or a VM on your home network, against your own Fluxer instance. The bot reads base URLs from your instance's `/.well-known/fluxer` ([Fluxer API docs](https://docs.fluxer.app/)).
- Everyone in a server can use every command and the dashboard; only binding the music channel needs Manage Server.
- Playlists are links added to the queue; the bot keeps no saved playlists.
- An activity log records who played, skipped, moved or removed what.

## Architecture

Chat commands and the web page are two front doors to the same per-server queue; only the player talks to yt-dlp and voice.

&#91;embedded content: bot architecture · 11 parts\]

Example, `!play <Spotify playlist>`: the command parses the link, the Spotify resolver returns tracks, and QueueService sends them to that server's task, which appends and saves them. If the player is idle, it asks the matcher for a YouTube video, starts the audio pipeline and pushes frames to the voice transport. QueueService then sends a snapshot, so every open dashboard updates.

## Voice and playback

Audio flows yt-dlp → ffmpeg → raw PCM → a LiveKit audio track, and all voice code sits behind one small adapter. The adapter matters because Fluxer is replacing LiveKit with its own voice server in 2026 ([roadmap](https://fluxer.app/blog/roadmap-2026)).

**How the bot joins voice today** ([Fluxer voice docs](https://docs.fluxer.app/voice/))

1. `ctx.join_voice(guild_id, channel_id)` in fluxer-rust sends the Voice State Update (opcode 4) and waits up to 10 seconds for the Voice Server Update with a LiveKit `endpoint` and `token` ([fluxer-rust `Context`](https://docs.rs/fluxer-rust/latest/fluxer/client/struct.Context.html)). The grant lives 600 seconds.
2. It connects with the `livekit` crate and returns a `FluxerVoiceConnection` whose LiveKit `room` is public.
3. The bot publishes its own audio track on that room, as described under the adapter below.

**Pipeline for each track**

1. `yt-dlp -f bestaudio -o -` writes the audio stream to stdout.
2. `ffmpeg` reads it and outputs 48 kHz stereo 16-bit PCM.
3. The player reads 20 ms frames (960 samples per channel) from ffmpeg's stdout with tokio, scales samples for volume, and pushes them with `NativeAudioSource::capture_frame`.
4. LiveKit encodes Opus at the channel bitrate: 64 kbps by default, 96 kbps maximum without server boosts.

Pipe yt-dlp into ffmpeg rather than giving ffmpeg a stream URL. YouTube URLs expire and are tied to the requesting IP, so letting yt-dlp download avoids most 403 errors. Seeking comes later, by restarting the pipeline at an offset.

**The adapter: `VoiceTransport`**

- A `VoiceTransport` trait with `join`, `push_frame`, `clear_buffer` and `leave`, plus ready and disconnected events.
- fluxer-rust's own `play_music(path, ...)` takes only a file or URL, runs ffmpeg itself, and posts ffmpeg errors as a chat message ([source](https://docs.rs/fluxer-rust/latest/src/fluxer/voice/mod.rs.html)). That rules out the yt-dlp pipe, instant volume and a tidy music channel.
- So the LiveKit implementation joins with `join_voice`, publishes its own `NativeAudioSource` track on the exposed `room`, and unpublishes the library's default track. Confirm in Phase 0 that listeners hear only the bot's track.
- Depend on the same `livekit` version fluxer-rust uses (0.7.x today), so only one copy of the native WebRTC library is built.
- Longer term, offer fluxer-rust a small pull request that exposes the audio source; the crate is Apache-2.0, so a fork is also fine.
- You run your own Fluxer instance, so Fluxer's new voice system reaches you only when you upgrade. Hold that upgrade until a new `VoiceTransport` is ready and tested.

**Reliability rules**

- yt-dlp and ffmpeg are spawned with `kill_on_drop(true)`. Skip and stop drop both processes and clear the audio buffer, so audio stops at once and no process is left running.
- Watchdog: no PCM for 10 seconds while playing restarts the pipeline once, then skips with a message.
- On a voice disconnect, call `join_voice` again for a fresh grant and continue the current track.
- Leave after 5 minutes with an empty queue or nobody else in the channel.
- Your instance needs voice enabled (`features.voice_enabled` in its discovery document), or every join is refused.
- Encrypted voice channels: bots are exempt from the join refusal, but whether listeners hear the bot's audio is untested. Test it in Phase 0.

## Queue and commands

`!play` always appends and starts playback only when the player is idle. Every change, from chat or the web page, goes through one tokio task per server that applies changes one at a time from a channel, so two requests can never interleave and lose a track.

**Rules for `!play <search or link>`**

1. The caller must be in a voice channel. If the bot is already playing in a different channel of that server, refuse with a message.
2. Resolve the input to one or more tracks: a video, a search hit, a Spotify track, or a playlist.
3. Append all tracks to the end in their original order, as one operation.
4. If the player is idle (nothing playing, not paused), join and start the first new track. If paused, only append.
5. Reply with what was added and its position; for playlists, the count added and the count skipped.

Each queue entry gets its own id, so the same song can appear twice and a reorder always moves the right copy.

| Command | What it does |
| --- | --- |
| `play <q>`, `p`, or a bare link in the music channel | Append; start if idle |
| `playnext <q>` | Insert as the next track |
| `skip` | Go to the next track |
| `pause`, `resume` | Pause or continue the current track |
| `stop` | Clear the queue and leave voice |
| `queue`, `q` | Show the next 10 entries and the dashboard link |
| `np` | Show the current track and its progress |
| `move <from> <to>` | Reorder from chat |
| `remove <n>` | Remove one entry |
| `clear` | Remove all upcoming entries |
| `shuffle` | Shuffle upcoming entries |
| `loop off/track/queue` | Set loop mode |
| `volume <0-150>` | Set volume |
| `history` | Show the last 10 activity log entries |
| `dashboard` | Link to the web page for this server |
| `musicchannel` | Bind the current channel as the music channel (Manage Server) |

Everyone in the server can use every command, in chat and on the dashboard. The one exception is `musicchannel`, which needs the Manage Server permission.

**Music channel**

Each server binds one channel where links alone control the bot and the bot keeps the channel tidy. It can be a voice channel's own text chat, since Fluxer voice channels also hold messages ([Fluxer voice docs](https://docs.fluxer.app/voice/)).

- A message that is only a YouTube or Spotify link is handled as `!play <link>`. Ordinary chat is left alone.
- One panel message shows now playing, the next 5 entries and the dashboard link. The bot edits it in place, at most every 5 seconds, instead of posting new messages.
- After a command or link is handled, the bot deletes the user's message. Its own confirmations and errors delete themselves after 10 seconds.
- If chat pushes the panel more than 10 messages up, the bot deletes it and posts it again at the bottom.
- On startup the bot deletes its own old messages in the channel and posts a fresh panel.
- In other channels the bot only replies with a pointer to the music channel, which also deletes itself.
- The bot needs permission to manage messages in that channel; without it, cleanup is skipped and a warning is logged.

Fluxer has no buttons or slash commands yet; both are planned for 2026 ([roadmap](https://fluxer.app/blog/roadmap-2026)). Until then the now-playing message is text plus the dashboard link, and the command handlers keep a shape that maps onto slash commands later.

## YouTube with yt-dlp

yt-dlp does both search and streaming, but it needs a JavaScript runtime and frequent updates to keep working on YouTube. Since version 2025.11.12, full YouTube support needs an external runtime such as Deno plus the yt-dlp-ejs component ([announcement](https://github.com/yt-dlp/yt-dlp/issues/15012)).

| Input | What the bot adds |
| --- | --- |
| watch, youtu.be, shorts or music.youtube.com link | That one video |
| `playlist?list=` link | The whole playlist (see Playlists) |
| Video link that also carries `&list=` | Only the video; the reply offers the playlist link |
| Auto-generated mix (`list=RD…`) | Only the video, since mixes are effectively endless |
| Plain text | Top non-live search result |

**Metadata now, audio later.** At add time, `yt-dlp -J` fetches title, length and thumbnail only. The audio stream is opened when the track starts, because stream links expire.

**Host requirements**

- yt-dlp updated daily; a stale yt-dlp is the most common cause of a broken music bot.
- Deno and yt-dlp-ejs installed alongside it.
- Keep yt-dlp as a separate binary rather than a Rust port, so it can be updated daily without rebuilding the bot.
- Your home network is a good place for this: YouTube blocks home addresses less often than VPS ones, so start without a proxy.
- Optional: the bgutil PO-token provider plugin, which the existing Fluxer bot [Vinto](https://github.com/notgapriel/vinto-the-fluxer-bot) runs as a sidecar container.
- Avoid cookies from your main Google account; if a video needs them, use a spare account.

**Failures**

- Private, deleted, age-restricted or region-blocked video: retry once with a different yt-dlp player client, then mark the entry failed, post one message and skip.
- Live streams and tracks over 3 hours are refused by default; both limits are settings.

## Spotify links

Spotify supplies only the song name, artist and length; the audio always comes from a matched YouTube video. Spotify's 2026 API rules make playlists the hard part.

**What Spotify's API allows now** (Development Mode, since February and March 2026)

- The app owner needs Spotify Premium, and each app allows 5 users ([migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)).
- Single tracks and albums still work: `GET /tracks/{id}` and `GET /albums/{id}/tracks` ([changelog](https://developer.spotify.com/documentation/web-api/references/changes/february-2026)).
- Playlist contents come back only for playlists the signed-in user owns or collaborates on. Other playlists return name and cover only.
- Spotify has said it is moving away from the Client Credentials flow for metadata ([rspotify discussion](https://github.com/ramsayleung/rspotify/issues/550)), so sign in once as a user instead.

**Where track lists come from, in order**

1. Web API with your own Spotify account, using the Premium account and app you already have. Log in once with the authorization code flow and store the refresh token. Covers tracks, albums and playlists you own or collaborate on.
2. Spotify's public embed page, for other people's public playlists. It is unofficial, can change without notice, and may not return very long playlists in full.
3. If both fail, the reply says what failed instead of silently adding nothing.

For the one-time login, register a loopback redirect such as `http://127.0.0.1:8888/callback` in your app; Spotify no longer accepts `localhost` ([redirect URI guide](https://developer.spotify.com/documentation/web-api/tutorials/migration-insecure-redirect-uri)). Call the API with reqwest directly rather than through a wrapper crate: the February 2026 renames broke playlist loading in older wrappers, for example in [Music Assistant](https://github.com/music-assistant/support/issues/5114).

Accepted forms: `open.spotify.com/{track|album|playlist}/{id}`, links with an `/intl-xx/` prefix, `spotify:` URIs, and `spotify.link` short links (follow the redirect first).

**Matching a Spotify track to YouTube**

1. Search YouTube for `artist - title` and take 5 results.
2. Score each: length within 3 seconds is a strong match, more than 15 seconds off is rejected; add points for title word overlap and for "- Topic" or official artist channels.
3. Subtract points for live, cover, remix, sped up, nightcore or karaoke, unless the Spotify title contains that word.
4. Take the best result above a threshold; otherwise mark the entry "no match" and skip it.
5. Cache Spotify id → YouTube id in SQLite. Later, the dashboard can fix a wrong match and the fix is saved.

**Match lazily.** Entries enter the queue with Spotify info at once and show on the dashboard. Each is matched when it is 3 places from playing, plus a background worker matching 2 at a time, so a 200-track playlist is added in seconds.

## Playlists

A playlist link adds every track in order as one queue operation, and only the first track is resolved before playback starts. The target is sound within 5 seconds even for a 500-track list.

1. Read the list cheaply: `yt-dlp --flat-playlist -J` for YouTube, the Spotify resolver for Spotify.
2. Drop private, deleted and over-length entries, and count them.
3. Cap one add at 500 tracks (a setting) and say how many were cut.
4. Append everything as pending entries; resolve audio and Spotify matches only near play time.
5. Reply with a summary, for example: Added 143 tracks from Road Trip (2 unavailable).

Playlists exist only as links added to the queue. The bot keeps no saved playlists of its own.

## Web dashboard

The dashboard is a page served by the bot itself: a signed-in listener sees the queue live and drags entries to reorder it. It calls the same queue service as the chat commands, so chat and web can never disagree.

**Login** ([Fluxer OAuth2](https://docs.fluxer.app/http-api/oauth2/))

- Register the OAuth2 application on your own Fluxer instance. Use the authorization code flow with PKCE and the `identify` scope, plus `guilds` to list the user's servers. Access tokens last 7 days, refresh tokens 30 days.
- The redirect URI must match the registered one byte for byte: same scheme, port and trailing slash.
- After login the server keeps its own session in an HttpOnly cookie and does not need the Fluxer token again.
- Any member of a server may use that server's page; the bot checks membership from its own gateway data.

**Reaching the page.** Listeners open the dashboard from outside your home network, so publish it through the reverse proxy that already serves your Fluxer instance, for example `music.<your domain>` with HTTPS. Only the web port is exposed; the bot itself needs outgoing connections only.

`!queue` and `!dashboard` reply with a link such as `https://music.example.com/g/<server id>`.

**First version of the page**

- Now playing: title, artwork, progress bar, pause and skip.
- Queue list with drag handles (SortableJS, which also works with touch), remove, and play next.
- An add box for a link or search text.
- A History tab showing the activity log: who played, skipped, moved or removed what, and when.
- Live updates over one WebSocket per open page.

| Direction | Message | Purpose |
| --- | --- | --- |
| Server → page | `snapshot {version, nowPlaying, queue}` | Full state on connect and after every change |
| Server → page | `position {entryId, startedAt, pausedAt}` | Page draws progress itself, no per-second messages |
| Page → server | `move {entryId, afterEntryId, baseVersion}` | Reorder by entry id, not by index |
| Page → server | `remove`, `add`, `skip`, `pause`, `resume` | Controls |
| Server → page | `error {code, message}` | An action was refused |

A move says "put X after Y", so it lands correctly even if someone else changed the queue meanwhile. The page moves the row at once, then treats the next snapshot as the truth; a refused move snaps back.

**Security:** check the WebSocket Origin header, validate every message with a schema, rate-limit actions per user, and serve over HTTPS behind a reverse proxy such as Caddy.

## Data and persistence

SQLite is enough: one file, no extra service, and queues survive a restart. The in-memory player is the live truth; every change writes that server's queue to disk, at most once per second.

| Table | Holds |
| --- | --- |
| `guild_settings` | Prefix, music channel, panel message id, default volume, idle timeout, maximum queue length |
| `queue_entries` | Server, entry id, position, source and source id, title, artist, length, thumbnail, requested by, added at, match status |
| `player_state` | Server, voice channel, current entry, position, paused, loop mode, volume |
| `track_matches` | Spotify id → YouTube id, score, matched at, fixed by hand |
| `activity_log` | Time, server, user, action (play, skip, move, remove, clear, stop), track, source (chat or web) |
| `web_sessions` | Session id, user id, expiry |

On startup the bot loads all queues. Servers that were playing are rejoined and continue from the current entry; a setting turns this off.

**Logs.** The activity log keeps 90 days by default (a setting) and shows in `!history` and on the dashboard. Separately, the app writes structured logs (errors, yt-dlp failures, voice reconnects) to rotating files and to Docker's log output.

## Stack and layout

One Cargo project building one Rust binary, plus a small JavaScript front end built to static files that the binary serves.

| Part | Choice | Why |
| --- | --- | --- |
| Language | Rust stable, edition 2024, tokio runtime | Your choice |
| Fluxer gateway and REST | [fluxer-rust](https://crates.io/crates/fluxer-rust) 0.2.x behind a thin adapter | Event handler, cache, voice join; a young crate, so wrapped |
| Voice | `livekit` crate, same version as fluxer-rust, behind `VoiceTransport` | Frame-level control, swappable later |
| Media | yt-dlp, Deno, ffmpeg via `tokio::process` | Updated outside the code |
| Spotify | reqwest with a stored refresh token | Uses the 2026 endpoint names directly |
| Web server | axum with its WebSocket support, tower-http for static files | Shares the bot's tokio runtime |
| Dashboard login | `oauth2` crate with PKCE | Standard authorization code flow |
| Front end | Svelte and SortableJS, built with Vite | Drag and drop with touch support |
| Storage | SQLite with sqlx and its migrations | Checked queries, one file |
| Messages and settings | serde; settings from environment variables | One typed source of truth |
| Logs | tracing with JSON output, tracing-appender for rotating files | fluxer-rust's `log` output is captured too |
| Deployment | Docker Compose on your home server: bot, optional PO-token sidecar | Sits behind your existing reverse proxy |

fluxer-rust 0.2.1 came out on 9 September 2026, has one maintainer, and about a quarter of its API is documented ([docs.rs](https://docs.rs/fluxer-rust/latest/fluxer/)). Two checks belong in Phase 0:

- Its README mentions only `.api_url()` for self-hosted instances. Confirm the gateway connection also goes to your instance, not fluxer.app.
- For any gateway call it lacks, check whether the public `ctx.gateway_tx` sender is enough.

Build on Debian-based images rather than Alpine, because the `livekit` crate links a native WebRTC library. fluxer-rust's voice also needs pkg-config, libglib2.0-dev and libva-dev on Debian. Use cargo-chef in the Dockerfile so dependencies are cached; the first build is slow.

```text
fluxer-music/
  Cargo.toml
  migrations/        sqlx migrations
  src/
    main.rs          start bot and web server on one runtime
    config.rs        read and check settings
    fluxer/          event handler, client adapter, permissions
    commands/        playback, queue, settings
    music_channel/   link detection, panel message, cleanup
    core/            Queue, GuildPlayer task, QueueService handles
    media/           ytdlp, youtube, spotify, matcher, url parsing
    voice/           VoiceTransport trait and its LiveKit version
    storage/         data access, activity log
    web/             axum routes, OAuth, WebSocket hub
  web/               the drag-and-drop page (Svelte)
  Dockerfile
  docker-compose.yml
```

Each server's player is one tokio task that owns a small state machine: idle → connecting → playing ⇄ paused → idle, plus a recovering state for watchdog restarts. Chat commands and the web server send it messages over an mpsc channel. It publishes each new snapshot on a watch channel that every open dashboard subscribes to.

Vinto, an existing Fluxer music bot, is worth reading for ideas, but its license is source-available and private-use only, so don't copy its code.

## Build phases

Voice comes first because it is the part most likely to fail or change; nothing else is worth building until the bot plays a YouTube stream cleanly.

&#91;embedded content: build phases · 7 gates\]

Each phase leaves something usable: after Phase 1 the bot is already a working YouTube music bot in chat. The dashboard comes after persistence, so it is built on the final data model.

## Risks

YouTube blocking is the main recurring risk. Fluxer's voice change is still large, but running your own instance means it arrives only when you choose to upgrade.

| Risk | Likelihood | Mitigation |
| --- | --- | --- |
| Fluxer replaces LiveKit with its own voice system in 2026 | High, on your schedule | All voice code behind `VoiceTransport`; upgrade your instance only after a new transport is tested |
| YouTube changes break yt-dlp | High, recurring | Daily yt-dlp updates, Deno, PO-token plugin, home IP, clear error messages |
| fluxer-rust falls behind Fluxer's API (0.2.x, one maintainer) | Medium | Thin adapter around it; Apache-2.0, so fork or patch it if needed |
| Bot's own LiveKit track conflicts with fluxer-rust's default track | Medium | Unpublish the default track; test in Phase 0; offer an upstream fix |
| Native WebRTC build in Docker is slow or fails | Medium | Debian-based images, cargo-chef caching, pinned `livekit` version |
| Spotify tightens its API further | Medium | Embed-page fallback, cached matches; Spotify stays optional |
| Encrypted voice channels drop or mute bot audio | Unknown | Test in Phase 0 before building on it |
| Cleanup deletes a message it shouldn't | Low | Delete only the bot's own messages and messages it handled as commands or links |
| Streaming YouTube audio is against YouTube's terms | Certain | Keep the bot private to your own instance |

## Decisions

Settled from your answers; the plan above follows them. One small item is still open.

- [x] Language: Rust with the fluxer-rust crate (changed from Python on 22 October).
- [x] Host: Docker or a VM on your home network.
- [x] Spotify: your Premium account and existing developer app.
- [x] Rights: everyone can use every command and the dashboard.
- [x] Playlists: links into the queue only, no saved playlists.
- [x] Fluxer: your own instance.
- [x] Music channel: bare links play, and the bot cleans up its own messages and handled commands.
- [x] Log: an activity log of who did what, shown in `!history` and on the dashboard, plus app logs on disk.
- [ ] Activity log retention: 90 days assumed.

## Sources

- [Fluxer roadmap 2026](https://fluxer.app/blog/roadmap-2026)
- [Fluxer API: Voice](https://docs.fluxer.app/voice/)
- [Fluxer API: OAuth2](https://docs.fluxer.app/http-api/oauth2/)
- [Fluxer API: Introduction](https://docs.fluxer.app/)
- [fluxer-rust on crates.io](https://crates.io/crates/fluxer-rust) and its [docs.rs page](https://docs.rs/fluxer-rust/latest/fluxer/)
- [fluxer-rust: `Context` and `join_voice`](https://docs.rs/fluxer-rust/latest/fluxer/client/struct.Context.html)
- [fluxer-rust: voice module source](https://docs.rs/fluxer-rust/latest/src/fluxer/voice/mod.rs.html)
- [yt-dlp: external JavaScript runtime now required](https://github.com/yt-dlp/yt-dlp/issues/15012)
- [Spotify: February 2026 migration guide](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
- [Spotify: Web API changelog, February 2026](https://developer.spotify.com/documentation/web-api/references/changes/february-2026)
- [Spotify: migrate away from insecure redirect URIs](https://developer.spotify.com/documentation/web-api/tutorials/migration-insecure-redirect-uri)
- [rspotify issue on the 2026 changes](https://github.com/ramsayleung/rspotify/issues/550)
- [Music Assistant issue: Spotify playlists returning 403](https://github.com/music-assistant/support/issues/5114)
- [Vinto music bot for Fluxer](https://github.com/notgapriel/vinto-the-fluxer-bot)
