//! Shared domain model, CRDT primitives and wire protocol for Weavedraw.
//!
//! This crate is deliberately free of any GUI or networking dependencies so
//! that both the headless server and the `egui` client (and tests) can use it.
//!
//! Layout:
//! - [`types`]    — plain data: [`Point`], [`Rgba`], [`Stroke`], [`CursorState`].
//! - [`crdt`]     — [`LamportClock`], [`Timestamp`], and the LWW [`StrokeSet`].
//! - [`protocol`] — [`ClientMessage`] / [`ServerMessage`] exchanged over WebSocket.
//! - [`codec`]    — `bincode` (default) or JSON (`json-wire` feature) framing.

pub mod codec;
pub mod crdt;
pub mod protocol;
pub mod types;

pub use codec::{CodecError, decode, decode_client, decode_server, encode};
pub use crdt::{LamportClock, StrokeEntry, StrokeOp, StrokeSet, Timestamp};
pub use protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, StrokeDelta};
pub use types::{ClientId, CursorState, Point, Rgba, RoomId, Stroke, StrokeId};
