//! `nookctl mcp`: stdio MCP server (rmcp). Every tool call is proxied to the daemon over IPC.
//! stdout carries JSON-RPC only; logs go to the file.

use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::ipc::client::Client;

/// Appendix A of the spec.
pub const INSTRUCTIONS: &str =
    "Ты разбираешь рабочий стол пользователя через Nook. Порядок задают углы:
↖ apps — ярлыки, которыми реально пользуются (до ~10), без подписей (hide_caption),
  в стиле tile (restyle_icon);
↗ projects — до 5 папок проектов в стиле orb (restyle_icon); неактивные (давно не менялись) — dim;
↘ inbox — свежее (до 7 дней) и то, что непонятно куда;
↙ trash — Корзина.

1. status → scan. Непонятное — peek.
2. Каждый элемент — ровно в одно место: группа или вне стола
   (move_to_folder в папку проекта / quarantine).
3. В карантин — только очевидный мусор: точные дубли (одинаковый хеш), установщики уже
   установленных программ, логи и временные файлы, битые ярлыки, ярлыки-дубли панели задач,
   ярлыки, которые не запускались больше полугода. Работы пользователя (psd, исходники,
   конфиги, проекты) — никогда в карантин, только в папки или во входящие.
4. Не трогай файлы своими инструментами (shell, PowerShell) — только через propose_plan.
5. После propose_plan коротко (5–10 строк) скажи, что куда и почему, отдельно — что уходит
   в карантин. Попроси зажать Ctrl+Alt для превью и вызови wait_for_decision.
6. Правки пользователя — новым propose_plan (старый заменится).";

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ScanArgs {
    /// Index of the first item to return (default 0).
    pub offset: Option<usize>,
    /// Max items to return (default 200, max 1000). 0 = summary only, no items.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeekArgs {
    /// Item id from `scan`, e.g. "it_7f3k2q4a".
    pub id: String,
}

#[derive(Clone, Default)]
pub struct NookMcp {
    client: Arc<Mutex<Option<Client>>>,
}

impl NookMcp {
    /// Calls the daemon; reconnects (and starts it) once if the pipe broke.
    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let mut guard = self.client.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(Client::connect_or_start().await?);
            }
            match guard.as_mut().unwrap().call(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 && e.to_string().contains("closed") => *guard = None,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn json_tool(&self, method: &str, params: Value) -> Result<CallToolResult, ErrorData> {
        match self.call(method, params).await {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&v).unwrap_or_default(),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{e:#}"
            ))])),
        }
    }
}

#[tool_router]
impl NookMcp {
    #[tool(
        description = "Nook and desktop status: version, Windows, monitor, grid (cols x rows, step), icon size, view flags (auto-arrange, snap), Explorer settings, sandbox mode, pending/last plan, quarantine size."
    )]
    async fn status(&self) -> Result<CallToolResult, ErrorData> {
        self.json_tool("status", Value::Null).await
    }

    #[tool(
        description = "All desktop items (user Desktop, Public Desktop, virtual) with id, kind, cell, shortcut target/pins/launches, folder summary and hints. Paged: default up to 200. Call with no limit to get every item; `limit: 0` returns only `summary`. Use `summary` for counts, do not recount by hand. Semantics: `duplicate_of` = byte-identical content (same blake3 hash); `is_copy` = only the NAME looks like a copy (content may differ — not a duplicate); `writable: false` = layout only (position on the desktop), Nook cannot move, rename or quarantine it (Public Desktop, virtual items); `launch_count`/`last_launch` absent = no data (tracking may be off), not \"never used\"; Nook does not know Recycle Bin contents."
    )]
    async fn scan(&self, Parameters(a): Parameters<ScanArgs>) -> Result<CallToolResult, ErrorData> {
        self.json_tool("scan", json!({ "offset": a.offset, "limit": a.limit }))
            .await
    }

    #[tool(
        description = "Look inside one item by id: shell thumbnail up to 512 px (images, video, PSD, PDF), first 4 KB of text (UTF-8/UTF-16/CP1251), folder or zip listing (up to 50), exe FileDescription/ProductName/CompanyName."
    )]
    async fn peek(&self, Parameters(a): Parameters<PeekArgs>) -> Result<CallToolResult, ErrorData> {
        let mut v = match self.call("peek", json!({ "id": a.id })).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{e:#}"
                ))]));
            }
        };
        let img = v
            .as_object_mut()
            .and_then(|o| o.remove("thumbnail_png_base64"));
        let mut content = vec![ContentBlock::text(
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )];
        if let Some(Value::String(b64)) = img
            && base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .is_ok()
        {
            content.push(ContentBlock::image(b64, "image/png"));
        }
        Ok(CallToolResult::success(content))
    }
}

#[tool_handler]
impl ServerHandler for NookMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("nook", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serves MCP over stdio until the client disconnects.
pub async fn serve_stdio() -> anyhow::Result<()> {
    let service = NookMcp::default().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
