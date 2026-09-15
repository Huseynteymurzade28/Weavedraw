//! Weavedraw desktop client — egui canvas, floating toolbar, network loop.
//!
//! ```text
//! weavedraw [ROOM] [--server ws://host:port/ws] [--name NAME]
//! ```
//! Every flag can also come from `WEAVEDRAW_SERVER`, `WEAVEDRAW_ROOM` and
//! `WEAVEDRAW_NAME`; command-line arguments win.

mod app;
mod canvas;
mod net;

use anyhow::{Context, bail};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::net::NetConfig;

const DEFAULT_SERVER: &str = "ws://127.0.0.1:8080/ws";
const DEFAULT_ROOM: &str = "lobby";

fn parse_args() -> anyhow::Result<NetConfig> {
    let mut url = std::env::var("WEAVEDRAW_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_owned());
    let mut room = std::env::var("WEAVEDRAW_ROOM").unwrap_or_else(|_| DEFAULT_ROOM.to_owned());
    let mut name = std::env::var("WEAVEDRAW_NAME").ok();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: weavedraw [ROOM] [--server URL] [--name NAME]");
                std::process::exit(0);
            }
            "--server" | "-s" => url = args.next().context("--server needs a URL")?,
            "--name" | "-n" => name = Some(args.next().context("--name needs a value")?),
            other if other.starts_with('-') => bail!("unknown flag {other}"),
            positional => room = positional.to_owned(),
        }
    }

    let client_id = Uuid::new_v4();
    let name = name
        .or_else(|| std::env::var("USER").ok())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| format!("guest-{}", &client_id.simple().to_string()[..4]));

    Ok(NetConfig {
        url,
        room,
        name,
        color: app::presence_color(client_id),
        client_id,
    })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = parse_args()?;
    tracing::info!(
        url = %config.url, room = %config.room, name = %config.name,
        wire = common::codec::wire_format(), "starting weavedraw client"
    );

    let title = format!("Weavedraw — {}", config.room);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(&title)
            .with_app_id("weavedraw")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| Ok(Box::new(app::WeavedrawApp::new(cc, config)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
