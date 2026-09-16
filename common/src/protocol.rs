//! Messages exchanged between client and server over a single WebSocket.
//!
//! Two flavours of traffic share the socket:
//!
//! * **Replicated** — [`StrokeOp`]s. Persisted by the server, applied to the
//!   room's [`StrokeSet`], and rebroadcast. Safe to reorder or duplicate.
//! * **Ephemeral** — cursors and in-progress [`StrokeDelta`]s. Never stored;
//!   relayed as-is to every other peer in the room. Latest value wins.
//!
//! Every message is a single WebSocket frame encoded via [`crate::codec`].

use serde::{Deserialize, Serialize};

use crate::crdt::{StrokeOp, StrokeSet};
use crate::types::{ClientId, CursorState, Point, Rgba, RoomId, Stroke, StrokeId, StrokeKind};

/// Bump whenever the wire format changes incompatibly. The server rejects
/// clients announcing a different version in [`ClientMessage::Hello`].
pub const PROTOCOL_VERSION: u16 = 2;

/// A chunk of an in-progress stroke, so peers can watch it being drawn
/// before it is committed as a [`StrokeOp::Add`].
///
/// The first delta for a given `stroke_id` starts a new preview. For a
/// [`StrokeKind::Freehand`] stroke subsequent deltas *append* `points`;
/// for every other kind they *replace* the preview's points and `kind`
/// (a shape's far corner moves, a label's text changes). Receivers discard
/// the preview once the matching `Add` arrives (or when the peer leaves).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrokeDelta {
    pub stroke_id: StrokeId,
    pub client_id: ClientId,
    pub color: Rgba,
    pub width: f32,
    pub kind: StrokeKind,
    /// Freehand: points appended since the previous delta for this
    /// `stroke_id`. Other kinds: the complete point list.
    pub points: Vec<Point>,
}

impl StrokeDelta {
    /// Fold this delta into the preview it belongs to.
    pub fn apply_to(self, preview: &mut Stroke) {
        if self.kind == StrokeKind::Freehand && preview.kind == StrokeKind::Freehand {
            preview.points.extend(self.points);
        } else {
            preview.kind = self.kind;
            preview.points = self.points;
        }
    }

    /// An empty preview for the first delta of a stroke.
    pub fn start_preview(&self) -> Stroke {
        Stroke {
            id: self.stroke_id,
            client_id: self.client_id,
            color: self.color,
            width: self.width,
            kind: StrokeKind::Freehand,
            points: Vec::new(),
        }
    }
}

/// Why the server refused a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    UnsupportedProtocol { server: u16, client: u16 },
    NotJoined,
    RoomFull,
    InvalidRoom,
    Malformed,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::UnsupportedProtocol { server, client } => {
                write!(f, "protocol mismatch: server v{server}, client v{client}")
            }
            Rejection::NotJoined => f.write_str("send Hello before any other message"),
            Rejection::RoomFull => f.write_str("room is full"),
            Rejection::InvalidRoom => f.write_str("invalid room id"),
            Rejection::Malformed => f.write_str("malformed message"),
        }
    }
}

/// Client → server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Must be the first message on a fresh connection.
    Hello {
        protocol: u16,
        client_id: ClientId,
        room: RoomId,
        name: String,
        color: Rgba,
    },
    /// Replicated mutations, in the order the client produced them.
    Ops(Vec<StrokeOp>),
    /// Ephemeral cursor / presence update.
    Cursor(CursorState),
    /// Ephemeral in-progress stroke chunk.
    StrokeDelta(StrokeDelta),
    /// Ask for a full [`ServerMessage::Snapshot`] (e.g. after a suspected desync).
    RequestSnapshot,
    /// Keepalive; echoed back as [`ServerMessage::Pong`] with the same payload.
    Ping(u64),
}

impl ClientMessage {
    /// Ephemeral messages are relayed but never persisted or acknowledged.
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            ClientMessage::Cursor(_) | ClientMessage::StrokeDelta(_)
        )
    }
}

/// Server → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Reply to a successful [`ClientMessage::Hello`]: full room state plus
    /// everyone currently present.
    Welcome {
        client_id: ClientId,
        room: RoomId,
        snapshot: StrokeSet,
        peers: Vec<CursorState>,
    },
    /// Full state, sent on request. The client should `merge` it.
    Snapshot(StrokeSet),
    /// Replicated mutations from a peer (the sender's own ops are *not* echoed).
    Ops {
        from: ClientId,
        ops: Vec<StrokeOp>,
    },
    Cursor(CursorState),
    StrokeDelta(StrokeDelta),
    PeerJoined(CursorState),
    PeerLeft(ClientId),
    Rejected(Rejection),
    Pong(u64),
}

impl ServerMessage {
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            ServerMessage::Cursor(_) | ServerMessage::StrokeDelta(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn delta(kind: StrokeKind, points: Vec<Point>) -> StrokeDelta {
        StrokeDelta {
            stroke_id: Uuid::nil(),
            client_id: Uuid::nil(),
            color: Rgba::WHITE,
            width: 1.0,
            kind,
            points,
        }
    }

    #[test]
    fn freehand_deltas_append_and_shape_deltas_replace() {
        let mut preview = delta(StrokeKind::Freehand, vec![]).start_preview();
        delta(StrokeKind::Freehand, vec![Point::ZERO]).apply_to(&mut preview);
        delta(StrokeKind::Freehand, vec![Point::new(1.0, 1.0)]).apply_to(&mut preview);
        assert_eq!(preview.points.len(), 2);

        let mut preview = delta(StrokeKind::Rect, vec![]).start_preview();
        delta(StrokeKind::Rect, vec![Point::ZERO, Point::new(1.0, 1.0)]).apply_to(&mut preview);
        delta(StrokeKind::Rect, vec![Point::ZERO, Point::new(5.0, 5.0)]).apply_to(&mut preview);
        assert_eq!(preview.kind, StrokeKind::Rect);
        assert_eq!(preview.points, vec![Point::ZERO, Point::new(5.0, 5.0)]);
    }
}
