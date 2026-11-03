# syntax=docker/dockerfile:1.7

# ---- Build ------------------------------------------------------------------
# Debian, not Alpine: livekit links a prebuilt native WebRTC library (glibc).
FROM rust:1.99-slim-trixie AS build

# Build dependencies of livekit/libwebrtc (see the fluxer-rust README).
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libglib2.0-dev libva-dev g++ \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The web player's page, embedded into the binary.
COPY web ./web

# --locked keeps the pinned livekit crate set from Cargo.lock. The cache mounts
# keep crates and the WebRTC build between image builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/breakcore-moose /breakcore-moose

# ---- Runtime ----------------------------------------------------------------
FROM debian:trixie-slim

# Root certificates for TLS to Fluxer, LiveKit and YouTube.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Static ffmpeg, far smaller than Debian's ffmpeg and its libraries.
COPY --from=mwader/static-ffmpeg:8.1.2 /ffmpeg /usr/local/bin/ffmpeg
# Deno and yt-dlp track their latest releases, because YouTube breaks old
# yt-dlp versions often. Rebuild with `docker build --pull` to pick up new ones.
# yt-dlp needs a JavaScript runtime for YouTube.
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno
# Standalone yt-dlp (bundles its own Python).
ADD --chmod=755 \
    https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
    /usr/local/bin/yt-dlp

COPY --from=build /breakcore-moose /usr/local/bin/breakcore-moose

RUN useradd --system --create-home --uid 10001 bot
USER bot
WORKDIR /home/bot

# Configure with environment variables, e.g. `docker run --env-file .env ...`.
# yt-dlp cookies are never baked into the image: mount them at run time with
# `-v "$PWD/cookies.txt:/home/bot/cookies.txt"` (picked up from the working
# directory). Without the mount the bot runs without cookies.
# The web player (only when WEB_PUBLIC_URL is set).
EXPOSE 8080
ENTRYPOINT ["breakcore-moose"]
