//! Pipe server (daemon side). Runs on the tokio runtime thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::HSTRING;

use super::{HandlerError, Request, Response};

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value, HandlerError>> + Send>>;
pub type Handler = Arc<dyn Fn(String, Value) -> HandlerFuture + Send + Sync>;

/// Security descriptor that grants access only to `sid`. Freed on drop.
struct OwnerOnly(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by the kernel.
unsafe impl Send for OwnerOnly {}
unsafe impl Sync for OwnerOnly {}

impl OwnerOnly {
    fn new(sid: &str) -> Result<Self> {
        // Protected DACL, one ACE: generic all for the current user. Nobody else.
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{sid})"));
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: out-param receives a LocalAlloc'd descriptor freed in Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;
        Ok(Self(sd))
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

fn create(name: &str, sd: &OwnerOnly, first: bool) -> Result<NamedPipeServer> {
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `sa` and the descriptor outlive the call; the kernel copies them.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&mut sa as *mut SECURITY_ATTRIBUTES).cast())
    }
    .with_context(|| format!("create pipe {name}"))
}

/// Accepts clients forever. Fails immediately if another daemon owns the pipe.
pub async fn serve(name: String, sid: String, handler: Handler) -> Result<()> {
    let sd = Arc::new(OwnerOnly::new(&sid)?);
    let mut server = create(&name, &sd, true)?;
    tracing::info!(pipe = %name, "ipc listening");
    loop {
        server.connect().await.context("pipe connect")?;
        let client = server;
        server = create(&name, &sd, false)?;
        let handler = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client, handler).await {
                tracing::debug!("ipc client ended: {e:#}");
            }
        });
    }
}

async fn handle_client(pipe: NamedPipeServer, handler: Handler) -> Result<()> {
    let (r, mut w) = tokio::io::split(pipe);
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let id = req.id;
                tracing::debug!(method = %req.method, id, "ipc request");
                match handler(req.method, req.params).await {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e.code, e.message),
                }
            }
            Err(e) => Response::err(0, -32700, format!("parse error: {e}")),
        };
        let mut out = serde_json::to_vec(&resp)?;
        out.push(b'\n');
        w.write_all(&out).await?;
        w.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ipc::client::Client;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn roundtrip_over_real_pipe() {
        let name = format!(r"\\.\pipe\nook-test-{}", std::process::id());
        let sid = crate::platform::current_user_sid().unwrap();
        let handler: Handler = Arc::new(|m: String, p: Value| -> HandlerFuture {
            Box::pin(async move {
                match m.as_str() {
                    "echo" => Ok(p),
                    other => Err(HandlerError::not_found(other)),
                }
            })
        });
        tokio::spawn(serve(name.clone(), sid.clone(), handler.clone()));

        let test = async {
            let mut c = loop {
                match Client::connect(&name) {
                    Ok(c) => break c,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            };
            let v = c
                .call("echo", serde_json::json!({"имя": "Семен"}))
                .await
                .unwrap();
            assert_eq!(v["имя"], "Семен");
            let e = c.call("nope", Value::Null).await.unwrap_err();
            assert!(e.to_string().contains("-32601"), "{e}");
            // A second server on the same name must fail (first_pipe_instance).
            assert!(
                serve(name.clone(), sid.clone(), handler.clone())
                    .await
                    .is_err()
            );
        };
        tokio::time::timeout(Duration::from_secs(10), test)
            .await
            .expect("IPC test timed out");
    }
}
