# Weavedraw

Real-time collaborative whiteboard in Rust: an `egui`/`eframe` desktop client
talking to a headless `axum` WebSocket server, with strokes replicated through
an LWW-Element-Set CRDT (Lamport timestamps + tombstones).

## Workspace

| Crate    | Package            | Purpose                                                        |
|----------|--------------------|----------------------------------------------------------------|
| `common` | `weavedraw-common` | Domain types, CRDT (`StrokeSet`), wire protocol, bincode codec |
| `server` | `weavedraw-server` | Room registry, WebSocket broadcast, snapshot persistence       |
| `client` | `weavedraw`        | Hardware-accelerated canvas, floating toolbar, network loop    |

## Build & run

```sh
cargo test --workspace          # run all tests
cargo run -p weavedraw-server   # start the server on ws://127.0.0.1:8080/ws
cargo run -p weavedraw -- ROOM  # start a client and join ROOM (default: lobby)
```

### Server configuration

| Variable              | Default          | Meaning                                  |
|-----------------------|------------------|------------------------------------------|
| `WEAVEDRAW_ADDR`      | `127.0.0.1:8080` | Listen address                           |
| `WEAVEDRAW_DATA_DIR`  | `./data`         | Room snapshots (`<room>.bincode`)        |
| `WEAVEDRAW_MAX_PEERS` | `64`             | Connections per room before `RoomFull`   |
| `WEAVEDRAW_FLUSH_SECS`| `2`              | Interval for writing dirty rooms to disk |
| `RUST_LOG`            | `info,server=debug` | `tracing` filter                      |

HTTP routes: `GET /ws` (WebSocket upgrade), `GET /rooms` (JSON), `GET /health`.

### Client

```sh
weavedraw [ROOM] [--server ws://host:port/ws] [--name NAME]
```

| Variable            | Default                    | Meaning                    |
|---------------------|----------------------------|----------------------------|
| `WEAVEDRAW_SERVER`  | `ws://127.0.0.1:8080/ws`   | Server URL                 |
| `WEAVEDRAW_ROOM`    | `lobby`                    | Room to join               |
| `WEAVEDRAW_NAME`    | `$USER`                    | Display name shown to peers|

Command-line flags override the environment. The client reconnects with
exponential backoff; strokes drawn while offline are queued and replayed
after the next handshake.

| Input                          | Action                                   |
|--------------------------------|------------------------------------------|
| Left drag                      | Draw (Pen) / erase (Eraser) / pan (Pan)  |
| Middle drag, Space + drag      | Pan                                      |
| Scroll, Ctrl + scroll / pinch  | Pan, zoom around the pointer             |
| `P` `E` `H`                    | Pen / Eraser / Pan                       |
| `[` `]`                        | Brush width                              |
| `Ctrl+Z`, `Ctrl+Shift+Z`       | Undo / redo (own strokes only)           |
| `Ctrl+Shift+Backspace`         | Clear all of your own strokes            |
| `0`                            | Reset view                               |

Enable `--features json-wire` on **both** binaries to switch the wire format
from bincode to JSON for debugging with `websocat`.

## Status

- [x] Step 1 — workspace & dependencies
- [x] Step 2 — shared types, CRDT, protocol, codec
- [x] Step 3 — server (rooms, broadcast, persistence, graceful shutdown)
- [x] Step 4 — client (canvas, floating toolbar, presence, reconnecting network loop)
