//! Runtime configuration, read from environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    /// `WEAVEDRAW_ADDR` — socket to listen on. Default `127.0.0.1:8080`.
    pub addr: SocketAddr,
    /// `WEAVEDRAW_DATA_DIR` — where room snapshots are persisted. Default `./data`.
    pub data_dir: PathBuf,
    /// `WEAVEDRAW_MAX_PEERS` — connections per room before `RoomFull`. Default 64.
    pub max_peers: usize,
    /// `WEAVEDRAW_FLUSH_SECS` — how often dirty rooms are written to disk. Default 2.
    pub flush_interval: Duration,
    /// How long a fresh connection may stay silent before sending `Hello`.
    pub hello_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: ([127, 0, 0, 1], 8080).into(),
            data_dir: PathBuf::from("data"),
            max_peers: 64,
            flush_interval: Duration::from_secs(2),
            hello_timeout: Duration::from_secs(10),
        }
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let mut cfg = Config::default();
        if let Ok(v) = std::env::var("WEAVEDRAW_ADDR") {
            cfg.addr = v
                .parse()
                .context("WEAVEDRAW_ADDR is not a socket address")?;
        }
        if let Ok(v) = std::env::var("WEAVEDRAW_DATA_DIR") {
            cfg.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("WEAVEDRAW_MAX_PEERS") {
            cfg.max_peers = v.parse().context("WEAVEDRAW_MAX_PEERS is not a number")?;
        }
        if let Ok(v) = std::env::var("WEAVEDRAW_FLUSH_SECS") {
            cfg.flush_interval =
                Duration::from_secs(v.parse().context("WEAVEDRAW_FLUSH_SECS is not a number")?);
        }
        Ok(cfg)
    }
}
