# Nook

Windows app (Rust) that keeps the desktop clean without drawing a single frame over the wallpaper.
Corners give order; Claude Code decides what goes where through an MCP bridge; Nook previews, applies and undoes.

Spec: [TASK.md](TASK.md) · wireframe: [docs/wireframe.svg](docs/wireframe.svg)

## Status

- [x] M0 — skeleton, config, logging, DPI, single instance, `nookctl doctor`
- [x] M1 — scan & peek (read-only), `dump`, `sandbox init`
- [x] M2 — daemon (tray, IPC) & MCP: `status`, `scan`, `peek` (read-only)
- [ ] M3 — layout
- [ ] M4 — plans, journal, quarantine, undo
- [ ] M5 — preview & hotkeys
- [ ] M6 — inbox, keeper, folder icons

## Build

```
cargo build --release
target\release\nookctl.exe doctor          # read-only environment report
target\release\nookctl.exe doctor --json
target\release\nookctl.exe dump [--json]          # all desktop items with hints (read-only)
target\release\nookctl.exe peek <id|name>         # thumbnail, text head, listing, PE info
target\release\nookctl.exe sandbox init --root C:\nook-sandbox
```

Development sandbox: set `NOOK_DESKTOP_ROOT=C:\nook-sandbox` — Nook then works on
`<root>\Desktop` and `<root>\Public` instead of the real desktop. Nook never deletes files.

## Claude Code

```
claude mcp add --scope user nook -- "D:\Dev\Nook\target\release\nookctl.exe" mcp
```

`nookctl mcp` starts the daemon (`nook.exe` next to it) if it isn't running. Debug the daemon directly:
`nookctl call status`, `nookctl call scan "{\"limit\":5}"`, `nookctl call peek "{\"id\":\"it_...\"}"`.
