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
cargo run -p weavedraw-server   # start the server (step 3)
cargo run -p weavedraw          # start a client   (step 4)
```

Enable `--features json-wire` on **both** binaries to switch the wire format
from bincode to JSON for debugging with `websocat`.

## Status

- [x] Step 1 — workspace & dependencies
- [x] Step 2 — shared types, CRDT, protocol, codec
- [ ] Step 3 — server
- [ ] Step 4 — client
