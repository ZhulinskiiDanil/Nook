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
  в стиле lifted (restyle_icon);
↗ projects — до 5 папок проектов в стиле tint (restyle_icon); неактивные (давно не менялись) — dim;
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeArgs {
    /// The plan: title, layout groups (apps/projects/inbox/trash) and whitelisted actions.
    pub plan: crate::plan::model::PlanInput,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanIdArgs {
    /// Plan id from `propose_plan`, e.g. "pl_x2k9m4qa".
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitArgs {
    pub id: String,
    /// Seconds to wait (default 60, max 300). On timeout the answer is `still_pending`.
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct UndoArgs {
    /// Plan to undo; default: the last applied one.
    pub plan_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct HistoryArgs {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestoreArgs {
    /// Quarantine entry ids from `quarantine_list`.
    pub ids: Vec<String>,
}

#[derive(Clone)]
pub struct NookMcp {
    client: Arc<Mutex<Option<Client>>>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl NookMcp {
    /// `apply_plan` is exposed only when `safety.allow_ai_apply = true` (spec §8).
    pub fn new(allow_ai_apply: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !allow_ai_apply {
            tool_router.remove_route("apply_plan");
        }
        Self {
            client: Arc::default(),
            tool_router,
        }
    }

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

    #[tool(
        description = "Validate a plan and make it pending (replaces any pending plan). Every desktop item must end up in exactly one place: a layout group (apps ↖, projects ↗, inbox ↘, trash ↙) or off the desktop (move_to_folder / quarantine). Protected OS files (desktop.ini) are exempt — hide them with explorer_setting hide_protected_os_files. Layout entries: item id, `new:%DESKTOP%\\\\Name` for a folder the plan creates, or {item, pin:{col,row}}. Actions (each needs a non-empty reason): move_to_folder{item,dest,create_dirs}, quarantine{item}, rename{item,new_name}, hide_caption{item} (.lnk/.url only), dim{item,value}, folder_icon{item,icon:\"default\"|path}, explorer_setting{key: hide_protected_os_files|auto_arrange_off|hide_this_pc_icon, value}. Items with writable:false allow layout only. Returns plan_id and summary \"before → after\", or a list of errors."
    )]
    async fn propose_plan(
        &self,
        Parameters(a): Parameters<ProposeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let plan = serde_json::to_value(&a.plan).unwrap_or_default();
        self.json_tool("propose_plan", json!({ "plan": plan }))
            .await
    }

    #[tool(
        description = "A plan with its state (pending, applied, rejected, superseded, failed, cancelled, undone) and per-action results."
    )]
    async fn get_plan(
        &self,
        Parameters(a): Parameters<PlanIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool("get_plan", json!({ "id": a.id })).await
    }

    #[tool(
        description = "Wait until the user applies (double LCtrl+LAlt / Enter in preview) or rejects (Esc) the plan. Default 60 s, max 300. Returns decision: applied | rejected | superseded | failed | cancelled | still_pending."
    )]
    async fn wait_for_decision(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool(
            "wait_for_decision",
            json!({ "id": a.id, "timeout_s": a.timeout_s }),
        )
        .await
    }

    #[tool(description = "Withdraw a pending plan.")]
    async fn cancel_plan(
        &self,
        Parameters(a): Parameters<PlanIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool("cancel_plan", json!({ "id": a.id })).await
    }

    #[tool(
        description = "Undo the last applied plan (or the given one): files, names, attributes, desktop.ini, Explorer settings and icon positions go back. Nothing is deleted."
    )]
    async fn undo(&self, Parameters(a): Parameters<UndoArgs>) -> Result<CallToolResult, ErrorData> {
        self.json_tool("undo", json!({ "plan_id": a.plan_id }))
            .await
    }

    #[tool(description = "Applied / undone / failed plans with summaries, newest first.")]
    async fn history(
        &self,
        Parameters(a): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool("history", json!({ "limit": a.limit })).await
    }

    #[tool(
        description = "What sits in quarantine: name, original path, size, reason, plan. Only the user can empty quarantine."
    )]
    async fn quarantine_list(&self) -> Result<CallToolResult, ErrorData> {
        self.json_tool("quarantine_list", Value::Null).await
    }

    #[tool(
        description = "Move quarantined items back to where they were (name conflicts get \" (2)\")."
    )]
    async fn quarantine_restore(
        &self,
        Parameters(a): Parameters<RestoreArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool("quarantine_restore", json!({ "ids": a.ids }))
            .await
    }

    #[tool(
        description = "Apply a pending plan without the user's preview. Present only when allow_ai_apply = true."
    )]
    async fn apply_plan(
        &self,
        Parameters(a): Parameters<PlanIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.json_tool("apply_plan", json!({ "id": a.id })).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for NookMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("nook", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serves MCP over stdio until the client disconnects.
pub async fn serve_stdio(allow_ai_apply: bool) -> anyhow::Result<()> {
    let service = NookMcp::new(allow_ai_apply)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
