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
| `deploy/systemd/apexos-usb-eject.path`/`.service` + `deploy/usb/usb-eject-drain` | Root eject watcher: drains `APEX-<label>` request files via `usb-umount --label` (see *Eject* below). Replaced the removed `deploy/sudoers.d/apexos-usb` (inert under `NoNewPrivileges`). |
| `deploy/systemd/apexos-usb-prep.path`/`.service` + `deploy/usb/usb-prep`(`-drain`) | Root prep watcher for *"Use this drive"*: drains `*.req` files via `usb-prep` (relabel/format → mount → init). |

`install.sh` installs all of the above (helpers → `/usr/local/lib/apexos/`,
`apexos-workspace-init` → `/usr/local/bin`), removes the old sudoers drop-in, makes
`<workspace>/media` + the agentd-owned request dirs, reloads udev, and enables the
eject/prep path units. Runs on every node (the marker-gate keeps it safe everywhere).

## Eject — privilege separation, NOT sudo

agentd runs as the non-root `agentd` user with **`NoNewPrivileges=true`** (systemd
hardening), and that flag makes the kernel **block setuid `sudo` entirely** — so a
sudoers drop-in can *never* let agentd umount (this is why the first eject attempt
failed in the field). The fix is privilege separation, so agentd never escalates at all:

1. `POST /api/media/eject {label}` (gateway) — or the agent `eject_media` tool — validates
   the label (`valid_exo_label`: `APEX-*`, sane chars, no `..`), confirms it's mounted, and
   **drops an empty `APEX-<label>` request file** into the agentd-owned eject dir
   (`AGENTD_USB_EJECT_DIR`, default `/var/lib/agentd/usb-eject` — under `ReadWritePaths`, so
   the non-root daemon can write it).
2. A **path unit** `apexos-usb-eject.path` (root) watches that dir and fires
   `apexos-usb-eject.service` (root oneshot), whose `usb-eject-drain` removes each request
   and runs `usb-umount --label` **as root** — the label re-validated by the drain and a
   third time by `usb-umount`, which hard-confines the target to `<workspace>/media/`.
3. The requester **polls `/proc/mounts`** (≤8s): success when the mountpoint disappears;
   otherwise a clear "still mounted — check `journalctl -u apexos-usb-eject`".

So the only thing agentd ever does is write a file in a directory it owns; the umount
happens entirely in a root service it can't influence beyond the (thrice-validated) label.
The old `deploy/sudoers.d/apexos-usb` is removed (it was inert under `NoNewPrivileges`).
Surfaced in the Explorer as a **⏏** affordance on each `media/*` stick row (ui-slint); on
success it refreshes the view.

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

## The agent knows the stick is there, and can eject it itself

Three pieces close the loop so APEX handles the stick conversationally ("I see APEX-work
is in — want me to pick up the project?" … "want me to eject it now that I've saved the
report?"):

- **Plug notification** — the `usb-mount` helper, right after a successful mount,
  best-effort POSTs `{label}` to `POST /api/media/plugged` (loopback + the shared token
  from agentd's env). The gateway injects a **root-session greeting** so APEX speaks up
  *the moment the stick lands*, rather than waiting for its next turn's embodiment block —
  mirrors the mesh-beacon `MESH_BEACON_NOTIFY_AGENT` pathway. Default ON;
  **`AGENTD_USB_NOTIFY_AGENT=0`** silences it (the embodiment hint still surfaces the
  stick passively). Best-effort: if agentd is down the notify just fails and the mount
  is unaffected. (Re-triggering an already-mounted stick is a no-op, so no duplicate
  greetings.)
- **Embodiment hint** — `build_embodiment` (agentd) adds a line listing the sticks
  mounted under `media/` when any are present: it reads `/proc/mounts` (authoritative —
  a leftover empty mountpoint after eject doesn't show) via the pure, unit-tested
  `parse_exo_sticks`. The line is **byte-stable** when nothing changes, so it's safe in
  the cache-sensitive embodiment block — it only mutates on a real plug/eject (exactly
  when the cache *should* refresh). So the agent wakes already knowing the stick exists,
  its label, and that it's read+write like any workspace folder.
- **`eject_media{label}` tool** (apexos-tools, policy **`allow`**) — the agent's own
  safe-eject: validates the label (`valid_exo_label`, mirrors the gateway + helper) and
  goes through the **same** request-file → root-drain path as the UI ⏏ (see *Eject* above;
  agentd can't sudo under `NoNewPrivileges`). Non-destructive (flush + unmount;
  re-pluggable), confined to `APEX-*` sticks. `allow` because the conversational "want me
  to eject it?" *is* the confirmation — a second
  approval card would be clunky. The `eject_media = "allow"` rule is seeded in
  `config/policy.toml` and **reaches already-deployed nodes via install.sh's additive
  `sync_policy_rules` on the next `apexos-update`** (policy is seed-or-additively-sync
  since 2026-07-04); a node not yet updated gates it as `unknown → ask`.

## "Use this drive" — adopt a stick as an exo-workspace (no CLI)

So a non-technical user can dedicate a USB stick without `exfatlabel`/`gparded`/CLI, the
Explorer offers a one-tap **"Use this drive"** (a stick → an `APEX-<name>` exo-workspace).
Two behaviours (the user chooses):

- **Relabel (default, keeps files)** — renames the stick's volume to `APEX-<name>`, writes
  the marker + `projects/data/notes`, mounts it. **Non-destructive** — existing files
  stay; a wrong pick just renames a volume (recoverable). Works on an already-formatted
  FAT/exFAT stick (the common case).
- **Format (wipe)** — *slice B* — wipes the stick to a clean exFAT `APEX-<name>` (handles a
  blank/RAW or non-FAT stick), behind an explicit erase-confirm.

**Privilege-separated, same as the eject** (agentd can't touch block devices under
`NoNewPrivileges`):

1. `GET /api/media/candidates?mode=relabel|format` lists the prep-able sticks — the
   **pure, unit-tested** `parse_prep_candidates` over `lsblk -J`, always restricted to a
   **USB-transport** disk that is **not** the system disk and **not** the active `media/`
   mount. `relabel` offers only FAT/exFAT that isn't already `APEX-*` (so a system/NVMe
   disk *and* an ext4 data drive never appear); `format` offers the broader wipeable set
   (any fstype, incl. blank). This decides what to *offer*.
2. `POST /api/media/prep {dev, name, mode}` validates (incl. the **≤6-char name** — exFAT/
   FAT labels cap at 11, and `APEX-` eats 5), then **drops a 3-line `*.req`**
   (`mode`/`dev`/`name`) into the agentd-owned `AGENTD_USB_PREP_DIR` (`/var/lib/agentd/usb-prep`).
3. The root `apexos-usb-prep.path`→`.service` drain runs **`usb-prep`** — **the security
   boundary**: it independently re-validates the device (USB-transport, and **never** the
   disk holding `/` or `/boot`) before unmount → **relabel** (`exfatlabel`/`fatlabel`, keeps
   files) **or format** (`wipefs` + `mkfs.exfat`, erases) → `usb-mount` →
   `apexos-workspace-init`. agentd's offer-list is convenience; this gate is what protects.
4. The requester **polls `/proc/mounts`** for the new `media/APEX-<name>` mount (≤25s).
   On success the existing plug-notification fires → APEX greets the freshly-adopted stick.

**UI (ui-slint `explorer_view.slint`):** a full-width **🔌 Use a USB drive** button in the
Explorer action row opens a picker modal with a **Keep my files / Erase & format** toggle
(switching re-`drive-scan`s — the wipeable set is broader). Radio-select a candidate
(model · size · label/fstype/blank), name it (→ `APEX-<name>`, ≤6 chars), then:
**SET UP DRIVE** (relabel) preps directly, **ERASE & SET UP** (format) goes through a
red **erase-confirm overlay** naming the exact device first. A *Setting up…* busy state
covers the ≤25s prep; Rust drives `drive-busy`/`drive-result` and the view auto-closes on
`"ok"` (a `changed drive-result` handler) then hops to `media/`. Candidate list + busy/
result are Rust-fed; the open/mode/selection/name form is view-local.

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

## Status — the loop is closed

The USB exo-workspace is feature-complete: the Explorer **file verbs**, the **`eject_media`**
agent tool, the embodiment **"stick mounted" hint**, the **privilege-separated eject**, the
**plug notification**, **"Use this drive"** (relabel + format, which subsumed the old
"`apexos-workspace-init` as an Explorer action" idea), and the **phone-handoff PWA file
browser** (browse / upload / download — `docs/web-ui.md`, so a phone reaches a mounted
stick's files). Remaining work is field validation on real sticks (the one hardware-gated
leg, like the Pi-only items) — not new features.
