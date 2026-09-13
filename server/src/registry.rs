//! In-memory registry of rooms, backed by per-room snapshot files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use common::{RoomId, StrokeSet, codec};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::Config;
use crate::room::Room;

pub struct Registry {
    config: Config,
    /// Async mutex so the disk load inside `get_or_create` can be held across
    /// an await without two joiners racing to create the same room.
    rooms: Mutex<HashMap<RoomId, Arc<Room>>>,
}

/// Summary exposed on `GET /rooms`.
#[derive(Debug, Clone, Serialize)]
pub struct RoomInfo {
    pub id: RoomId,
    pub peers: usize,
    pub strokes: usize,
}

impl Registry {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            rooms: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Room ids are URL/file-safe slugs: `[A-Za-z0-9_-]{1,64}`.
    pub fn is_valid_room_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    fn snapshot_path(&self, id: &str) -> PathBuf {
        self.config
            .data_dir
            .join(format!("{id}.{}", codec::wire_format()))
    }

    /// Fetch a room, loading it from disk or creating it empty on first use.
    pub async fn get_or_create(&self, id: &str) -> anyhow::Result<Arc<Room>> {
        let mut rooms = self.rooms.lock().await;
        if let Some(room) = rooms.get(id) {
            return Ok(room.clone());
        }
        let path = self.snapshot_path(id);
        let initial = load_snapshot(&path).await?;
        info!(
            room = id,
            strokes = initial.len(),
            tracked = initial.tracked_len(),
            "room opened"
        );
        let room = Room::new(id.to_owned(), path, initial, self.config.max_peers);
        rooms.insert(id.to_owned(), room.clone());
        Ok(room)
    }

    pub async fn list(&self) -> Vec<RoomInfo> {
        let rooms = self.rooms.lock().await;
        let mut infos: Vec<_> = rooms
            .values()
            .map(|r| RoomInfo {
                id: r.id.clone(),
                peers: r.peer_count(),
                strokes: r.stroke_count(),
            })
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    /// Persist every dirty room. Returns how many were written.
    pub async fn flush_all(&self) -> usize {
        let rooms: Vec<Arc<Room>> = self.rooms.lock().await.values().cloned().collect();
        let mut written = 0;
        for room in rooms {
            match room.save_if_dirty().await {
                Ok(true) => written += 1,
                Ok(false) => {}
                Err(e) => warn!(room = %room.id, "failed to save snapshot: {e:#}"),
            }
        }
        written
    }
}

async fn load_snapshot(path: &PathBuf) -> anyhow::Result<StrokeSet> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            codec::decode(&bytes).with_context(|| format!("corrupt snapshot at {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StrokeSet::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
