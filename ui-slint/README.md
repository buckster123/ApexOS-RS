# ui-slint

> The native Slint KMS/DRM (or winit) UI binary — `apexos-rs-ui`.

ApexOS-RS's unique contribution: a single ~30 MB native UI rendered straight to the display via
KMS/DRM (no browser, no compositor) on a Pi, or a desktop window via winit. Chat, tool cards,
dashboard, sensor heatmap, council, terminal, sketchpad, and the GL/SDF face — all driven off the
agentd WebSocket. tokio runs on background threads; the Slint event loop owns the main thread.

- **Key files:** `src/main.rs` (tokio bootstrap, WS connect+reconnect, event→model mapping, window manager) · `src/ui/appwindow.slint` (root) · `src/ui/components/` (chat_view, tool_card, dashboard, sensor_view, council_view, terminal_view, …) · `src/ui/types.slint` · `build.rs`
- **Depends on:** `slint` (features `backend-linuxkms-noseat` + `backend-winit`), `tokio`, `tokio-tungstenite`, `futures-util`, `serde`/`serde_json`, `reqwest`, `slint-build`. Needs `libfontconfig1-dev` to build.
- **Lift via:** the binary is ApexOS-specific, but the hard-won Slint-on-KMS patterns are documented as recipes in [`docs/slint-notes.md`](../docs/slint-notes.md) (thread model, `VecModel`, `ScrollView` on linuxkms, the GL face).

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../docs/repo-map.md) (full map).
