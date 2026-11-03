# BreakcoreMoose

A music bot for self-hosted [Fluxer](https://fluxer.app) instances. It's one Rust binary that runs the chat bot, the audio player and an optional web player, so chat commands and the web page always edit the same queue.

- Plays anything [yt-dlp](https://github.com/yt-dlp/yt-dlp) supports: single tracks, playlists and YouTube searches. Audio is streamed and never written to disk.
- Has a priority queue (`!next`), seek, pause, shuffle, loop and a volume control that takes effect right away.
- Can keep a **music channel** with a live now-playing panel. Anything posted there without the prefix gets played.
- Has a **web player** styled like a late-90s desktop media player. It has a playlist editor with drag-and-drop, plus search over YouTube Music, SoundCloud and (optionally) **Jellyfin**. Login goes through Fluxer OAuth2.

## Deploy with Docker (recommended)

The image is published on Docker Hub as [`seidenschnabel2k/breakcoremoose`](https://hub.docker.com/r/seidenschnabel2k/breakcoremoose). It contains the bot, a static ffmpeg, Deno and the latest standalone yt-dlp.

1. Create a bot on your Fluxer instance and copy its token.
2. Write a `.env` file (see [Configuration](#configuration) below, or copy [`.env.example`](.env.example)):

   ```sh
   FLUXER_TOKEN=your-bot-token
   FLUXER_URL=https://chat.example.com
   ```

3. Run it:

   ```sh
   docker pull seidenschnabel2k/breakcoremoose
   docker run -d --name breakcore-moose --restart unless-stopped \
     --env-file .env \
     seidenschnabel2k/breakcoremoose
   ```

4. Invite the bot to your server, join a voice channel and type `!play <link or search>`.

**With the web player**, publish its port and put it behind a reverse proxy that terminates TLS and passes WebSocket upgrades:

```sh
docker run -d --name breakcore-moose --restart unless-stopped \
  --env-file .env \
  -p 127.0.0.1:8080:8080 \
  seidenschnabel2k/breakcoremoose
```

**With YouTube cookies** (see [`YTDLP_COOKIES`](#yt-dlp-and-ffmpeg)), mount the file into the container's working directory. The container runs as uid 10001, and yt-dlp writes refreshed cookies back, so the file must be writable by that uid:

```sh
sudo chown 10001 cookies.txt
docker run -d --name breakcore-moose --restart unless-stopped \
  --env-file .env \
  -v "$PWD/cookies.txt:/home/bot/cookies.txt" \
  seidenschnabel2k/breakcoremoose
```

### Updating

YouTube breaks old yt-dlp versions often, so update the image regularly:

```sh
docker pull seidenschnabel2k/breakcoremoose
docker rm -f breakcore-moose
docker run -d ... # the same command as above
```

The bot runs as a non-root user and can't update yt-dlp inside the container. A new image is the way to get a new yt-dlp.

## Configuration

All settings are environment variables. The bot also reads a `.env` file from its working directory, and real environment variables override it. Empty values count as unset.

### Fluxer connection

| Variable | Required | Default | Description |
|---|---|---|---|
| `FLUXER_TOKEN` | **yes** | – | The bot token from your Fluxer instance. |
| `FLUXER_URL` | one of these two | – | Base URL of your instance, e.g. `https://chat.example.com`. The bot reads the API address from `<FLUXER_URL>/.well-known/fluxer` at startup. It logs a warning if the instance reports voice as disabled. |
| `FLUXER_API_URL` | one of these two | – | The API base directly, e.g. `https://api.example.com/v1`. Skips discovery and takes precedence over `FLUXER_URL`. |

### Chat and playback

| Variable | Default | Description |
|---|---|---|
| `BOT_PREFIX` | `!` | Command prefix. |
| `MUSIC_CHANNEL_ID` | – | ID of a text channel to use as the music channel. Separate several with commas, at most one per server. In a music channel the bot: <br>• deletes **every** message there on startup and keeps a single now-playing panel; <br>• plays any message without the prefix as `play`; <br>• deletes every message that isn't its own, and its own replies after 10 seconds. <br>The bot needs the *Manage Messages* permission there. Nothing outside these channels is ever deleted. |
| `VOLUME` | `50` | Starting volume, in percent of the source volume (0–200). This is the "100 %" of `!volume` and the web slider: `!volume 100` plays at `VOLUME`, `!volume 50` at half of it. Changes made at runtime aren't saved across restarts. |
| `MAX_TRACK_SECONDS` | `10800` (3 h) | Single tracks longer than this are refused. Live streams are always refused. |
| `PLAYLIST_LIMIT` | `500` | The most tracks one playlist link may add. |

### yt-dlp and ffmpeg

| Variable | Default | Description |
|---|---|---|
| `YTDLP_PATH` | `yt-dlp` | yt-dlp executable (name on `PATH` or a full path). |
| `YTDLP_COOKIES` | `cookies.txt` if it exists | A Netscape-format cookie file that is passed to every yt-dlp call as `--cookies`. Use it so yt-dlp is logged in to YouTube, which helps against "confirm you're not a bot" blocks and age-restricted videos. If you set this variable, the file must exist. If you don't, `cookies.txt` in the working directory is used when present (in Docker: `/home/bot/cookies.txt`). Without a cookie file the bot still works, only without a login. Export one from a logged-in browser with an extension, or with `yt-dlp --cookies-from-browser firefox --cookies cookies.txt`. yt-dlp writes refreshed cookies back to the file, so it must be writable. Keep it private. It's a login. |
| `FFMPEG_PATH` | `ffmpeg` | ffmpeg executable. |

### Web player (optional)

The web player is turned on by `WEB_PUBLIC_URL`. When it's set, the OAuth variables are required.

| Variable | Default | Description |
|---|---|---|
| `WEB_PUBLIC_URL` | – (off) | The public URL people open, e.g. `https://music.example.com`, without a path. The OAuth redirect URI and the WebSocket origin check are derived from it. `!web` posts `<WEB_PUBLIC_URL>/g/<server id>`. |
| `WEB_BIND` | `0.0.0.0:8080` | Address and port the HTTP server listens on. |
| `FLUXER_OAUTH_CLIENT_ID` | – | Client ID of an OAuth2 application registered on your Fluxer instance. |
| `FLUXER_OAUTH_CLIENT_SECRET` | – | Its client secret. |
| `WEB_SECRET` | random per start | Key that encrypts the login cookie, at least 32 characters (e.g. `openssl rand -hex 32`). Without it, everyone is logged out whenever the bot restarts. |

Setting it up:

1. Register an OAuth2 application on your Fluxer instance. Its redirect URI must be **exactly** `<WEB_PUBLIC_URL>/auth/callback`, e.g. `https://music.example.com/auth/callback`. The bot only asks for the `identify` scope.
2. Put a reverse proxy (Caddy, nginx, Traefik …) in front of `WEB_BIND` that serves `WEB_PUBLIC_URL` over HTTPS and passes WebSocket upgrades (the player uses `/api/g/<server>/ws`).
3. Only members of a server can open that server's player. Membership is checked again every 5 minutes.

Minimal Caddy example:

```
music.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

### Jellyfin (optional)

Adds a Jellyfin tab to the web player's search window (songs, albums, playlists, artists and instant mixes). Set both of the first two variables or neither. Audio is streamed by ffmpeg straight from your Jellyfin server, so the bot must be able to reach `JELLYFIN_URL`.

| Variable | Default | Description |
|---|---|---|
| `JELLYFIN_URL` | – | Base URL of your Jellyfin server, e.g. `http://jellyfin.lan:8096`. |
| `JELLYFIN_API_KEY` | – | An API key, created in Jellyfin under *Dashboard → API Keys*. |
| `JELLYFIN_USER` | first administrator | Name of the Jellyfin user whose library and playlists are shown. |

### Logging

| Variable | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | Log filter, e.g. `debug` or `breakcore_moose=debug,info`. |

### Full example

```sh
FLUXER_TOKEN=your-bot-token
FLUXER_URL=https://chat.example.com
MUSIC_CHANNEL_ID=1234567890123456789
VOLUME=50

WEB_PUBLIC_URL=https://music.example.com
FLUXER_OAUTH_CLIENT_ID=...
FLUXER_OAUTH_CLIENT_SECRET=...
WEB_SECRET=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

JELLYFIN_URL=http://jellyfin.lan:8096
JELLYFIN_API_KEY=...
```

## Commands

With the default prefix `!`:

| Command | Description |
|---|---|
| `!play <link or search>` (`!p`) | Add a link (anything yt-dlp supports), a playlist, or the first YouTube search result. |
| `!play <playlist link> 10` | Add the playlist without its first 10 tracks. Works with `!next` too. |
| `!next <link or search>` (`!n`) | Add to the priority queue, which plays before the regular queue. |
| `!skip` (`!s`) | Play the next track. |
| `!seek <time>` | Jump to a time (`1:30`, `90`, `1:02:03`) or move by `+30` / `-15`. |
| `!queue` (`!q`) | Show what's playing and coming up. |
| `!shuffle` | Shuffle the queue (priority tracks stay first). |
| `!clear` | Remove all upcoming tracks, priority ones too. |
| `!pause` / `!resume` | Pause or continue the current track. |
| `!volume <0-100>` (`!vol`, `!v`) | Set the volume. 100 % is the configured `VOLUME`. Without a number it shows the current volume. |
| `!loop off\|track\|queue` | Repeat nothing, the current track, or the whole queue. |
| `!join [channel]` (`!j`) | Join your voice channel, or the one named. |
| `!leave` (`!stop`) | Stop, clear the queue and leave voice. |
| `!web` | Link to the web player. |
| `!help` | List the commands. |

If the bot says you aren't in a voice channel even though you are, rejoin the channel or use `!join <channel name>`. The bot only sees users who joined voice after it started.

## Build and run on bare metal

### Runtime dependencies

The bot calls these as external programs, so they must be on `PATH`, or be set with `YTDLP_PATH` / `FFMPEG_PATH`:

- [**yt-dlp**](https://github.com/yt-dlp/yt-dlp): use a current release, not a distro package that may be months old. The standalone binary is easiest:
  ```sh
  sudo curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux -o /usr/local/bin/yt-dlp
  sudo chmod 755 /usr/local/bin/yt-dlp
  ```
  Keep it current with `yt-dlp -U` (e.g. from a daily cron job).
- [**Deno**](https://deno.com): yt-dlp needs a JavaScript runtime for YouTube. `curl -fsSL https://deno.land/install.sh | sh`, or your package manager.
- **ffmpeg**: any recent version, e.g. `sudo apt install ffmpeg`.

### Building

You need a stable Rust toolchain with edition 2024 support (Rust 1.85 or newer; install it with [rustup](https://rustup.rs)), plus the build dependencies of LiveKit's native WebRTC library. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libglib2.0-dev libva-dev
```

The bot links a prebuilt glibc WebRTC library, so build on a glibc-based distribution, not musl/Alpine. Then build:

```sh
git clone https://github.com/Seidenschnabel2k/BreakcoreMoose.git
cd BreakcoreMoose
cargo build --release --locked
```

`--locked` matters. The LiveKit crates are pinned to versions that build together, and newer patch releases don't. The first build takes a while because it fetches and links WebRTC. The web player's page is embedded in the binary, so the result is a single file, `target/release/breakcore-moose`.

### Running

```sh
cp .env.example .env    # fill it in
./target/release/breakcore-moose
```

The bot reads `.env` and `cookies.txt` from its **working directory**.

To keep it running, use a systemd unit like this one, e.g. `/etc/systemd/system/breakcore-moose.service`:

```ini
[Unit]
Description=BreakcoreMoose music bot
After=network-online.target
Wants=network-online.target

[Service]
User=breakcore
WorkingDirectory=/opt/breakcore-moose
ExecStart=/opt/breakcore-moose/breakcore-moose
Restart=on-failure
RestartSec=5
# Deno installed per user lives in ~/.deno/bin
Environment=PATH=/home/breakcore/.deno/bin:/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --create-home breakcore
sudo mkdir -p /opt/breakcore-moose
sudo cp target/release/breakcore-moose .env /opt/breakcore-moose/
sudo chown -R breakcore /opt/breakcore-moose
sudo systemctl daemon-reload
sudo systemctl enable --now breakcore-moose
journalctl -u breakcore-moose -f
```

### Building the Docker image yourself

```sh
docker build --pull -t breakcore-moose .
docker run -d --restart unless-stopped --env-file .env breakcore-moose
```

`--pull` refreshes the base images, so you get the latest yt-dlp and Deno.

### Development

```sh
cargo test                  # unit tests, offline
cargo test -- --ignored     # live yt-dlp/ffmpeg and YouTube Music tests, need network
cargo clippy --all-targets
```

## License

[GPL-2.0](LICENSE)
