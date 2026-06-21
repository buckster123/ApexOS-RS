# apexos-tools

> MCP-over-stdio system tool plugin: shell / file / http / sysinfo / audio / GPIO / display.

The agent's hands on the host: ~28 tools over an MCP stdio loop — run_command (denylist-guarded),
read/write/list files, http_fetch (SSRF-guarded), git_*, camera/screenshot capture, audio editing,
GPIO, the face display. FS access is confined through the std-only `apexos-confine` crate; this
crate supplies the policy (per-agent workspace, read/git roots, secret denylist).

- **Key files:** `src/tools.rs` (`list()` schema + `call()` dispatch, `confine`/`confine_git_repo`, `denylist_check`) · `src/main.rs` (stdio JSON-RPC loop)
- **Depends on:** `apexos-confine`, `serde`/`serde_json`, `reqwest` (blocking).
- **Lift via:** run beside any MCP-speaking agent for host tools; or lift individual tool impls. The confinement *mechanism* lives in [`apexos-confine`](../../../apexos-confine/) (liftable on its own).

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../../../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../../../docs/repo-map.md) (full map).
