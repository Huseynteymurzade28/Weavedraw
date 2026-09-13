//! Weavedraw WebSocket server — room registry, broadcast, persistence.
//! (Step 3: implementation pending.)

fn main() {
    println!(
        "weavedraw-server: protocol v{} ({} wire)",
        common::PROTOCOL_VERSION,
        common::codec::wire_format()
    );
}
