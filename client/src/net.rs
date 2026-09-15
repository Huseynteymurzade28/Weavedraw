//! Network thread: owns the WebSocket, reconnects with backoff, and bridges
//! the async world to the synchronous `egui` update loop via channels.
//!
//! UI → net: [`NetHandle::send`] (never blocks). Replicated [`StrokeOp`]s are
//! queued while offline and flushed after the next successful handshake;
//! ephemeral traffic is simply dropped.
//!
//! net → UI: [`NetEvent`]s, drained each frame with [`NetHandle::poll`].
//! Every event also pokes `egui::Context::request_repaint` so the UI wakes
//! up without polling.

use std::time::{Duration, Instant};

use common::{ClientId, ClientMessage, PROTOCOL_VERSION, Rgba, ServerMessage, StrokeOp, codec};
use crossbeam_channel::{Receiver, Sender};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Everything needed to (re)establish a session.
#[derive(Debug, Clone)]
pub struct NetConfig {
    pub url: String,
    pub room: String,
    pub name: String,
    pub color: Rgba,
    pub client_id: ClientId,
}

/// Connection lifecycle and traffic, as seen by the UI.
#[derive(Debug)]
pub enum NetEvent {
    Connecting {
        attempt: u32,
    },
    /// Socket is up and `Hello` was sent; the `Welcome` follows as a `Message`.
    Connected,
    Message(ServerMessage),
    /// Round-trip time measured by the keepalive ping.
    Rtt(Duration),
    Disconnected {
        reason: String,
    },
}

pub struct NetHandle {
    to_net: mpsc::UnboundedSender<ClientMessage>,
    from_net: Receiver<NetEvent>,
}

impl NetHandle {
    /// Queue a message for the network thread. Never blocks.
    pub fn send(&self, msg: ClientMessage) {
        // A closed channel means the net thread died; the UI will show the
        // last `Disconnected` reason, so there is nothing more to do here.
        let _ = self.to_net.send(msg);
    }

    /// Drain everything that arrived since the last frame.
    pub fn poll(&self) -> impl Iterator<Item = NetEvent> + '_ {
        self.from_net.try_iter()
    }
}

const PING_INTERVAL: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Start the network thread. It runs for the lifetime of the process.
pub fn spawn(config: NetConfig, ctx: egui::Context) -> NetHandle {
    let (to_net, from_ui) = mpsc::unbounded_channel();
    let (to_ui, from_net) = crossbeam_channel::unbounded();

    std::thread::Builder::new()
        .name("weavedraw-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run(config, from_ui, Emitter { tx: to_ui, ctx }));
        })
        .expect("spawn network thread");

    NetHandle { to_net, from_net }
}

/// Sends events to the UI and wakes it up.
struct Emitter {
    tx: Sender<NetEvent>,
    ctx: egui::Context,
}

impl Emitter {
    fn emit(&self, ev: NetEvent) {
        if self.tx.send(ev).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

async fn run(config: NetConfig, mut from_ui: mpsc::UnboundedReceiver<ClientMessage>, ui: Emitter) {
    let mut attempt = 0u32;
    // Ops produced while offline; replayed after the next handshake.
    let mut outbox: Vec<StrokeOp> = Vec::new();

    loop {
        attempt += 1;
        ui.emit(NetEvent::Connecting { attempt });

        let reason = match tokio_tungstenite::connect_async(&config.url).await {
            Ok((ws, _)) => {
                info!(url = %config.url, room = %config.room, "connected");
                attempt = 0;
                match session(ws, &config, &mut from_ui, &mut outbox, &ui).await {
                    Ok(()) => "server closed the connection".to_owned(),
                    Err(e) => format!("{e:#}"),
                }
            }
            Err(e) => format!("connect failed: {e}"),
        };

        if from_ui.is_closed() {
            return; // UI is gone: exit quietly.
        }
        warn!("disconnected: {reason}");
        ui.emit(NetEvent::Disconnected { reason });

        let backoff = Duration::from_millis(500 * 2u64.pow(attempt.min(5))).min(MAX_BACKOFF);
        // Keep queueing ops (and dropping ephemera) while we wait.
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                msg = from_ui.recv() => match msg {
                    Some(m) => stash_offline(m, &mut outbox),
                    None => return,
                },
            }
        }
    }
}

fn stash_offline(msg: ClientMessage, outbox: &mut Vec<StrokeOp>) {
    if let ClientMessage::Ops(ops) = msg {
        outbox.extend(ops);
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn frame(msg: &ClientMessage) -> anyhow::Result<Message> {
    let bytes = codec::encode(msg)?;
    Ok(if codec::is_text_wire() {
        Message::Text(String::from_utf8(bytes)?.into())
    } else {
        Message::Binary(bytes.into())
    })
}

/// One connected session; returns when the socket closes or errors.
async fn session(
    mut ws: Ws,
    config: &NetConfig,
    from_ui: &mut mpsc::UnboundedReceiver<ClientMessage>,
    outbox: &mut Vec<StrokeOp>,
    ui: &Emitter,
) -> anyhow::Result<()> {
    ws.send(frame(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client_id: config.client_id,
        room: config.room.clone(),
        name: config.name.clone(),
        color: config.color,
    })?)
    .await?;
    ui.emit(NetEvent::Connected);

    if !outbox.is_empty() {
        debug!(ops = outbox.len(), "flushing offline outbox");
        ws.send(frame(&ClientMessage::Ops(std::mem::take(outbox)))?)
            .await?;
    }

    let started = Instant::now();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // first tick fires immediately; skip it
    let mut in_flight_ping: Option<(u64, Instant)> = None;

    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(frame) = incoming else { return Ok(()) };
                let bytes = match frame? {
                    Message::Binary(b) => b.to_vec(),
                    Message::Text(t) => t.as_bytes().to_vec(),
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                };
                match codec::decode_server(&bytes) {
                    Ok(ServerMessage::Pong(n)) => {
                        if let Some((sent, at)) = in_flight_ping.take()
                            && sent == n
                        {
                            ui.emit(NetEvent::Rtt(at.elapsed()));
                        }
                    }
                    Ok(msg) => ui.emit(NetEvent::Message(msg)),
                    Err(e) => warn!("dropping undecodable frame: {e}"),
                }
            }
            outgoing = from_ui.recv() => {
                let Some(msg) = outgoing else { return Ok(()) };
                ws.send(frame(&msg)?).await?;
            }
            _ = ping.tick() => {
                let n = started.elapsed().as_nanos() as u64;
                in_flight_ping = Some((n, Instant::now()));
                ws.send(frame(&ClientMessage::Ping(n))?).await?;
            }
        }
    }
}
