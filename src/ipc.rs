//! IPC wire protocol (line-delimited JSON over a Unix socket) and the CLI client.
//!
//! The wire format is unchanged from the tokio version, so existing keybinds and
//! scripts keep working. What changed: there is no async server and no `Command`
//! enum / `oneshot` round-trip. The reactor parses a `Request` and produces a
//! `Reply` inline on its single thread (see `reactor::handle_ipc`), because the
//! state it needs is right there — no task boundary to cross.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status,
    Pause { seconds: u64 },
    Resume,
    Nudge { delta: i32 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusReply {
    pub lux_raw: f32,
    pub lux_smoothed: f32,
    pub display_pct: Option<f32>,
    pub keyboard_pct: Option<f32>,
    pub paused_until_unix: Option<u64>,
    pub display_override_active: bool,
    pub keyboard_override_active: bool,
    pub idle: bool,
    pub nudge_pct: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Status(StatusReply),
    Error(String),
}

pub fn socket_path() -> Result<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    Ok(dir.join("asahi-brightness.sock"))
}

pub mod client {
    use super::*;

    fn round_trip(request: Request) -> Result<Reply> {
        let path = socket_path()?;
        let mut stream = UnixStream::connect(&path)
            .with_context(|| format!("connecting {}. Is the daemon running?", path.display()))?;
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response)?;
        if response.trim().is_empty() {
            bail!("daemon closed connection without reply");
        }
        Ok(serde_json::from_str(response.trim())?)
    }

    pub fn status() -> Result<()> {
        match round_trip(Request::Status)? {
            Reply::Status(status) => {
                println!("{}", serde_json::to_string_pretty(&status)?);
                Ok(())
            }
            Reply::Error(msg) => Err(anyhow::anyhow!(msg)),
            Reply::Ok => Err(anyhow::anyhow!("unexpected Ok reply to Status")),
        }
    }

    pub fn pause(seconds: u64) -> Result<()> {
        expect_ok(round_trip(Request::Pause { seconds })?)
    }

    pub fn resume() -> Result<()> {
        expect_ok(round_trip(Request::Resume)?)
    }

    pub fn nudge(delta: i32) -> Result<()> {
        expect_ok(round_trip(Request::Nudge { delta })?)
    }

    fn expect_ok(reply: Reply) -> Result<()> {
        match reply {
            Reply::Ok => Ok(()),
            Reply::Error(msg) => Err(anyhow::anyhow!(msg)),
            Reply::Status(_) => Err(anyhow::anyhow!("unexpected Status reply")),
        }
    }
}
