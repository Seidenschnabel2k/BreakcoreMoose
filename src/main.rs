mod bot;
mod commands;
mod config;
mod core;
mod media;
mod music_channel;
mod voice;
mod web;

use std::sync::Arc;

use anyhow::Result;
use fluxer::prelude::Client;
use tracing_subscriber::EnvFilter;

use crate::bot::Gateway;
use crate::config::Config;
use crate::core::Players;
use crate::media::jellyfin::Jellyfin;

#[tokio::main]
async fn main() -> Result<()> {
    // Real environment variables win over .env.
    let dotenv = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match dotenv {
        Ok(path) => tracing::info!("loaded {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => anyhow::bail!("reading .env: {e}"),
    }

    let config = Arc::new(Config::from_env().await?);
    tracing::info!(api = %config.api_url, prefix = %config.prefix, "starting");

    // fluxer-rust builds its Http inside the client; players need their own
    // to post messages outside of event handlers.
    let http = Arc::new(fluxer::http::Http::new(
        &config.token,
        config.api_url.clone(),
    ));
    let players = Players::new(config.clone(), http.clone());
    let gateway = Arc::new(Gateway::default());
    let public_url = config.web.as_ref().map(|w| w.public_url.clone());
    let jellyfin = config
        .jellyfin
        .clone()
        .map(|j| Arc::new(Jellyfin::new(j, public_url)));
    web::spawn(
        config.clone(),
        players.clone(),
        gateway.clone(),
        http,
        jellyfin,
    )?;

    let mut client = Client::builder(&config.token)
        .api_url(config.api_url.clone())
        .event_handler(bot::Handler::new(config.clone(), players, gateway))
        .build();
    client.start().await?;
    Ok(())
}
