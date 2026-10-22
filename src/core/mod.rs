//! Per-server players. Every queue change goes through one task per server,
//! so chat (and later the dashboard) can never interleave edits.

mod player;
mod queue;

pub use player::{Enqueued, JoinError, PlayerMsg, SeekTarget, Snapshot, describe};
#[cfg(test)]
pub use player::NowPlaying;

use std::collections::HashMap;
use std::sync::Arc;

use fluxer::http::Http;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;

#[derive(Clone)]
pub struct Players {
    config: Arc<Config>,
    http: Arc<Http>,
    players: Arc<Mutex<HashMap<String, mpsc::Sender<PlayerMsg>>>>,
}

impl Players {
    pub fn new(config: Arc<Config>, http: Arc<Http>) -> Self {
        Self { config, http, players: Default::default() }
    }

    /// The sender for this server's player, spawning the task on first use.
    pub async fn get(&self, guild_id: &str) -> mpsc::Sender<PlayerMsg> {
        let mut players = self.players.lock().await;
        if let Some(tx) = players.get(guild_id).filter(|tx| !tx.is_closed()) {
            return tx.clone();
        }
        let tx = player::spawn(guild_id.to_string(), self.config.clone(), self.http.clone());
        players.insert(guild_id.to_string(), tx.clone());
        tx
    }

    /// The sender only if a player already exists.
    pub async fn existing(&self, guild_id: &str) -> Option<mpsc::Sender<PlayerMsg>> {
        self.players.lock().await.get(guild_id).cloned()
    }
}
