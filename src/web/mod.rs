//! The web player: an axum server in the same process as the bot, so the page
//! and chat commands talk to the same per-server player. People log in with
//! Fluxer OAuth2 and may only open servers they are a member of.

mod api;
mod auth;
mod ws;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use axum::Router;
use axum::extract::FromRef;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum_extra::extract::cookie::Key;
use fluxer::http::Http;
use rand::RngExt;
use sha2::{Digest, Sha512};

use crate::bot::Gateway;
use crate::config::{Config, WebConfig};
use crate::core::Players;
use crate::media::Track;
use crate::media::jellyfin::Jellyfin;

/// How long a membership check is trusted.
const MEMBER_TTL: Duration = Duration::from_secs(5 * 60);
/// Search results stay addable this long.
const HIT_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_HITS: usize = 20_000;
/// Per user: burst size and refill per second of the action budget.
const RATE_BURST: f64 = 40.0;
const RATE_PER_SEC: f64 = 4.0;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    config: Arc<Config>,
    web: WebConfig,
    players: Players,
    gateway: Arc<Gateway>,
    bot_http: Arc<Http>,
    jellyfin: Option<Arc<Jellyfin>>,
    http: reqwest::Client,
    key: Key,
    /// (guild, user) → (checked at, is member).
    members: Mutex<HashMap<(String, String), (Instant, bool)>>,
    /// Search results by opaque token, so the page adds tracks the server
    /// found itself instead of sending arbitrary URLs.
    hits: Mutex<HashMap<String, (Instant, Track)>>,
    /// user → (tokens left, last refill).
    rate: Mutex<HashMap<String, (f64, Instant)>>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.key.clone()
    }
}

pub fn spawn(
    config: Arc<Config>,
    players: Players,
    gateway: Arc<Gateway>,
    bot_http: Arc<Http>,
    jellyfin: Option<Arc<Jellyfin>>,
) -> Result<()> {
    let Some(web) = config.web.clone() else {
        tracing::info!("web player off (set WEB_PUBLIC_URL to enable it)");
        return Ok(());
    };
    let key = match &web.secret {
        Some(secret) => Key::from(Sha512::digest(secret.as_bytes()).as_slice()),
        None => {
            tracing::warn!("WEB_SECRET is not set; web logins end when the bot restarts");
            Key::generate()
        }
    };
    let state = AppState(Arc::new(Inner {
        config,
        web: web.clone(),
        players,
        gateway,
        bot_http,
        jellyfin,
        http: reqwest::Client::new(),
        key,
        members: Default::default(),
        hits: Default::default(),
        rate: Default::default(),
    }));

    let app = Router::new()
        .route("/", get(index))
        .route("/g/{guild}", get(index))
        .route(
            "/assets/app.js",
            get(|| asset("text/javascript", include_str!("../../web/app.js"))),
        )
        .route(
            "/assets/style.css",
            get(|| asset("text/css", include_str!("../../web/style.css"))),
        )
        .route(
            "/assets/icon.svg",
            get(|| asset("image/svg+xml", include_str!("../../web/icon.svg"))),
        )
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/api/me", get(api::me))
        .route("/api/g/{guild}/search/{source}", get(api::search))
        .route("/api/g/{guild}/jellyfin/{id}", get(api::jellyfin_tracks))
        .route("/api/g/{guild}/ws", get(ws::upgrade))
        .route("/img/jellyfin/{id}", get(api::jellyfin_image))
        .with_state(state);

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(&web.bind)
            .await
            .with_context(|| format!("binding {}", web.bind))
        {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("web player: {e:#}");
                return;
            }
        };
        tracing::info!(bind = %web.bind, url = %web.public_url, "web player listening");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("web player stopped: {e}");
        }
    });
    Ok(())
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../../web/index.html"),
    )
}

async fn asset(kind: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}

impl Inner {
    /// Whether the user is in this server, as the bot sees it. Cached for a few minutes.
    pub async fn is_member(&self, guild_id: &str, user_id: &str) -> bool {
        let key = (guild_id.to_string(), user_id.to_string());
        if let Some((at, ok)) = self.members.lock().unwrap().get(&key)
            && at.elapsed() < MEMBER_TTL
        {
            return *ok;
        }
        let ok = match self.bot_http.get_guild_member(guild_id, user_id).await {
            Ok(_) => true,
            Err(fluxer::error::ClientError::Api(e))
                if e.contains("HTTP 404") || e.contains("HTTP 403") =>
            {
                false
            }
            Err(e) => {
                // Don't cache a network hiccup as "not a member".
                tracing::warn!(guild = guild_id, "membership check failed: {e}");
                return false;
            }
        };
        self.members
            .lock()
            .unwrap()
            .insert(key, (Instant::now(), ok));
        ok
    }

    /// Servers the bot is in, by id and name.
    pub async fn bot_guilds(&self) -> Vec<(String, String)> {
        let Some(ctx) = self.gateway.context().await else {
            return Vec::new();
        };
        let guilds = ctx.cache.guilds.read().await;
        let mut list: Vec<(String, String)> = guilds
            .values()
            .map(|g| (g.id.clone(), g.name.clone().unwrap_or_else(|| g.id.clone())))
            .collect();
        list.sort_by_key(|(_, name)| name.to_lowercase());
        list
    }

    /// Spends `cost` from the user's action budget; false when it's empty.
    pub fn allow(&self, user_id: &str, cost: f64) -> bool {
        let mut rate = self.rate.lock().unwrap();
        let now = Instant::now();
        let (tokens, last) = rate.entry(user_id.to_string()).or_insert((RATE_BURST, now));
        *tokens = (*tokens + last.elapsed().as_secs_f64() * RATE_PER_SEC).min(RATE_BURST);
        *last = now;
        if *tokens < cost {
            return false;
        }
        *tokens -= cost;
        true
    }

    /// Stores a search result and returns the token the page adds it by.
    pub fn remember_hit(&self, track: Track) -> String {
        let token = random_token();
        let mut hits = self.hits.lock().unwrap();
        if hits.len() >= MAX_HITS {
            hits.retain(|_, (at, _)| at.elapsed() < HIT_TTL);
            if hits.len() >= MAX_HITS {
                hits.clear();
            }
        }
        hits.insert(token.clone(), (Instant::now(), track));
        token
    }

    pub fn hit(&self, token: &str) -> Option<Track> {
        let hits = self.hits.lock().unwrap();
        hits.get(token)
            .filter(|(at, _)| at.elapsed() < HIT_TTL)
            .map(|(_, t)| t.clone())
    }
}

/// 128 random bits as hex.
pub fn random_token() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
