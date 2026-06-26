# USB exo-workspace — a portable agent workspace on removable media

Plug a prepared USB stick into any ApexOS node and it becomes part of the agent's
workspace: APEX reads **and writes** it, it shows up in the Explorer (📁 Files), and
the desktop apps reach it — so a human carries their work between nodes (and, later,
hands off to the phone PWA). Slice 1 delivers the **plug → mount → use → eject** core.

## The idea (why it's simple under the hood)

Everything in ApexOS confines file access to **`AGENTD_WORKSPACE`** (`/var/lib/agentd/
workspace`): the Explorer (`/api/workspace/list` → `resolve_workspace_path`), the
agent's FS tools (`tools.rs::confine()`), and the desktop apps. So if a stick mounts
**under** the workspace at `…/workspace/media/<label>`, it's automatically reachable
everywhere with **zero confine/gateway/security changes** — it just appears as a
`media/` folder. That's the whole trick.

## The convention (the "marker")

A stick is an ApexOS **exo-workspace** when:
- its **filesystem LABEL** is `APEX-<name>` — udev reads this *without* mounting, so
  it's the claim discriminator (no probe-mount race); and
- it carries an **`apexos-workspace.toml`** marker at its root + the standard layout
  (`projects/ data/ notes/`).

**exFAT is recommended** — the owner uid is set at mount time (so the stick is
portable across nodes whatever each node's `agentd` uid is) and it's Mac/Windows/
Android-friendly (the future phone-handoff leg). Prepare a stick with
`apexos-workspace-init <mountpoint> [name]` (writes the marker + layout, and prints
the relabel command, since relabelling needs the unmounted device).

## Marker-gated own-mount (the claim)

Only `APEX-*` sticks are claimed — every other USB is left to the desktop's file
manager (GNOME/udisks) or simply ignored on a kiosk, so this is safe on a
daily-driver laptop. **Why own-mount instead of adopting the DE's mount:** `agentd`
runs as the non-root `agentd` user; a DE mounts sticks as *you* with restrictive
perms, so the agent couldn't write them. So we mount with `uid=agentd` ourselves.

| Piece | Role |
|-------|------|
| `deploy/udev/99-apexos-usb.rules` | On an `APEX-*` block dev: `UDISKS_IGNORE=1` (DE defers) + start the mount service on `add`, run the umount helper on `remove`. |
| `deploy/systemd/apexos-usb-mount@.service` | Oneshot (root) — `ExecStart`/`ExecStop` call the helpers, keyed on the kernel dev name. |
| `deploy/usb/usb-mount` | Own-mount at `<workspace>/media/<label>` with `noexec,nosuid,nodev` + `uid=agentd,gid=agentd` (FAT/exFAT) or mount+chown (other). Hard-confines the mountpoint to `media/`; idempotent; records dev→mountpoint in `/run/apexos-usb/`. |
| `deploy/usb/usb-umount` | Unmount by `<dev>` (udev remove, via the `/run` state) or `--label APEX-…` (eject). Hard-confines + validates the label before touching anything. |
| `deploy/sudoers.d/apexos-usb` | Lets the non-root `agentd` user run **only** `usb-umount` as root (for UI/agent eject). |

`install.sh` installs all of the above (helpers → `/usr/local/lib/apexos/`,
`apexos-workspace-init` → `/usr/local/bin`), validates the sudoers with `visudo -c`,
makes `<workspace>/media`, and reloads udev. Runs on every node (the marker-gate
keeps it safe everywhere).

## Eject

`POST /api/media/eject {label}` (gateway) → validates the label (`valid_exo_label`,
unit-tested: `APEX-*`, sane chars, no `..`) → runs `usb-umount --label` via the narrow
sudoers (argv, never a shell). Surfaced in the Explorer as a **⏏** affordance on each
`media/*` stick row (ui-slint); on success it refreshes the view.

## File operations (the Explorer is a real file manager)

Because an `APEX-*` stick is **GNOME-ignored by design** (the udev `UDISKS_IGNORE`),
the **Explorer (📁 Files) is the on-ApexOS path for moving work on and off the stick**
— so it carries the full verb set, not just read+navigate+preview:

| Verb | Endpoint (gated, confined) | Notes |
|------|----------------------------|-------|
| New folder | `POST /api/workspace/mkdir {path}` | single level under an existing dir |
| Rename | `POST /api/workspace/rename {path, name}` | `name` is one safe component (no `/`, `..`) |
| Delete | `POST /api/workspace/delete {path}` | file or recursive dir; refuses the workspace root |
| Move | `POST /api/workspace/move {src, dest}` | `dest` = target **dir** (keeps basename); cross-device (workspace ⇄ stick) falls back to copy+remove (EXDEV) |
| Copy | `POST /api/workspace/copy {src, dest}` | recursive for a folder |

All five resolve through the **same `resolve_workspace_path` / `resolve_workspace_write_path`**
confinement the read endpoints + agent FS tools use — both ends of a move/copy are
workspace-confined, names are validated (`safe_component`, unit-tested), `..`/absolute
escapes and name collisions are rejected, and a mounted stick's *mountpoint* can't be
deleted (it's busy — eject first). The verbs are UI-only (the agent already has
`write_file`/`delete_path`); no new agent tool or policy rule.

**UI (ui-slint `explorer_view.slint`):** an action row (**+ Folder**, **Paste**) + a
per-row **⋮** menu (Rename / Cut / Copy / Delete). Cut/Copy load a view-local
clipboard; **Paste** drops it into the folder in view (cut → move, copy → copy). New
folder + rename use a name-prompt overlay; delete uses a confirm overlay. A
drive→workspace move is just Cut on a `media/<label>/…` row → navigate → Paste.

## Why the systemd sandbox isn't a problem here

agentd runs under `ProtectSystem=strict` but with **`PrivateMounts=no`** and the host
root mount is **`shared`**, so a mount made on the host under `/var/lib/agentd`
propagates into agentd's namespace — the agent sees the stick with no extra systemd
config. (Verified on apex-3.) `noexec,nosuid,nodev` blanket-protects against an
untrusted FS image; deeper untrusted-filesystem hardening is a post-mk1 item.

## Verify

- **Dev box (no real stick)**: `valid_exo_label` unit test (gateway); the bash helpers
  syntax-check + their validation/confinement paths run (bad label rejected, not-mounted
  no-op); `POST /api/media/eject` rejects bad labels and attempts the helper for good
  ones (gated). A full loopback test: `truncate -s 64M img && mkfs.exfat -L APEX-test img
  && losetup …` then run `usb-mount` (needs root).
- **Field-test (real exFAT stick)**: `apexos-workspace-init` + relabel `APEX-<name>` →
  plug it → it mounts at `workspace/media/APEX-<name>`, shows in Explorer, APEX can
  `read_file`/`write_file` under it, ⏏ unmounts cleanly; a *normal* USB still goes to
  GNOME (untouched). This is the one hardware-gated leg.

## Deferred (follow-on slices)

Phone-handoff (the PWA workspace file-browser leg), an agent-facing `eject_media` tool
+ a plug *notification* into the root session, an embodiment "exo-workspace mounted"
hint, and `apexos-workspace-init` as a one-tap Explorer action.
