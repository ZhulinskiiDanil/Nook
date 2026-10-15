# Nook

Windows app (Rust) that keeps the desktop clean without drawing a single frame over the wallpaper.
Corners give order; Claude Code decides what goes where through an MCP bridge; Nook previews, applies and undoes.

Spec: [TASK.md](TASK.md) · wireframe: [docs/wireframe.svg](docs/wireframe.svg)

## Status

- [x] M0 — skeleton, config, logging, DPI, single instance, `nookctl doctor`
- [x] M1 — scan & peek (read-only), `dump`, `sandbox init`
- [ ] M2 — daemon & MCP (read-only)
- [ ] M3 — layout
- [ ] M4 — plans, journal, quarantine, undo
- [ ] M5 — preview & hotkeys
- [ ] M6 — inbox, keeper, folder icons

## Build

```
cargo build --release
target\release\nookctl.exe doctor          # read-only environment report
target\release\nookctl.exe doctor --json
```

Development sandbox: set `NOOK_DESKTOP_ROOT=C:\nook-sandbox` — Nook then works on
`<root>\Desktop` and `<root>\Public` instead of the real desktop. Nook never deletes files.
