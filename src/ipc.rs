use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

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

pub enum Command {
    Pause(u64),
    Resume,
    Nudge(i32),
    GetStatus(oneshot::Sender<StatusReply>),
}

pub mod server {
    use super::*;

    pub async fn run(tx: mpsc::Sender<Command>) -> Result<()> {
        let path = socket_path()?;
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let listener =
            UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        tracing::info!(path = %path.display(), "IPC socket listening");

        loop {
            let (stream, _) = listener.accept().await?;
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(err) = handle(stream, tx).await {
                    tracing::warn!(error = %err, "IPC client error");
                }
            });
        }
    }

    async fn handle(stream: UnixStream, tx: mpsc::Sender<Command>) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            let request: Request = match serde_json::from_str(&line) {
                Ok(parsed) => parsed,
                Err(err) => {
                    write_reply(&mut writer, Reply::Error(format!("bad request: {err}"))).await?;
                    continue;
                }
            };
            let reply = match request {
                Request::Status => {
                    let (oneshot_tx, oneshot_rx) = oneshot::channel();
                    if tx.send(Command::GetStatus(oneshot_tx)).await.is_err() {
                        Reply::Error("daemon channel closed".into())
                    } else {
                        match oneshot_rx.await {
                            Ok(status) => Reply::Status(status),
                            Err(_) => Reply::Error("daemon dropped status request".into()),
                        }
                    }
                }
                Request::Pause { seconds } => {
                    let _ = tx.send(Command::Pause(seconds)).await;
                    Reply::Ok
                }
                Request::Resume => {
                    let _ = tx.send(Command::Resume).await;
                    Reply::Ok
                }
                Request::Nudge { delta } => {
                    let _ = tx.send(Command::Nudge(delta)).await;
                    Reply::Ok
                }
            };
            write_reply(&mut writer, reply).await?;
        }
        Ok(())
    }

    async fn write_reply<Writer: AsyncWriteExt + Unpin>(
        writer: &mut Writer,
        reply: Reply,
    ) -> Result<()> {
        let mut body = serde_json::to_string(&reply)?;
        body.push('\n');
        writer.write_all(body.as_bytes()).await?;
        Ok(())
    }
}

pub mod client {
    use super::*;

    async fn round_trip(request: Request) -> Result<Reply> {
        let path = socket_path()?;
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting {}. Is the daemon running?", path.display()))?;
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;
        let (reader, _writer) = stream.split();
        let mut lines = BufReader::new(reader).lines();
        let response = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("daemon closed connection without reply"))?;
        Ok(serde_json::from_str(&response)?)
    }

    pub async fn status() -> Result<()> {
        match round_trip(Request::Status).await? {
            Reply::Status(status) => {
                println!("{}", serde_json::to_string_pretty(&status)?);
                Ok(())
            }
            Reply::Error(msg) => Err(anyhow::anyhow!(msg)),
            Reply::Ok => Err(anyhow::anyhow!("unexpected Ok reply to Status")),
        }
    }

    pub async fn pause(seconds: u64) -> Result<()> {
        expect_ok(round_trip(Request::Pause { seconds }).await?)
    }

    pub async fn resume() -> Result<()> {
        expect_ok(round_trip(Request::Resume).await?)
    }

    pub async fn nudge(delta: i32) -> Result<()> {
        expect_ok(round_trip(Request::Nudge { delta }).await?)
    }

    fn expect_ok(reply: Reply) -> Result<()> {
        match reply {
            Reply::Ok => Ok(()),
            Reply::Error(msg) => Err(anyhow::anyhow!(msg)),
            Reply::Status(_) => Err(anyhow::anyhow!("unexpected Status reply")),
        }
    }
}
