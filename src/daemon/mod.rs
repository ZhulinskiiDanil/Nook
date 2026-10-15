//! The daemon: STA UI thread (window, tray, desktop view) + tokio thread (IPC).
//! Heavy work (scan, peek) runs on blocking workers with their own COM apartment.

pub mod desk;
pub mod ui;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::ipc::server::{Handler, HandlerFuture};
use crate::ipc::{HandlerError, pipe_name};
use crate::paths::Desktops;
use crate::plan::apply::{self, Desk, Env, NoDesk};
use crate::plan::model::{PlanInput, PlanState, StoredPlan};
use crate::plan::store::Store;
use crate::platform::{ComApartment, current_user_sid};
use crate::scan::{ScanResult, scan_with, view_snapshot};
use crate::shell::desktop_view::DesktopView;
use ui::{Ui, UiBridge};

pub const SCAN_DEFAULT_LIMIT: usize = 200;

/// Spec §10: file operations on the real desktop stay locked until the user writes
/// «можно на живом». Unlocked on 2026-10-15 at the user's explicit go-ahead.
pub const LIVE_FILE_OPS_UNLOCKED: bool = true;

struct State {
    config: Config,
    desktops: Desktops,
    ui: UiBridge,
    data_dir: PathBuf,
    store: Store,
    last_scan: Mutex<Option<Arc<ScanResult>>>,
    /// Bumped on every plan state change; `wait_for_decision` watches it.
    decisions: tokio::sync::watch::Sender<u64>,
    /// One apply/undo at a time.
    apply_lock: Mutex<()>,
}

impl State {
    fn desk(&self) -> Box<dyn Desk> {
        if self.desktops.is_sandbox() {
            Box::new(NoDesk)
        } else {
            Box::new(desk::LiveDesk {
                ui: self.ui.clone(),
                data_dir: self.data_dir.clone(),
            })
        }
    }

    fn with_env<T>(&self, f: impl FnOnce(&Env) -> Result<T>) -> Result<T> {
        let desk = self.desk();
        let env = Env {
            desktops: &self.desktops,
            config: &self.config,
            store: &self.store,
            desk: desk.as_ref(),
            shows_hidden: crate::shell::explorer_settings::ExplorerSettings::read().shows_hidden(),
            fail_at_step: None,
        };
        f(&env)
    }

    fn changed(&self) {
        self.decisions.send_modify(|n| *n += 1);
    }

    fn guard_live(&self) -> Result<(), HandlerError> {
        if !self.desktops.is_sandbox() && !LIVE_FILE_OPS_UNLOCKED {
            return Err(HandlerError::invalid(
                "file operations on the real desktop are locked during development (spec §10). \
                 Run the daemon in sandbox mode (NOOK_DESKTOP_ROOT) or wait for «можно на живом».",
            ));
        }
        Ok(())
    }

    /// Full scan on the calling (blocking worker) thread; view snapshot via the UI thread.
    fn scan(&self) -> Result<Arc<ScanResult>> {
        let _com = ComApartment::sta()?;
        let ui = self.ui.clone();
        let r = Arc::new(scan_with(&self.desktops, move || {
            ui.call(view_snapshot)?
        })?);
        *self.last_scan.lock().unwrap() = Some(r.clone());
        Ok(r)
    }
}

#[derive(Deserialize, Default)]
struct ScanParams {
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct PeekParams {
    id: String,
}

#[derive(Deserialize)]
struct ProposeParams {
    plan: PlanInput,
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize, Default)]
struct OptIdParams {
    plan_id: Option<String>,
}

#[derive(Deserialize)]
struct WaitParams {
    id: String,
    timeout_s: Option<u64>,
}

#[derive(Deserialize, Default)]
struct HistoryParams {
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct RestoreParams {
    ids: Vec<String>,
}

fn req<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, HandlerError> {
    serde_json::from_value(v).map_err(|e| HandlerError::invalid(e.to_string()))
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, HandlerError> {
    Ok(serde_json::to_value(v).map_err(anyhow::Error::from)?)
}

/// Compact plan view for Claude.
fn plan_view(sp: &StoredPlan) -> Value {
    json!({
        "id": sp.id,
        "title": sp.plan.title,
        "state": sp.state,
        "sandbox": sp.sandbox,
        "summary": sp.summary,
        "created": sp.created,
        "applied_at": sp.applied_at,
        "warnings": sp.warnings,
        "results": sp.results,
        "plan": sp.plan,
    })
}

fn decision_view(sp: &StoredPlan) -> Value {
    let decision = match sp.state {
        PlanState::Pending => "still_pending",
        PlanState::Applied => "applied",
        PlanState::Rejected => "rejected",
        PlanState::Superseded => "superseded",
        PlanState::Failed => "failed",
        PlanState::Cancelled => "cancelled",
        PlanState::Undone => "undone",
    };
    json!({ "id": sp.id, "decision": decision, "summary": sp.summary, "results": sp.results })
}

fn blocking<F>(f: F) -> HandlerFuture
where
    F: FnOnce() -> Result<Value, HandlerError> + Send + 'static,
{
    Box::pin(async move {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| HandlerError::from(anyhow::anyhow!("worker panicked: {e}")))?
    })
}

fn params<T: for<'de> Deserialize<'de> + Default>(v: Value) -> Result<T, HandlerError> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| HandlerError::invalid(e.to_string()))
}

fn dispatch(state: Arc<State>, method: String, p: Value) -> HandlerFuture {
    match method.as_str() {
        "ping" => {
            Box::pin(async { Ok(json!({"pong": true, "version": env!("CARGO_PKG_VERSION")})) })
        }
        "status" => blocking(move || {
            let view = (!state.desktops.is_sandbox()).then(|| {
                state
                    .ui
                    .call(|| DesktopView::open().and_then(|v| v.info()))
                    .and_then(|r| r)
            });
            let mut s =
                crate::status::collect(&state.desktops, &state.config.safety.quarantine_dir, view);
            let line = |sp: &StoredPlan| {
                format!(
                    "{} «{}» {} ({:?})",
                    sp.id, sp.plan.title, sp.summary, sp.state
                )
            };
            s.pending_plan = state.store.pending().map(|sp| line(&sp));
            s.last_plan = state
                .store
                .last_applied(state.desktops.is_sandbox())
                .map(|sp| line(&sp));
            if state.desktops.is_sandbox()
                && let crate::paths::DesktopMode::Sandbox(root) = &state.desktops.mode
                && let Some(q) = s.quarantine.as_mut()
            {
                // Sandbox quarantine lives inside the sandbox.
                q.path = root.join("quarantine").display().to_string();
            }
            let mut v = serde_json::to_value(s).map_err(anyhow::Error::from)?;
            v["live_file_ops_locked"] =
                json!(!state.desktops.is_sandbox() && !LIVE_FILE_OPS_UNLOCKED);
            Ok(v)
        }),
        "scan" => blocking(move || {
            let sp: ScanParams = params(p)?;
            let r = state.scan()?;
            let offset = sp.offset.unwrap_or(0);
            let limit = sp.limit.unwrap_or(SCAN_DEFAULT_LIMIT).min(1000);
            let page: Vec<_> = r.items.iter().skip(offset).take(limit).collect();
            Ok(json!({
                "sandbox": r.sandbox,
                "total": r.total,
                "summary": summary(&r),
                "offset": offset,
                "returned": page.len(),
                "grid": r.grid,
                "items": page,
                "warnings": r.warnings,
            }))
        }),
        "peek" => blocking(move || {
            let pp: PeekParams =
                serde_json::from_value(p).map_err(|e| HandlerError::invalid(e.to_string()))?;
            let cached = state.last_scan.lock().unwrap().clone();
            let find = |r: &ScanResult| r.items.iter().find(|i| i.id == pp.id).cloned();
            let item = match cached.as_deref().and_then(find) {
                Some(i) => i,
                None => find(&*state.scan()?)
                    .ok_or_else(|| HandlerError::invalid(format!("no item {}", pp.id)))?,
            };
            let _com = ComApartment::sta()?;
            let pk = crate::intel::peek::peek(&item);
            let mut v = serde_json::to_value(&pk).map_err(anyhow::Error::from)?;
            if let Some(png) = &pk.thumbnail_png {
                v["thumbnail_png_base64"] =
                    json!(base64::engine::general_purpose::STANDARD.encode(png));
            }
            Ok(v)
        }),
        "propose_plan" => blocking(move || {
            let pp: ProposeParams = req(p)?;
            let scan = state.scan()?;
            let r = state.with_env(|env| apply::propose(pp.plan, &scan, env))?;
            state.changed();
            match r {
                Ok(prop) => Ok(json!({
                    "ok": true,
                    "plan_id": prop.plan_id,
                    "summary": prop.summary,
                    "warnings": prop.warnings,
                    "superseded": prop.superseded,
                    "sandbox": prop.sandbox,
                    "next": "Tell the user what goes where (5-10 lines), ask them to hold LCtrl+LAlt for a preview, then call wait_for_decision.",
                })),
                Err(errors) => Ok(json!({ "ok": false, "errors": errors })),
            }
        }),
        "get_plan" => blocking(move || {
            let ip: IdParams = req(p)?;
            Ok(plan_view(&state.store.load(&ip.id)?))
        }),
        "wait_for_decision" => Box::pin(async move {
            let wp: WaitParams = req(p)?;
            let timeout = Duration::from_secs(wp.timeout_s.unwrap_or(60).clamp(1, 300));
            let mut rx = state.decisions.subscribe();
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let sp = state.store.load(&wp.id)?;
                if sp.state != PlanState::Pending {
                    return Ok(decision_view(&sp));
                }
                if tokio::time::timeout_at(deadline, rx.changed())
                    .await
                    .is_err()
                {
                    return Ok(decision_view(&sp));
                }
            }
        }),
        "cancel_plan" => blocking(move || {
            let ip: IdParams = req(p)?;
            let sp = apply::set_state(&ip.id, PlanState::Cancelled, &state.store)?;
            state.changed();
            Ok(decision_view(&sp))
        }),
        "reject_plan" => blocking(move || {
            let ip: IdParams = req(p)?;
            let sp = apply::set_state(&ip.id, PlanState::Rejected, &state.store)?;
            state.changed();
            Ok(decision_view(&sp))
        }),
        "apply_plan" => blocking(move || {
            let ip: IdParams = req(p)?;
            state.guard_live()?;
            let _g = state.apply_lock.lock().unwrap();
            let scan = state.scan()?;
            let _com = ComApartment::sta()?;
            let sp = state.with_env(|env| apply::apply(&ip.id, &scan, env))?;
            state.changed();
            Ok(decision_view(&sp))
        }),
        "undo" => blocking(move || {
            let op: OptIdParams = params(p)?;
            state.guard_live()?;
            let _g = state.apply_lock.lock().unwrap();
            let _com = ComApartment::sta()?;
            let sp = state.with_env(|env| apply::undo(op.plan_id.as_deref(), env))?;
            state.changed();
            Ok(decision_view(&sp))
        }),
        "history" => blocking(move || {
            let hp: HistoryParams = params(p)?;
            let list: Vec<Value> = state
                .store
                .list()
                .iter()
                .filter(|sp| matches!(sp.state, PlanState::Applied | PlanState::Undone | PlanState::Failed))
                .take(hp.limit.unwrap_or(20))
                .map(|sp| {
                    json!({ "id": sp.id, "title": sp.plan.title, "state": sp.state, "summary": sp.summary,
                            "applied_at": sp.applied_at, "sandbox": sp.sandbox })
                })
                .collect();
            Ok(json!({ "plans": list }))
        }),
        "quarantine_list" => blocking(move || {
            let q = state.with_env(apply::quarantine_list)?;
            let bytes: u64 = q.iter().map(|e| e.size).sum();
            Ok(json!({ "count": q.len(), "bytes": bytes, "items": to_value(&q)? }))
        }),
        "quarantine_restore" => blocking(move || {
            let rp: RestoreParams = req(p)?;
            state.guard_live()?;
            let _g = state.apply_lock.lock().unwrap();
            let _com = ComApartment::sta()?;
            let report = state.with_env(|env| apply::quarantine_restore(&rp.ids, env))?;
            Ok(json!({ "report": report }))
        }),
        other => {
            let e = HandlerError::not_found(other);
            Box::pin(async move { Err(e) })
        }
    }
}

/// Runs the daemon until the user picks "Выход". Call on the main thread.
pub fn run(config: Config) -> Result<()> {
    let _com = ComApartment::sta()?;
    let desktops = Desktops::resolve()?;
    tracing::info!(sandbox = desktops.is_sandbox(), user = %desktops.user.display(), "desktops");
    let (ui, bridge) = Ui::create()?;

    let data_dir = crate::paths::AppDirs::resolve()?.data_dir;
    let store = Store::new(&data_dir);
    let (decisions, _) = tokio::sync::watch::channel(0u64);
    let state = Arc::new(State {
        config,
        desktops,
        ui: bridge,
        data_dir,
        store,
        last_scan: Mutex::new(None),
        decisions,
        apply_lock: Mutex::new(()),
    });
    let name = pipe_name()?;
    let sid = current_user_sid()?;
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("tokio runtime: {e}");
                    return;
                }
            };
            let handler: Handler = Arc::new(move |m, p| dispatch(state.clone(), m, p));
            if let Err(e) = rt.block_on(crate::ipc::server::serve(name, sid, handler)) {
                tracing::error!("ipc server stopped: {e:#}");
            }
        })
        .context("spawn ipc thread")?;

    ui.run();
    tracing::info!("message loop ended");
    Ok(())
}

/// Counts Claude should quote instead of recounting.
fn summary(r: &ScanResult) -> Value {
    use std::collections::BTreeSet;

    use crate::scan::{Item, Kind, Source};
    let count = |f: &dyn Fn(&Item) -> bool| r.items.iter().filter(|i| f(i)).count();
    let dup_groups: BTreeSet<Vec<&str>> = r
        .items
        .iter()
        .filter(|i| !i.hints.duplicate_of.is_empty())
        .map(|i| {
            let mut g: Vec<&str> = std::iter::once(i.id.as_str())
                .chain(i.hints.duplicate_of.iter().map(String::as_str))
                .collect();
            g.sort_unstable();
            g
        })
        .collect();
    json!({
        "total": r.total,
        "by_source": {
            "user": count(&|i| i.source == Source::User),
            "public": count(&|i| i.source == Source::Public),
            "virtual": count(&|i| i.source == Source::Virtual),
        },
        "by_kind": {
            "shortcut": count(&|i| matches!(i.kind, Kind::Shortcut | Kind::Url)),
            "folder": count(&|i| i.kind == Kind::Folder),
            "file": count(&|i| i.kind == Kind::File),
            "virtual": count(&|i| i.kind == Kind::Virtual),
        },
        "layout_only": count(&|i| !i.writable),
        "exact_duplicate_groups": dup_groups.len(),
        "name_looks_like_copy": count(&|i| i.hints.is_copy),
        "broken_shortcuts": count(&|i| i.shortcut.as_ref().is_some_and(|s| s.target_exists == Some(false))),
        "pinned_to_taskbar": count(&|i| i.shortcut.as_ref().is_some_and(|s| s.pinned_to_taskbar)),
        // Start_TrackProgs = 0 disables UserAssist; old entries may still exist.
        "launch_tracking_enabled": crate::shell::explorer_settings::launch_tracking_enabled(),
        "items_with_launch_data": count(&|i| i.shortcut.as_ref().is_some_and(|s| s.launch_count.is_some())),
    })
}
