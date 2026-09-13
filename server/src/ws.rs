//! WebSocket session: handshake, message dispatch, and broadcast relay.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use common::protocol::Rejection;
use common::{ClientId, ClientMessage, CursorState, PROTOCOL_VERSION, ServerMessage, codec};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::AppState;
use crate::registry::Registry;
use crate::room::{Room, encode_frame};

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = session(socket, state, addr).await {
            debug!(%addr, "session ended with error: {e:#}");
        }
    })
}

/// A live, joined connection.
struct Peer {
    id: ClientId,
    room: Arc<Room>,
    sink: Sink,
}

impl Peer {
    async fn send(&mut self, msg: &ServerMessage) -> anyhow::Result<()> {
        self.sink.send(encode_frame(msg)?).await?;
        Ok(())
    }
}

async fn session(socket: WebSocket, state: AppState, addr: SocketAddr) -> anyhow::Result<()> {
    let (mut sink, mut stream) = socket.split();
    let mut shutdown = state.shutdown.clone();

    // ---- handshake -------------------------------------------------------
    let hello = tokio::time::timeout(
        state.registry.config().hello_timeout,
        next_message(&mut stream),
    );
    let (client_id, room_id, name, color) = match hello.await {
        Ok(Some(ClientMessage::Hello {
            protocol,
            client_id,
            room,
            name,
            color,
        })) => {
            if protocol != PROTOCOL_VERSION {
                return reject(
                    sink,
                    Rejection::UnsupportedProtocol {
                        server: PROTOCOL_VERSION,
                        client: protocol,
                    },
                )
                .await;
            }
            if !Registry::is_valid_room_id(&room) {
                return reject(sink, Rejection::InvalidRoom).await;
            }
            (client_id, room, name, color)
        }
        Ok(Some(_)) => return reject(sink, Rejection::NotJoined).await,
        Ok(None) => return Ok(()),
        Err(_) => {
            debug!(%addr, "no Hello within timeout");
            return Ok(());
        }
    };

    let room = state.registry.get_or_create(&room_id).await?;
    // Subscribe *before* snapshotting so nothing slips between the two.
    // Anything in both is harmless: CRDT ops are idempotent.
    let mut rx = room.subscribe();
    let mut cursor = CursorState::new(client_id, name.chars().take(32).collect::<String>(), color);
    if let Err(r) = room.join(cursor.clone()) {
        return reject(sink, r).await;
    }

    let welcome = ServerMessage::Welcome {
        client_id,
        room: room_id.clone(),
        snapshot: room.snapshot(),
        peers: room.peers_except(client_id),
    };
    sink.send(encode_frame(&welcome)?).await?;
    room.broadcast(Some(client_id), &ServerMessage::PeerJoined(cursor.clone()));

    let mut peer = Peer {
        id: client_id,
        room: room.clone(),
        sink,
    };

    // ---- main loop -------------------------------------------------------
    let result: anyhow::Result<()> = async {
        loop {
            tokio::select! {
                incoming = stream.next() => {
                    let Some(frame) = incoming else { break };
                    let bytes = match frame? {
                        Message::Binary(b) => b,
                        Message::Text(t) => t.into(),
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => continue,
                    };
                    match codec::decode_client(&bytes) {
                        Ok(msg) => handle_client_message(&mut peer, &mut cursor, msg).await?,
                        Err(e) => {
                            warn!(client = %client_id, "malformed frame: {e}");
                            peer.send(&ServerMessage::Rejected(Rejection::Malformed)).await?;
                        }
                    }
                }
                outgoing = rx.recv() => {
                    match outgoing {
                        Ok(out) => {
                            if out.exclude != Some(client_id) {
                                peer.sink.send(out.frame).await?;
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            // Too slow to keep up: resync from the full state.
                            warn!(client = %client_id, missed = n, "peer lagged; sending snapshot");
                            peer.send(&ServerMessage::Snapshot(peer.room.snapshot())).await?;
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
                _ = shutdown.changed() => {
                    let _ = peer.sink.send(Message::Close(None)).await;
                    break;
                }
            }
        }
        Ok(())
    }
    .await;

    // ---- teardown --------------------------------------------------------
    room.leave(client_id);
    room.broadcast(Some(client_id), &ServerMessage::PeerLeft(client_id));
    let _ = peer.sink.close().await;
    info!(client = %client_id, room = %room_id, %addr, "session closed");
    result
}

async fn handle_client_message(
    peer: &mut Peer,
    cursor: &mut CursorState,
    msg: ClientMessage,
) -> anyhow::Result<()> {
    match msg {
        ClientMessage::Ops(ops) => {
            if ops.is_empty() {
                return Ok(());
            }
            let changed = peer.room.apply_ops(&ops);
            debug!(client = %peer.id, ops = ops.len(), changed, "ops applied");
            peer.room
                .broadcast(Some(peer.id), &ServerMessage::Ops { from: peer.id, ops });
        }
        ClientMessage::Cursor(mut c) => {
            // Never trust the client to speak for someone else.
            c.client_id = peer.id;
            c.name.clone_from(&cursor.name);
            c.color = cursor.color;
            *cursor = c.clone();
            peer.room.update_cursor(c.clone());
            peer.room
                .broadcast(Some(peer.id), &ServerMessage::Cursor(c));
        }
        ClientMessage::StrokeDelta(mut d) => {
            d.client_id = peer.id;
            peer.room
                .broadcast(Some(peer.id), &ServerMessage::StrokeDelta(d));
        }
        ClientMessage::RequestSnapshot => {
            peer.send(&ServerMessage::Snapshot(peer.room.snapshot()))
                .await?;
        }
        ClientMessage::Ping(n) => peer.send(&ServerMessage::Pong(n)).await?,
        ClientMessage::Hello { .. } => {
            peer.send(&ServerMessage::Rejected(Rejection::Malformed))
                .await?;
        }
    }
    Ok(())
}

/// Read the next decodable client message, skipping control frames.
/// `None` when the socket closed.
async fn next_message(stream: &mut Stream) -> Option<ClientMessage> {
    while let Some(Ok(frame)) = stream.next().await {
        let bytes = match frame {
            Message::Binary(b) => b,
            Message::Text(t) => t.into(),
            Message::Close(_) => return None,
            _ => continue,
        };
        match codec::decode_client(&bytes) {
            Ok(msg) => return Some(msg),
            Err(e) => {
                debug!("dropping undecodable frame during handshake: {e}");
                return None;
            }
        }
    }
    None
}

async fn reject(mut sink: Sink, why: Rejection) -> anyhow::Result<()> {
    debug!("rejecting connection: {why}");
    sink.send(encode_frame(&ServerMessage::Rejected(why))?)
        .await?;
    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}
