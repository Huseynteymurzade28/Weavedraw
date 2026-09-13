//! Weavedraw desktop client — egui canvas, toolbar, network loop.
//! (Step 4: implementation pending.)

fn main() {
    println!(
        "weavedraw client: protocol v{} ({} wire)",
        common::PROTOCOL_VERSION,
        common::codec::wire_format()
    );
}
