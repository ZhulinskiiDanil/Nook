//! Pipe client (nookctl side). Starts the daemon if it isn't running.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

use super::{Request, Response, pipe_name};

pub const DAEMON_WAIT: Duration = Duration::from_secs(5);

pub struct Client {
    lines: Lines<BufReader<ReadHalf<NamedPipeClient>>>,
    writer: WriteHalf<NamedPipeClient>,
    next_id: u64,
}

fn daemon_exe() -> Result<PathBuf> {
    let me = std::env::current_exe()?;
    Ok(me.with_file_name("nook.exe"))
}

fn spawn_daemon() -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let exe = daemon_exe()?;
    // Our stdio may be Claude Code's MCP pipes. If the long-lived daemon inherited them,
    // the client would never see EOF after we exit. Make them non-inheritable first.
    // SAFETY: only flips the inherit flag on our own std handles.
    unsafe {
        use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
        use windows::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            if let Ok(h) = GetStdHandle(which)
                && !h.is_invalid()
            {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
            }
        }
    }
    if !exe.is_file() {
        bail!("daemon not running and {} not found", exe.display());
    }
    std::process::Command::new(&exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("start {}", exe.display()))?;
    tracing::info!("started daemon {}", exe.display());
    Ok(())
}

impl Client {
    /// Connects to an existing pipe without starting anything.
    pub fn connect(name: &str) -> Result<Self> {
        let pipe = ClientOptions::new()
            .open(name)
            .with_context(|| format!("open {name}"))?;
        let (r, w) = tokio::io::split(pipe);
        Ok(Self {
            lines: BufReader::new(r).lines(),
            writer: w,
            next_id: 1,
        })
    }

    /// Connects, starting `nook.exe` (next to this exe) if needed and waiting up to 5 s.
    pub async fn connect_or_start() -> Result<Self> {
        let name = pipe_name()?;
        let deadline = Instant::now() + DAEMON_WAIT;
        let mut spawned = false;
        loop {
            match ClientOptions::new().open(&name) {
                Ok(pipe) => {
                    let (r, w) = tokio::io::split(pipe);
                    return Ok(Self {
                        lines: BufReader::new(r).lines(),
                        writer: w,
                        next_id: 1,
                    });
                }
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND.0 as i32) => {
                    if !spawned {
                        spawn_daemon()?;
                        spawned = true;
                    }
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {}
                Err(e) => return Err(e).context("open daemon pipe"),
            }
            if Instant::now() > deadline {
                bail!(
                    "Nook daemon did not answer within {} s",
                    DAEMON_WAIT.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        let resp_line = self
            .lines
            .next_line()
            .await?
            .context("daemon closed the pipe")?;
        let resp: Response =
            serde_json::from_str(&resp_line).context("bad response from daemon")?;
        match (resp.result, resp.error) {
            (_, Some(e)) => bail!("{} (code {})", e.message, e.code),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }
}
