//! A single whiteboard room: replicated state, presence, and fan-out.
//!
//! Locks are `std::sync` because every critical section is short and never
//! awaits. Fan-out is a `tokio::sync::broadcast` of *pre-encoded* frames so
//! each message is serialised once regardless of peer count.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use axum::extract::ws::Message;
use common::protocol::Rejection;
use common::{ClientId, CursorState, RoomId, ServerMessage, StrokeOp, StrokeSet, codec};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Frames a peer may miss before it is told to resync via a full snapshot.
const BROADCAST_CAPACITY: usize = 4096;

/// One outbound frame plus the peer that must *not* receive it (its author).
#[derive(Debug, Clone)]
pub struct Outbound {
    pub exclude: Option<ClientId>,
    pub frame: Message,
}

pub struct Room {
    pub id: RoomId,
    state: RwLock<StrokeSet>,
    peers: RwLock<HashMap<ClientId, CursorState>>,
    tx: broadcast::Sender<Outbound>,
    dirty: AtomicBool,
    path: PathBuf,
    max_peers: usize,
}

impl Room {
    pub fn new(id: RoomId, path: PathBuf, initial: StrokeSet, max_peers: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            id,
            state: RwLock::new(initial),
            peers: RwLock::new(HashMap::new()),
            tx,
            dirty: AtomicBool::new(false),
            path,
            max_peers,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Outbound> {
        self.tx.subscribe()
    }

    /// Clone of the full replicated state (for `Welcome` / `Snapshot`).
    pub fn snapshot(&self) -> StrokeSet {
        self.state.read().unwrap().clone()
    }

    /// Apply replicated ops; returns how many changed visible state.
    pub fn apply_ops(&self, ops: &[StrokeOp]) -> usize {
        let changed = self.state.write().unwrap().apply_all(ops.iter().cloned());
        if changed > 0 {
            self.dirty.store(true, Ordering::Release);
        }
        changed
    }

    pub fn stroke_count(&self) -> usize {
        self.state.read().unwrap().len()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.read().unwrap().len()
    }

    /// Everyone in the room except `me`, with their last known cursor.
    pub fn peers_except(&self, me: ClientId) -> Vec<CursorState> {
        self.peers
            .read()
            .unwrap()
            .values()
            .filter(|c| c.client_id != me)
            .cloned()
            .collect()
    }

    /// Register a peer. Fails when the room is at capacity.
    pub fn join(&self, cursor: CursorState) -> Result<(), Rejection> {
        let mut peers = self.peers.write().unwrap();
        if peers.len() >= self.max_peers && !peers.contains_key(&cursor.client_id) {
            return Err(Rejection::RoomFull);
        }
        info!(room = %self.id, client = %cursor.client_id, name = %cursor.name, "peer joined");
        peers.insert(cursor.client_id, cursor);
        Ok(())
    }

    pub fn leave(&self, id: ClientId) -> bool {
        let removed = self.peers.write().unwrap().remove(&id).is_some();
        if removed {
            info!(room = %self.id, client = %id, "peer left");
        }
        removed
    }

    /// Remember the latest cursor so late joiners see it immediately.
    pub fn update_cursor(&self, cursor: CursorState) {
        if let Some(existing) = self.peers.write().unwrap().get_mut(&cursor.client_id) {
            *existing = cursor;
        }
    }

    /// Encode once and fan out to every subscriber except `exclude`.
    pub fn broadcast(&self, exclude: Option<ClientId>, msg: &ServerMessage) {
        match encode_frame(msg) {
            Ok(frame) => {
                // Err only means "no subscribers", which is fine.
                let _ = self.tx.send(Outbound { exclude, frame });
            }
            Err(e) => warn!(room = %self.id, "failed to encode broadcast: {e}"),
        }
    }

    /// Persist to disk if anything changed since the last save.
    /// Returns `Ok(true)` when a write actually happened.
    pub async fn save_if_dirty(&self) -> anyhow::Result<bool> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        let bytes = codec::encode(&self.snapshot())?;
        if let Err(e) = write_atomic(&self.path, &bytes).await {
            // Try again on the next flush.
            self.dirty.store(true, Ordering::Release);
            return Err(e);
        }
        debug!(room = %self.id, bytes = bytes.len(), "snapshot saved");
        Ok(true)
    }
}

/// Wrap a message in the WebSocket frame type matching the wire format.
pub fn encode_frame(msg: &ServerMessage) -> Result<Message, codec::CodecError> {
    let bytes = codec::encode(msg)?;
    Ok(if codec::is_text_wire() {
        let text = String::from_utf8(bytes)
            .map_err(|e| codec::CodecError::Encode(format!("json is not utf-8: {e}")))?;
        Message::Text(text.into())
    } else {
        Message::Binary(bytes.into())
    })
}

/// Write via a temp file + rename so a crash never leaves a torn snapshot.
async fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
