//! End-to-end tests: boot the real axum server on an ephemeral port and talk
//! to it with `tokio-tungstenite` clients.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::protocol::Rejection;
use common::{
    ClientId, ClientMessage, CursorState, LamportClock, PROTOCOL_VERSION, Point, Rgba,
    ServerMessage, Stroke, StrokeOp, codec,
};
use futures_util::{SinkExt, StreamExt};
use server::{Config, Registry, app, flush_loop};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(3);

struct TestServer {
    addr: SocketAddr,
    registry: Arc<Registry>,
    shutdown: watch::Sender<bool>,
    handle: JoinHandle<()>,
    flusher: JoinHandle<()>,
}

impl TestServer {
    async fn start(data_dir: PathBuf) -> Self {
        let config = Config {
            addr: "127.0.0.1:0".parse().unwrap(),
            data_dir,
            max_peers: 2,
            flush_interval: Duration::from_millis(50),
            hello_timeout: Duration::from_millis(500),
        };
        let registry = Arc::new(Registry::new(config));
        let (shutdown, rx) = watch::channel(false);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let flusher = tokio::spawn(flush_loop(registry.clone(), rx.clone()));
        let router = app(registry.clone(), rx.clone());
        let handle = tokio::spawn(async move {
            let mut rx = rx;
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.changed().await;
            })
            .await
            .unwrap();
        });
        Self {
            addr,
            registry,
            shutdown,
            handle,
            flusher,
        }
    }

    async fn stop(self) {
        self.shutdown.send(true).unwrap();
        let _ = self.flusher.await;
        let _ = tokio::time::timeout(TIMEOUT, self.handle).await;
        self.registry.flush_all().await;
    }
}

struct Client {
    id: ClientId,
    clock: LamportClock,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .expect("connect");
        let id = Uuid::new_v4();
        Self {
            id,
            clock: LamportClock::new(id),
            ws,
        }
    }

    async fn send(&mut self, msg: &ClientMessage) {
        let bytes = codec::encode(msg).unwrap();
        let frame = if codec::is_text_wire() {
            Message::Text(String::from_utf8(bytes).unwrap().into())
        } else {
            Message::Binary(bytes.into())
        };
        self.ws.send(frame).await.expect("send");
    }

    /// Next decoded message, panicking on timeout or close.
    async fn recv(&mut self) -> ServerMessage {
        self.try_recv().await.expect("connection closed")
    }

    async fn try_recv(&mut self) -> Option<ServerMessage> {
        loop {
            let frame = tokio::time::timeout(TIMEOUT, self.ws.next())
                .await
                .expect("timed out waiting for server message")?
                .ok()?;
            let bytes = match frame {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => return None,
                _ => continue,
            };
            return Some(codec::decode_server(&bytes).expect("decode"));
        }
    }

    /// Full handshake; returns the `Welcome`.
    async fn join(addr: SocketAddr, room: &str, name: &str) -> (Self, ServerMessage) {
        let mut c = Self::connect(addr).await;
        c.send(&ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client_id: c.id,
            room: room.into(),
            name: name.into(),
            color: Rgba::from_hex(0x89b4fa),
        })
        .await;
        let welcome = c.recv().await;
        assert!(
            matches!(welcome, ServerMessage::Welcome { .. }),
            "got {welcome:?}"
        );
        (c, welcome)
    }

    fn add_op(&mut self) -> StrokeOp {
        let stroke = Stroke::new(self.id, Rgba::WHITE, 2.0)
            .with_points([Point::new(0.0, 0.0), Point::new(5.0, 5.0)]);
        StrokeOp::Add {
            stroke,
            ts: self.clock.tick(),
        }
    }

    /// Round-trip a ping to prove no other message is queued ahead of it.
    async fn assert_quiet(&mut self) {
        self.send(&ClientMessage::Ping(42)).await;
        assert_eq!(self.recv().await, ServerMessage::Pong(42));
    }
}

#[tokio::test]
async fn unreadable_snapshot_is_moved_aside_and_room_opens_empty() {
    let dir = temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("broken.{}", codec::wire_format()));
    std::fs::write(&file, b"definitely not a snapshot").unwrap();

    let server = TestServer::start(dir.clone()).await;
    let (_a, welcome) = Client::join(server.addr, "broken", "a").await;
    match welcome {
        ServerMessage::Welcome { snapshot, .. } => assert!(snapshot.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
    server.stop().await;

    assert!(!file.exists(), "broken snapshot still in place");
    let aside = dir.join(format!("broken.{}.corrupt", codec::wire_format()));
    assert_eq!(std::fs::read(&aside).unwrap(), b"definitely not a snapshot");

    let _ = std::fs::remove_dir_all(dir);
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("weavedraw-test-{}", Uuid::new_v4()))
}

#[tokio::test]
async fn handshake_welcome_and_peer_join_notifications() {
    let server = TestServer::start(temp_dir()).await;

    let (mut a, welcome_a) = Client::join(server.addr, "lobby", "alice").await;
    match welcome_a {
        ServerMessage::Welcome {
            client_id,
            room,
            snapshot,
            peers,
        } => {
            assert_eq!(client_id, a.id);
            assert_eq!(room, "lobby");
            assert!(snapshot.is_empty());
            assert!(peers.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }

    let (b, welcome_b) = Client::join(server.addr, "lobby", "bob").await;
    match welcome_b {
        ServerMessage::Welcome { peers, .. } => {
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].client_id, a.id);
            assert_eq!(peers[0].name, "alice");
        }
        other => panic!("unexpected {other:?}"),
    }

    match a.recv().await {
        ServerMessage::PeerJoined(c) => {
            assert_eq!(c.client_id, b.id);
            assert_eq!(c.name, "bob");
        }
        other => panic!("unexpected {other:?}"),
    }

    let rooms = server.registry.list().await;
    assert_eq!(rooms.len(), 1);
    assert_eq!((rooms[0].id.as_str(), rooms[0].peers), ("lobby", 2));

    server.stop().await;
}

#[tokio::test]
async fn ops_are_broadcast_to_peers_but_not_echoed() {
    let server = TestServer::start(temp_dir()).await;
    let (mut a, _) = Client::join(server.addr, "r", "a").await;
    let (mut b, _) = Client::join(server.addr, "r", "b").await;
    let _ = a.recv().await; // PeerJoined(b)

    let op = a.add_op();
    a.send(&ClientMessage::Ops(vec![op.clone()])).await;

    match b.recv().await {
        ServerMessage::Ops { from, ops } => {
            assert_eq!(from, a.id);
            assert_eq!(ops, vec![op]);
        }
        other => panic!("unexpected {other:?}"),
    }
    a.assert_quiet().await;

    server.stop().await;
}

#[tokio::test]
async fn late_joiner_receives_snapshot_and_lagging_is_recoverable() {
    let server = TestServer::start(temp_dir()).await;
    let (mut a, _) = Client::join(server.addr, "r", "a").await;

    let add = a.add_op();
    let doomed = a.add_op();
    let doomed_id = doomed.id();
    a.send(&ClientMessage::Ops(vec![add.clone(), doomed])).await;
    let rm = StrokeOp::Remove {
        id: doomed_id,
        ts: a.clock.tick(),
    };
    a.send(&ClientMessage::Ops(vec![rm])).await;
    a.assert_quiet().await; // make sure the server has applied everything

    let (mut c, welcome) = Client::join(server.addr, "r", "c").await;
    match welcome {
        ServerMessage::Welcome { snapshot, .. } => {
            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot.tracked_len(), 2, "tombstone retained");
            assert!(snapshot.contains(add.id()));
        }
        other => panic!("unexpected {other:?}"),
    }

    c.send(&ClientMessage::RequestSnapshot).await;
    assert!(matches!(c.recv().await, ServerMessage::Snapshot(s) if s.len() == 1));

    server.stop().await;
}

#[tokio::test]
async fn presence_is_relayed_with_identity_enforced() {
    let server = TestServer::start(temp_dir()).await;
    let (mut a, _) = Client::join(server.addr, "r", "a").await;
    let (mut b, _) = Client::join(server.addr, "r", "b").await;
    let _ = a.recv().await; // PeerJoined(b)

    // Try to impersonate someone else; the server must stamp our real id.
    let mut spoofed = CursorState::new(Uuid::new_v4(), "mallory", Rgba::BLACK);
    spoofed.position = Some(Point::new(3.0, 4.0));
    spoofed.drawing = true;
    a.send(&ClientMessage::Cursor(spoofed)).await;

    match b.recv().await {
        ServerMessage::Cursor(c) => {
            assert_eq!(c.client_id, a.id);
            assert_eq!(c.name, "a");
            assert_eq!(c.position, Some(Point::new(3.0, 4.0)));
            assert!(c.drawing);
        }
        other => panic!("unexpected {other:?}"),
    }

    // Stroke previews are relayed the same way.
    a.send(&ClientMessage::StrokeDelta(common::StrokeDelta {
        stroke_id: Uuid::new_v4(),
        client_id: Uuid::new_v4(),
        color: Rgba::WHITE,
        width: 1.0,
        kind: common::StrokeKind::Freehand,
        points: vec![Point::ZERO],
    }))
    .await;
    assert!(matches!(b.recv().await, ServerMessage::StrokeDelta(d) if d.client_id == a.id));

    // A never hears its own presence back.
    a.assert_quiet().await;

    server.stop().await;
}

#[tokio::test]
async fn peer_disconnect_is_announced() {
    let server = TestServer::start(temp_dir()).await;
    let (mut a, _) = Client::join(server.addr, "r", "a").await;
    let (mut b, _) = Client::join(server.addr, "r", "b").await;
    let _ = a.recv().await; // PeerJoined(b)

    let b_id = b.id;
    b.ws.close(None).await.unwrap();
    drop(b);

    assert_eq!(a.recv().await, ServerMessage::PeerLeft(b_id));
    assert_eq!(server.registry.list().await[0].peers, 1);

    server.stop().await;
}

#[tokio::test]
async fn bad_handshakes_are_rejected() {
    let server = TestServer::start(temp_dir()).await;

    // Wrong protocol version.
    let mut c = Client::connect(server.addr).await;
    c.send(&ClientMessage::Hello {
        protocol: 99,
        client_id: c.id,
        room: "r".into(),
        name: "x".into(),
        color: Rgba::WHITE,
    })
    .await;
    assert_eq!(
        c.recv().await,
        ServerMessage::Rejected(Rejection::UnsupportedProtocol {
            server: PROTOCOL_VERSION,
            client: 99
        })
    );
    assert!(
        c.try_recv().await.is_none(),
        "server should close after rejecting"
    );

    // Non-Hello first message.
    let mut c = Client::connect(server.addr).await;
    c.send(&ClientMessage::Ping(1)).await;
    assert_eq!(
        c.recv().await,
        ServerMessage::Rejected(Rejection::NotJoined)
    );

    // Room id with path characters.
    let mut c = Client::connect(server.addr).await;
    c.send(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client_id: c.id,
        room: "../etc".into(),
        name: "x".into(),
        color: Rgba::WHITE,
    })
    .await;
    assert_eq!(
        c.recv().await,
        ServerMessage::Rejected(Rejection::InvalidRoom)
    );

    // Room capacity (max_peers = 2 in the test config).
    let (_a, _) = Client::join(server.addr, "full", "a").await;
    let (_b, _) = Client::join(server.addr, "full", "b").await;
    let mut c = Client::connect(server.addr).await;
    c.send(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client_id: c.id,
        room: "full".into(),
        name: "c".into(),
        color: Rgba::WHITE,
    })
    .await;
    assert_eq!(c.recv().await, ServerMessage::Rejected(Rejection::RoomFull));

    server.stop().await;
}

#[tokio::test]
async fn snapshots_survive_a_restart() {
    let dir = temp_dir();

    let server = TestServer::start(dir.clone()).await;
    let (mut a, _) = Client::join(server.addr, "persist", "a").await;
    let op = a.add_op();
    a.send(&ClientMessage::Ops(vec![op.clone()])).await;
    a.assert_quiet().await;
    server.stop().await; // final flush

    let file = dir.join(format!("persist.{}", codec::wire_format()));
    assert!(file.exists(), "snapshot file written at {}", file.display());

    let server = TestServer::start(dir.clone()).await;
    let (_b, welcome) = Client::join(server.addr, "persist", "b").await;
    match welcome {
        ServerMessage::Welcome { snapshot, .. } => {
            assert_eq!(snapshot.len(), 1);
            assert!(snapshot.contains(op.id()));
        }
        other => panic!("unexpected {other:?}"),
    }
    server.stop().await;

    let _ = std::fs::remove_dir_all(dir);
}
