//! Framing: turn messages into WebSocket payloads and back.
//!
//! Default is `bincode` (compact, fast, binary frames). Enable the
//! `json-wire` feature on *both* peers to switch to `serde_json` text frames
//! that are easy to read in Wireshark / `websocat` while debugging.

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::protocol::{ClientMessage, ServerMessage};

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("decode failed: {0}")]
    Decode(String),
    /// A text frame arrived while in binary mode, or vice versa.
    #[error("unexpected frame type (expected {expected})")]
    FrameType { expected: &'static str },
}

/// `true` when messages should be sent as WebSocket *text* frames.
pub const fn is_text_wire() -> bool {
    cfg!(feature = "json-wire")
}

/// Human-readable name of the active wire format, for logs.
pub const fn wire_format() -> &'static str {
    if is_text_wire() { "json" } else { "bincode" }
}

#[cfg(not(feature = "json-wire"))]
mod imp {
    use super::*;

    const CONFIG: bincode::config::Configuration = bincode::config::standard();

    pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
        bincode::serde::encode_to_vec(value, CONFIG).map_err(|e| CodecError::Encode(e.to_string()))
    }

    pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
        let (value, read) = bincode::serde::decode_from_slice(bytes, CONFIG)
            .map_err(|e| CodecError::Decode(e.to_string()))?;
        if read != bytes.len() {
            return Err(CodecError::Decode(format!(
                "{} trailing bytes",
                bytes.len() - read
            )));
        }
        Ok(value)
    }
}

#[cfg(feature = "json-wire")]
mod imp {
    use super::*;

    pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
        serde_json::to_vec(value).map_err(|e| CodecError::Encode(e.to_string()))
    }

    pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
        serde_json::from_slice(bytes).map_err(|e| CodecError::Decode(e.to_string()))
    }
}

/// Serialise any message (or the [`StrokeSet`](crate::StrokeSet) itself, for
/// on-disk persistence) with the active wire format.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    imp::encode(value)
}

/// Inverse of [`encode`]. Rejects trailing garbage in binary mode.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    imp::decode(bytes)
}

pub fn decode_client(bytes: &[u8]) -> Result<ClientMessage, CodecError> {
    decode(bytes)
}

pub fn decode_server(bytes: &[u8]) -> Result<ServerMessage, CodecError> {
    decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::{LamportClock, StrokeOp, StrokeSet, Timestamp};
    use crate::protocol::{PROTOCOL_VERSION, Rejection, StrokeDelta};
    use crate::types::{CursorState, Point, Rgba, Stroke};
    use uuid::Uuid;

    fn sample_stroke(client: Uuid) -> Stroke {
        Stroke::new(client, Rgba::from_hex(0xf5c2e7), 3.5).with_points([
            Point::new(0.0, 0.0),
            Point::new(10.5, -3.25),
            Point::new(42.0, 42.0),
        ])
    }

    #[test]
    fn client_messages_round_trip() {
        let me = Uuid::new_v4();
        let mut clock = LamportClock::new(me);
        let stroke = sample_stroke(me);
        let msgs = vec![
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client_id: me,
                room: "lobby".into(),
                name: "huso".into(),
                color: Rgba::from_hex(0x89b4fa),
            },
            ClientMessage::Ops(vec![
                StrokeOp::Add {
                    stroke: stroke.clone(),
                    ts: clock.tick(),
                },
                StrokeOp::Remove {
                    id: stroke.id,
                    ts: clock.tick(),
                },
            ]),
            ClientMessage::Cursor(CursorState {
                position: Some(Point::new(1.0, 2.0)),
                drawing: true,
                ..CursorState::new(me, "huso", Rgba::WHITE)
            }),
            ClientMessage::StrokeDelta(StrokeDelta {
                stroke_id: stroke.id,
                client_id: me,
                color: stroke.color,
                width: stroke.width,
                kind: stroke.kind.clone(),
                points: stroke.points.clone(),
            }),
            ClientMessage::RequestSnapshot,
            ClientMessage::Ping(u64::MAX),
        ];
        for msg in msgs {
            let bytes = encode(&msg).unwrap();
            assert_eq!(decode_client(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn server_messages_round_trip() {
        let peer = Uuid::new_v4();
        let mut clock = LamportClock::new(peer);
        let mut snapshot = StrokeSet::new();
        snapshot.apply(StrokeOp::Add {
            stroke: sample_stroke(peer),
            ts: clock.tick(),
        });
        let msgs = vec![
            ServerMessage::Welcome {
                client_id: Uuid::new_v4(),
                room: "lobby".into(),
                snapshot: snapshot.clone(),
                peers: vec![CursorState::new(peer, "peer", Rgba::BLACK)],
            },
            ServerMessage::Snapshot(snapshot),
            ServerMessage::Ops {
                from: peer,
                ops: vec![StrokeOp::Remove {
                    id: Uuid::new_v4(),
                    ts: Timestamp::new(7, peer),
                }],
            },
            ServerMessage::PeerJoined(CursorState::new(peer, "peer", Rgba::BLACK)),
            ServerMessage::PeerLeft(peer),
            ServerMessage::Rejected(Rejection::UnsupportedProtocol {
                server: 1,
                client: 0,
            }),
            ServerMessage::Pong(3),
        ];
        for msg in msgs {
            let bytes = encode(&msg).unwrap();
            assert_eq!(decode_server(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode_client(&[0xff; 16]).is_err());
        assert!(decode_server(b"").is_err());
    }

    #[cfg(not(feature = "json-wire"))]
    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode(&ClientMessage::Ping(1)).unwrap();
        bytes.push(0);
        assert!(matches!(decode_client(&bytes), Err(CodecError::Decode(_))));
    }

    #[test]
    fn cursor_update_is_small() {
        // Presence traffic is the hot path; keep an eye on its size.
        let msg = ClientMessage::Cursor(CursorState {
            position: Some(Point::new(123.4, 567.8)),
            ..CursorState::new(Uuid::new_v4(), "ab", Rgba::WHITE)
        });
        let n = encode(&msg).unwrap().len();
        let budget = if is_text_wire() { 256 } else { 48 };
        assert!(n <= budget, "cursor frame is {n} bytes");
    }
}
