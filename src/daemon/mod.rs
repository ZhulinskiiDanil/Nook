//! The daemon: STA UI thread (window, tray, desktop view) + tokio thread (IPC).
//! Heavy work (scan, peek) runs on blocking workers with their own COM apartment.

pub mod ui;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::ipc::server::{Handler, HandlerFuture};
use crate::ipc::{HandlerError, pipe_name};
use crate::paths::Desktops;
use crate::platform::{ComApartment, current_user_sid};
use crate::scan::{ScanResult, scan_with, view_snapshot};
use crate::shell::desktop_view::DesktopView;
use ui::{Ui, UiBridge};

pub const SCAN_DEFAULT_LIMIT: usize = 200;

struct State {
    config: Config,
    desktops: Desktops,
    ui: UiBridge,
    last_scan: Mutex<Option<Arc<ScanResult>>>,
}

impl State {
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
            let s =
                crate::status::collect(&state.desktops, &state.config.safety.quarantine_dir, view);
            Ok(serde_json::to_value(s).map_err(anyhow::Error::from)?)
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

    let state = Arc::new(State {
        config,
        desktops,
        ui: bridge,
        last_scan: Mutex::new(None),
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
