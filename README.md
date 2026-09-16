# Weavedraw

Real-time collaborative whiteboard in Rust: an `egui`/`eframe` desktop client
talking to a headless `axum` WebSocket server, with strokes replicated through
an LWW-Element-Set CRDT (Lamport timestamps + tombstones).

## Workspace

| Crate    | Package            | Purpose                                                        |
|----------|--------------------|----------------------------------------------------------------|
| `common` | `weavedraw-common` | Domain types, CRDT (`StrokeSet`), wire protocol, bincode codec |
| `server` | `weavedraw-server` | Room registry, WebSocket broadcast, snapshot persistence       |
| `client` | `weavedraw-client` | Hardware-accelerated canvas, floating toolbar, network loop    |

## Build & run

```sh
cargo test --workspace          # run all tests
cargo run -p weavedraw-server   # start the server on ws://127.0.0.1:8080/ws
cargo run -p weavedraw-client -- ROOM   # start a client and join ROOM (default: lobby)
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

Every element on the board is a `Stroke` with a `kind`: freehand polyline,
line, rectangle, ellipse or text. Shapes are stored as two corner points
and expanded to an outline on the client, so the CRDT, wire format and
persistence never need to know about geometry. In-progress shapes and
labels are streamed to peers like freehand strokes (replacing rather than
appending, since their far corner or text keeps changing).

Rendering: committed freehand strokes are smoothed (centripetal Catmull-Rom),
tessellated once per zoom bucket and kept on the GPU in 64-stroke chunks
that are only re-uploaded when their membership changes, so a static
drawing costs one draw call per visible chunk regardless of size. Strokes
are simplified (Ramer–Douglas–Peucker) before being replicated. Without an
OpenGL context the client falls back to painting through egui each frame.
Text is always painted through egui (it needs the font atlas) on top of the
GPU chunks.

| Input                          | Action                                   |
|--------------------------------|------------------------------------------|
| Left drag                      | Draw (Pen, Line, Rect, Ellipse) / erase (Eraser) / pan (Pan) |
| Shift + drag                   | Constrain to square / circle / 45° line  |
| Left click (Text)              | Place a label; Enter commits, Shift+Enter breaks a line, Esc cancels |
| Middle drag, Space + drag      | Pan                                      |
| Scroll, Ctrl + scroll / pinch  | Pan, zoom around the pointer             |
| `P` `L` `R` `O` `T` `E` `H`    | Pen / Line / Rect / Ellipse / Text / Eraser / Pan |
| `[` `]`                        | Brush width (text size for the Text tool) |
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
- [x] Step 5 — shape and text tools (protocol v2; v1 snapshots are moved aside as `.corrupt` and the room opens empty)
