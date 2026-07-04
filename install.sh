#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════════════╗
# ║              ApexOS-RS — One-shot installer                              ║
# ║  Pure Rust: agentd · cerebro-mcp · apexos-tools · ui-slint              ║
# ║  Runs on: Pi Zero 2W → Pi 5 → x86 mini-PC → GPU workstation             ║
# ╚══════════════════════════════════════════════════════════════════════════╝
#
# Quick install — UNATTENDED (recommended for fresh devices):
#   curl -fsSL https://raw.githubusercontent.com/buckster123/ApexOS-RS/main/install.sh | sudo bash
#   A piped install has no keyboard, so it runs fully hands-free: auto-detects the
#   hardware tier/mode and reads the API key + optional settings from a file named
#   apexos.env (or apexos.conf) dropped on a USB stick or the SD card's boot
#   partition. No menus to hang on. See "boot/USB provisioning" below.
#
# Interactive menus (TUI) — run from a real terminal, not a pipe:
#   curl -fsSLO https://raw.githubusercontent.com/buckster123/ApexOS-RS/main/install.sh
#   sudo bash install.sh                 # download-then-run → whiptail menus work
#   ...or force the TUI even when piped:  ... | sudo bash -s -- --tui
#
# Boot/USB provisioning file (apexos.env / apexos.conf), any of these lines:
#   ANTHROPIC_API_KEY=sk-ant-...   (or just a bare sk-ant-... on its own line)
#   OPENROUTER_API_KEY=...
#
# API-key handling: a key is discovered from (in precedence) --api-key, a boot/USB
# apexos.env, an exported $ANTHROPIC_API_KEY (best-effort — sudo resets env), or
# typed in. Whatever the source, it's surfaced (masked + where it came from) and
# LIVE-VERIFIED against Anthropic (GET /v1/models — auth-only, costs nothing; an
# out-of-credits-but-valid key still passes). A rejected (invalid/revoked) key
# re-prompts interactively, or is kept-with-a-loud-warning under --yes. Offline →
# can't verify, kept with a note. An existing /etc/agentd/env key is preserved +
# re-verified on a key-less re-run.
#   APEXOS_MODE=kiosk|headless|desktop      APEXOS_TIER=nano|micro|standard|pro
#   APEXOS_NO_UI=1   APEXOS_NO_SENSOR=0   APEXOS_NO_CEREBRO_API=0   APEXOS_VOICE=1
#
# Idempotency: the resolved choices are saved to /etc/agentd/install.conf and
# restored on every re-run, so `apexos-update` (a no-flag, no-USB re-run) keeps the
# same deployment shape instead of re-auto-detecting (no headless→kiosk flip).
# Precedence: CLI flag > USB apexos.conf > install.conf > auto-detect. Change a
# node's shape with a flag, a fresh USB file, or by deleting install.conf.
#
# Options:
#   -y / --yes              Force unattended (also implied when stdin isn't a TTY)
#   --tui                   Force the interactive menus even under curl|bash
#   --no-ui                 Skip apexos-rs-ui (headless / server mode)
#   --no-cerebro-api        Skip cerebro-api REST dashboard
#   --no-sensor             Skip apex-sensor-bridge (no sensorhead attached)
#   --no-occipital          Skip the Occipital web cortex (clone + build of the sibling repo)
#   --no-voice              Skip whisper + piper wake-word
#   --api-key=KEY           Set ANTHROPIC_API_KEY non-interactively
#   --openrouter-key=KEY    Set OPENROUTER_API_KEY
#   --tier=TIER             nano | micro | standard | pro (default: auto-detect)
#   --mode=MODE             kiosk | headless | desktop (default: auto)
#   --repo-dir=PATH         Use a local clone instead of fetching from GitHub

set -euo pipefail

# ── Log setup ──────────────────────────────────────────────────────────────────
LOG=/var/log/apexos-install.log
if mkdir -p "$(dirname "$LOG")" 2>/dev/null && touch "$LOG" 2>/dev/null; then
  exec > >(tee -a "$LOG") 2>&1
else
  LOG=/tmp/apexos-install.log
  exec > >(tee -a "$LOG") 2>&1
fi
echo "── ApexOS-RS install started $(date) ──"

# On failure: restore the terminal (whiptail may have left it in raw mode) then report.
trap 'stty sane </dev/tty 2>/dev/null || true; echo -e "\n${RED:-}  ✗ Install failed at line $LINENO — see $LOG${NC:-}" >&2' ERR

# ── Colours ────────────────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
  RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
  CYAN='\033[0;36m'; BOLD='\033[1m'; DIM='\033[2m'; NC='\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; DIM=''; NC=''
fi

ok()   { echo -e "${GREEN}  ✓${NC} $*"; }
info() { echo -e "${CYAN}  →${NC} $*"; }
warn() { echo -e "${YELLOW}  ⚠${NC} $*"; }
die()  { echo -e "${RED}  ✗${NC} $*" >&2; exit 1; }
hdr()  { echo -e "\n${CYAN}${BOLD}━━━  $*  ━━━${NC}\n"; }

# Ensure the handful of tools needed to even fetch + clone the repo. A freshly
# imaged Pi OS Lite has neither git nor (sometimes) curl, and the clone below runs
# BEFORE the main apt step — so without this, a one-shot `curl … | sudo bash` on a
# never-updated device dies on `git clone`. Installs only what is missing.
ensure_bootstrap_deps() {
  local need=()
  command -v git  &>/dev/null || need+=(git)
  command -v curl &>/dev/null || need+=(curl)
  [[ -e /etc/ssl/certs/ca-certificates.crt ]] || need+=(ca-certificates)
  (( ${#need[@]} )) || return 0
  info "Installing bootstrap tools: ${need[*]}"
  apt-get update -qq
  apt-get install -y --no-install-recommends "${need[@]}" >/dev/null \
    || die "Could not install ${need[*]} — check network/DNS and try again."
}

# ── Key-file discovery (USB stick / SD boot partition) ─────────────────────────
# Anthropic keys are ~100 chars of "alien glyphs" — typing them into a TUI is a
# typo nightmare. So we look for a pre-written key/env file on removable media
# first. Filenames we accept (case-insensitive), at the root or one dir deep:
KEYFILE_NAMES=(apexos.env apexos.conf apexos-rs.env agentd.env apex.env apexos.txt apexos-key.txt env.txt)

# Set by find_key_file on success — keys plus optional install settings:
FOUND_ANTHROPIC=""; FOUND_OPENROUTER=""; FOUND_KEY_SRC=""
FOUND_MODE=""; FOUND_TIER=""; FOUND_NO_UI=""; FOUND_NO_SENSOR=""
FOUND_NO_CEREBRO_API=""; FOUND_VOICE=""; FOUND_NO_OCCIPITAL=""

# Resolved-choices record: written at the end of a successful install and restored
# on every re-run (load_persisted_config) so `apexos-update` keeps the same
# deployment shape instead of re-auto-detecting (which flips a headless node →
# kiosk). Lower precedence than a CLI flag or a freshly-plugged USB provisioning
# file, higher than auto-detect.
CONF_FILE=/etc/agentd/install.conf

# Echo the value of KEY= in an env-style file (handles `export `, quotes, CRLF).
_envval() {
  grep -aE "^[[:space:]]*(export[[:space:]]+)?$2=" "$1" 2>/dev/null \
    | head -1 | sed -E 's/^[^=]*=//; s/^["'"'"']//; s/["'"'"']$//' | tr -d ' \r' || true
}

# Truthy test for config flags: yes/y/1/true/on (case-insensitive).
_truthy() { [[ "$1" =~ ^([Yy]([Ee][Ss])?|1|[Tt][Rr][Uu][Ee]|[Oo][Nn])$ ]]; }

# Mask an API key for display — enough to recognize, never enough to leak.
mask_key() {
  local k="$1"
  if [[ ${#k} -le 14 ]]; then printf 'sk-ant-…'; else printf '%s…%s' "${k:0:11}" "${k: -4}"; fi
}

# Live-verify an Anthropic key WITHOUT spending a cent: GET /v1/models is auth-
# gated but free (listing models consumes no tokens), so it confirms the KEY is
# real regardless of credit balance — a valid-but-out-of-credits key still lists
# models (200). 401/403 = invalid or revoked. No network / server error = we
# simply can't verify (an offline-first install is legitimate). Convenience worth
# keeping (auto-discovery from the operator's existing env) without the dud-key
# footgun. Echoes: valid | invalid | offline.
check_anthropic_key() {
  local key="$1" code
  code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 12 \
    https://api.anthropic.com/v1/models \
    -H "x-api-key: ${key}" -H "anthropic-version: 2023-06-01" 2>/dev/null || true)
  case "$code" in
    200|429) echo valid   ;;   # 200 OK; 429 = valid key, just rate-limited
    401|403) echo invalid ;;   # authentication_error / permission denied
    *)       echo offline ;;   # '', 000 (timeout), 5xx, anything unexpected
  esac
}

# Surface a resolved key (masked + where it came from) and verify it live.
# Returns 0 when it's safe to use (valid, or unverifiable-offline); 1 when
# Anthropic rejected it, so the caller can re-prompt / warn.
vet_key() {
  local key="$1" src="${2:-provided}"
  info "Anthropic key $(mask_key "$key")  (from: ${src})"
  VET_RESULT="$(check_anthropic_key "$key")"   # global — finalize_key_check reads it
  case "$VET_RESULT" in
    valid)   ok   "Key verified with Anthropic ✓"; return 0 ;;
    offline) warn "Couldn't reach Anthropic to verify the key (offline?). Keeping it."; return 0 ;;
    *)       warn "Anthropic REJECTED this key — invalid or revoked ✗"; return 1 ;;
  esac
}

# Vet the currently-resolved $API_KEY and route the outcome: a rejected key is
# CLEARED in interactive mode (so the key menu re-prompts) but KEPT-with-warning
# under --yes (don't silently drop an operator's explicit unattended choice).
# Valid / unverifiable-offline → kept. No-op when no key is set.
finalize_key_check() {
  [[ -z "${API_KEY:-}" ]] && return 0
  if vet_key "$API_KEY" "${API_KEY_SRC:-provided}"; then
    if ! ${YES:-false}; then
      if [[ "${VET_RESULT:-}" == valid ]]; then
        tui_msg "Key verified ✓" \
          "Anthropic confirmed your key:\n\n  $(mask_key "$API_KEY")\n  from: ${API_KEY_SRC:-provided}"
      else
        tui_msg "Key not verified" \
          "Couldn't reach Anthropic to check this key (no internet yet?):\n\n  $(mask_key "$API_KEY")\n\nKeeping it — make sure it's valid before starting agentd."
      fi
    fi
    return 0
  fi
  if ${YES:-false}; then
    warn "Continuing with the rejected key (unattended). Update ANTHROPIC_API_KEY in /etc/agentd/env, then restart agentd."
    return 0
  fi
  tui_msg "Key rejected ✗" \
    "Anthropic rejected this key (invalid or revoked):\n\n  $(mask_key "$API_KEY")\n  from: ${API_KEY_SRC:-provided}\n\nLet's pick another one."
  API_KEY=""; API_KEY_SRC=""
  return 1
}

# Parse a provisioning file: API keys (or a bare sk-ant-… key) plus optional
# APEXOS_* install settings. Returns 0 if it carried anything we can use.
_parse_key_file() {
  local f="$1"
  FOUND_ANTHROPIC=$(_envval "$f" ANTHROPIC_API_KEY)
  FOUND_OPENROUTER=$(_envval "$f" OPENROUTER_API_KEY)
  if [[ -z "$FOUND_ANTHROPIC" ]]; then
    FOUND_ANTHROPIC=$(grep -aoE 'sk-ant-[A-Za-z0-9_-]+' "$f" 2>/dev/null | head -1 || true)
  fi
  FOUND_MODE=$(_envval "$f" APEXOS_MODE)
  FOUND_TIER=$(_envval "$f" APEXOS_TIER)
  FOUND_NO_UI=$(_envval "$f" APEXOS_NO_UI)
  FOUND_NO_SENSOR=$(_envval "$f" APEXOS_NO_SENSOR)
  FOUND_NO_CEREBRO_API=$(_envval "$f" APEXOS_NO_CEREBRO_API)
  FOUND_VOICE=$(_envval "$f" APEXOS_VOICE)
  FOUND_NO_OCCIPITAL=$(_envval "$f" APEXOS_NO_OCCIPITAL)
  [[ -n "${FOUND_ANTHROPIC}${FOUND_OPENROUTER}${FOUND_MODE}${FOUND_TIER}${FOUND_NO_UI}${FOUND_NO_SENSOR}${FOUND_NO_CEREBRO_API}${FOUND_VOICE}${FOUND_NO_OCCIPITAL}" ]]
}

# Scan mounted media + the SD boot partition, then probe UNmounted removable
# partitions (fresh Pi OS Lite doesn't auto-mount USB). Sets FOUND_* on success.
find_key_file() {
  FOUND_ANTHROPIC=""; FOUND_OPENROUTER=""; FOUND_KEY_SRC=""
  local dir name f

  # 1) Already-mounted removable media + the FAT boot partition (mounts on any PC).
  local search_dirs=(/boot/firmware /boot /media /mnt /run/media)
  while IFS= read -r dir; do search_dirs+=("$dir"); done < <(
    awk '$2 ~ /^\/(media|mnt|run\/media)/ {print $2}' /proc/mounts 2>/dev/null || true)

  for dir in "${search_dirs[@]}"; do
    [[ -d "$dir" ]] || continue
    for name in "${KEYFILE_NAMES[@]}"; do
      while IFS= read -r f; do
        [[ -f "$f" ]] || continue
        if _parse_key_file "$f"; then FOUND_KEY_SRC="$f"; return 0; fi
      done < <(find "$dir" -maxdepth 2 -iname "$name" -type f 2>/dev/null || true)
    done
  done

  # 2) Probe unmounted removable partitions read-only.
  local mp=/run/apexos-usb
  mkdir -p "$mp"
  local line NAME FSTYPE RM TYPE
  while IFS= read -r line; do
    eval "$line"                       # sets NAME FSTYPE RM TYPE from lsblk -P
    [[ "${TYPE:-}" == "part" ]]   || continue
    [[ "${RM:-0}" == "1" ]]       || continue
    [[ -n "${FSTYPE:-}" ]]        || continue
    if findmnt -nS "$NAME" &>/dev/null; then continue; fi
    mount -o ro "$NAME" "$mp" 2>/dev/null || continue
    for name in "${KEYFILE_NAMES[@]}"; do
      while IFS= read -r f; do
        [[ -f "$f" ]] || continue
        if _parse_key_file "$f"; then
          FOUND_KEY_SRC="${NAME} → $(basename "$f")"
          umount "$mp" 2>/dev/null || true
          rmdir "$mp" 2>/dev/null || true
          return 0
        fi
      done < <(find "$mp" -maxdepth 2 -iname "$name" -type f 2>/dev/null || true)
    done
    umount "$mp" 2>/dev/null || true
  done < <(lsblk -Ppo NAME,FSTYPE,RM,TYPE 2>/dev/null || true)
  rmdir "$mp" 2>/dev/null || true
  return 1
}

# Config-first provisioning: load key + optional settings from a boot/USB file so a
# fully unattended install can be configured by dropping one file on the boot
# partition from any PC. Precedence: explicit CLI flags > boot file > auto-detect.
load_boot_provisioning() {
  find_key_file || return 0          # nothing on removable media — fine
  [[ -z "$API_KEY"        && -n "$FOUND_ANTHROPIC"  ]] && { API_KEY="$FOUND_ANTHROPIC"; API_KEY_SRC="boot/USB: ${FOUND_KEY_SRC}"; }
  [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
  # A freshly-plugged USB file overrides a stored install.conf (deliberate re-
  # provisioning) but never a CLI flag — so gate on the *_CLI provenance markers,
  # not the "auto" sentinel (install.conf may already have filled MODE/TIER).
  [[ -n "$FOUND_TIER" ]] && ! $TIER_CLI && TIER="$FOUND_TIER"
  [[ -n "$FOUND_MODE" ]] && ! $MODE_CLI && MODE="$FOUND_MODE"
  [[ -n "$FOUND_NO_UI"          ]] && ! $NO_UI_CLI          && { _truthy "$FOUND_NO_UI"          && NO_UI=true          || NO_UI=false; }
  [[ -n "$FOUND_NO_SENSOR"      ]] && ! $NO_SENSOR_CLI      && { _truthy "$FOUND_NO_SENSOR"      && NO_SENSOR=true      || NO_SENSOR=false; }
  [[ -n "$FOUND_NO_CEREBRO_API" ]] && ! $NO_CEREBRO_API_CLI && { _truthy "$FOUND_NO_CEREBRO_API" && NO_CEREBRO_API=true || NO_CEREBRO_API=false; }
  [[ -n "$FOUND_NO_OCCIPITAL"   ]] && ! $NO_OCCIPITAL_CLI   && { _truthy "$FOUND_NO_OCCIPITAL"   && NO_OCCIPITAL=true   || NO_OCCIPITAL=false; }
  [[ -n "$FOUND_VOICE"          ]] && ! $NO_VOICE_CLI       && { _truthy "$FOUND_VOICE"          && NO_VOICE=false      || NO_VOICE=true; }
  local what="settings"; [[ -n "$FOUND_ANTHROPIC" ]] && what="key + settings"
  ok "Provisioned from ${FOUND_KEY_SRC} ($what)"
}

# Restore the resolved choices from a prior install so a re-run (`apexos-update`,
# which re-invokes this script with no flags and no USB) keeps the same deployment
# shape rather than re-auto-detecting — the bug that flipped a headless Pi → kiosk
# and a manually-enabled sensor head back off. Only fills knobs the user did not set
# on the command line; a freshly-plugged USB file (load_boot_provisioning, runs
# after) still overrides this. Precedence: CLI > USB file > install.conf > auto.
load_persisted_config() {
  if [[ ! -f "$CONF_FILE" ]]; then
    # No record yet. Either a truly fresh install (agentd not present → let auto-
    # detect run) OR a node deployed before install.conf existed — the first
    # `apexos-update` carrying this fix. For the latter, infer the live UI/API shape
    # from enabled services so THIS run is already idempotent (it persists below),
    # instead of re-auto-detecting and flipping a headless Pi → kiosk. The kiosk vs
    # headless axis is exactly what the old detect flipped; tiers re-detect harmlessly.
    if [[ -x /usr/local/bin/agentd ]]; then
      [[ "$MODE" == "auto" ]] && ! $MODE_CLI && { systemctl is-enabled apexos-rs-ui &>/dev/null && MODE="kiosk" || MODE="headless"; }
      ! $NO_CEREBRO_API_CLI && { systemctl is-enabled cerebro-api &>/dev/null && NO_CEREBRO_API=false || NO_CEREBRO_API=true; }
      info "No install.conf — inferred existing shape from services (mode=$MODE)"
    fi
    return 0
  fi
  local c_mode c_tier c_no_ui c_no_sensor c_no_api c_voice c_no_occipital
  c_mode=$(_envval "$CONF_FILE" APEXOS_MODE)
  c_tier=$(_envval "$CONF_FILE" APEXOS_TIER)
  c_no_ui=$(_envval "$CONF_FILE" APEXOS_NO_UI)
  c_no_sensor=$(_envval "$CONF_FILE" APEXOS_NO_SENSOR)
  c_no_api=$(_envval "$CONF_FILE" APEXOS_NO_CEREBRO_API)
  c_voice=$(_envval "$CONF_FILE" APEXOS_VOICE)
  c_no_occipital=$(_envval "$CONF_FILE" APEXOS_NO_OCCIPITAL)
  [[ -n "$c_mode" ]] && ! $MODE_CLI && MODE="$c_mode"
  [[ -n "$c_tier" ]] && ! $TIER_CLI && TIER="$c_tier"
  [[ -n "$c_no_ui"     ]] && ! $NO_UI_CLI          && { _truthy "$c_no_ui"     && NO_UI=true          || NO_UI=false; }
  [[ -n "$c_no_sensor" ]] && ! $NO_SENSOR_CLI      && { _truthy "$c_no_sensor" && NO_SENSOR=true      || NO_SENSOR=false; }
  [[ -n "$c_no_api"    ]] && ! $NO_CEREBRO_API_CLI && { _truthy "$c_no_api"    && NO_CEREBRO_API=true || NO_CEREBRO_API=false; }
  [[ -n "$c_no_occipital" ]] && ! $NO_OCCIPITAL_CLI && { _truthy "$c_no_occipital" && NO_OCCIPITAL=true || NO_OCCIPITAL=false; }
  [[ -n "$c_voice"     ]] && ! $NO_VOICE_CLI       && { _truthy "$c_voice"     && NO_VOICE=false      || NO_VOICE=true; }
  ok "Restored install choices from $CONF_FILE (mode=$MODE tier=$TIER)"
}

# ── Args ───────────────────────────────────────────────────────────────────────
YES=false; TUI_FORCE=false
# Sensor head OFF by default (most devices have no BME688/MLX90640 attached); a
# boot-file APEXOS_NO_SENSOR=false or the manual checklist turns it on.
NO_UI=false; NO_CEREBRO_API=false; NO_SENSOR=true; NO_VOICE=true
# Occipital (web reading cortex) defaults ON — a fresh install clones + builds the
# sibling repo and registers occipital-mcp. Skip with --no-occipital / a boot-file
# APEXOS_NO_OCCIPITAL=1. (OCC_FEATURES/OCCIPITAL_INSTALLED initialised for `set -u`.)
NO_OCCIPITAL=false; OCC_FEATURES=""; OCCIPITAL_INSTALLED=false
API_KEY=""; OPENROUTER_KEY=""; API_KEY_SRC=""
TIER="auto"; MODE="auto"; REPO_DIR=""
IS_DESKTOP=false   # MODE==desktop → build the UI but launch a winit window, not the kiosk service

# Provenance markers — which knobs were set explicitly on the command line. A CLI
# flag always wins; stored install.conf and USB provisioning only fill knobs the
# user did NOT pass. (Precedence: CLI > USB file > install.conf > auto-detect.)
MODE_CLI=false; TIER_CLI=false
NO_UI_CLI=false; NO_CEREBRO_API_CLI=false; NO_SENSOR_CLI=false; NO_VOICE_CLI=false
NO_OCCIPITAL_CLI=false

for arg in "$@"; do
  case "$arg" in
    -y|--yes)              YES=true ;;
    --tui)                 TUI_FORCE=true ;;
    --no-ui)               NO_UI=true; NO_UI_CLI=true ;;
    --no-cerebro-api)      NO_CEREBRO_API=true; NO_CEREBRO_API_CLI=true ;;
    --no-sensor)           NO_SENSOR=true; NO_SENSOR_CLI=true ;;
    --no-occipital)        NO_OCCIPITAL=true; NO_OCCIPITAL_CLI=true ;;
    --no-voice)            NO_VOICE=true;  NO_VOICE_CLI=true ;;
    --voice)               NO_VOICE=false; NO_VOICE_CLI=true ;;
    --api-key=*)           API_KEY="${arg#*=}"; API_KEY_SRC="--api-key flag" ;;
    --openrouter-key=*)    OPENROUTER_KEY="${arg#*=}" ;;
    --tier=*)              TIER="${arg#*=}"; TIER_CLI=true ;;
    --mode=*)              MODE="${arg#*=}"; MODE_CLI=true ;;
    --repo-dir=*)          REPO_DIR="${arg#*=}" ;;
    *) warn "unknown option: $arg" ;;
  esac
done

[[ $EUID -eq 0 ]] || die "Run as root: sudo bash install.sh"

# Resolve the unprivileged build user (the rustup toolchain owner). Normally the
# invoking sudo user; when invoked as bare root (e.g. a UI "Update" button, or
# `apexos-update` run as root) fall back to the owner of an existing clone so we
# never try to build as root (root has no rustup toolchain).
BUILD_USER="${SUDO_USER:-}"
if [[ -z "$BUILD_USER" || "$BUILD_USER" == "root" ]] && [[ -d /opt/ApexOS-RS ]]; then
  BUILD_USER=$(stat -c '%U' /opt/ApexOS-RS 2>/dev/null || echo "")
fi
[[ -n "$BUILD_USER" ]] || BUILD_USER=root
BUILD_HOME=$(getent passwd "$BUILD_USER" | cut -d: -f6)

# ── Interactive or unattended? ─────────────────────────────────────────────────
# `curl … | sudo bash` has no keyboard on stdin (it's the script pipe), so an
# interactive TUI there just renders a box that can never read a keypress and
# hangs the console in raw mode. The robust default: if stdin isn't a real
# terminal (or --yes was passed), run FULLY unattended — auto-detect + boot/USB
# file, no prompts. A real terminal (download-then-run) gets the TUI; --tui forces
# it even when piped (uses </dev/tty for the keyboard).
if ! $TUI_FORCE && { $YES || ! [ -t 0 ]; }; then
  YES=true
fi

# ── TUI helpers (whiptail with plain-text fallback) ────────────────────────────
HAVE_WHIPTAIL=false
if ! $YES; then
  if ! command -v whiptail &>/dev/null; then
    # Install whiptail silently — it's tiny and on every Debian mirror
    apt-get update -qq && apt-get install -y --no-install-recommends whiptail &>/dev/null && \
      HAVE_WHIPTAIL=true || true
  else
    HAVE_WHIPTAIL=true
  fi
fi

# All TUI calls route through these wrappers.
# whiptail draws on /dev/tty (unaffected by the exec-tee on fd1/fd2) AND reads the
# keyboard from /dev/tty — critical under `curl … | sudo bash`, where the script's
# stdin is the pipe, not the terminal. Without `</dev/tty` whiptail renders the box
# but can never read a keypress, leaving the console in raw mode (^[[A on arrows).
H=20; W=72   # default dialog height/width

# Show a message box. $1=title $2=body
tui_msg() {
  if $HAVE_WHIPTAIL; then
    whiptail --title "$1" --msgbox "$2" $H $W 3>&1 1>/dev/tty 2>&3 </dev/tty || true
  else
    hdr "$1"; echo -e "$2"; echo
  fi
}

# Ask yes/no. Returns 0=yes 1=no. $1=title $2=body
tui_yesno() {
  if $HAVE_WHIPTAIL; then
    whiptail --title "$1" --yesno "$2" $H $W 3>&1 1>/dev/tty 2>&3 </dev/tty; return $?
  else
    hdr "$1"; echo -e "$2"
    local reply; read -r reply </dev/tty
    [[ "${reply:-y}" =~ ^[Yy]$ ]] && return 0 || return 1
  fi
}

# Single-select menu. Echoes selected tag. $1=title $2=prompt $3...=tag desc pairs
tui_menu() {
  local title="$1" prompt="$2"; shift 2
  if $HAVE_WHIPTAIL; then
    local choice
    choice=$(whiptail --title "$title" --menu "$prompt" $H $W $(( ($# / 2) + 1 )) "$@" \
      3>&1 1>/dev/tty 2>&3 </dev/tty) || true
    echo "$choice"
  else
    hdr "$title"; echo -e "$prompt"
    local i=1
    while [[ $# -ge 2 ]]; do
      echo "  $i) $2 ($1)"; shift 2; ((i++))
    done
    local reply; read -r reply </dev/tty; echo "${reply:-1}"
  fi
}

# Multi-select checklist. Echoes space-separated quoted tags. $1=title $2=prompt $3...=tag desc state triples
tui_checklist() {
  local title="$1" prompt="$2"; shift 2
  if $HAVE_WHIPTAIL; then
    whiptail --title "$title" --checklist "$prompt" $H $W $(( ($# / 3) + 1 )) "$@" \
      3>&1 1>/dev/tty 2>&3 </dev/tty || true
  else
    hdr "$title"; echo -e "$prompt"
    # Print items, auto-accept all ON items
    local result=""
    while [[ $# -ge 3 ]]; do
      local tag="$1" desc="$2" state="$3"; shift 3
      echo "  [$( [[ "$state" == "ON" ]] && echo "x" || echo " " )] $desc ($tag)"
      [[ "$state" == "ON" ]] && result="$result \"$tag\""
    done
    echo "  (auto-accepting defaults)"
    echo "$result"
  fi
}

# Password/text input. Echoes entered value. $1=title $2=prompt $3=type(text|password)
tui_input() {
  local title="$1" prompt="$2" type="${3:-text}"
  if $HAVE_WHIPTAIL; then
    local flag="--inputbox"
    [[ "$type" == "password" ]] && flag="--passwordbox"
    whiptail --title "$title" $flag "$prompt" 10 $W "" \
      3>&1 1>/dev/tty 2>&3 </dev/tty || true
  else
    echo -e "${BOLD}  ?${NC} $prompt"
    if [[ "$type" == "password" ]]; then
      local val; read -rs val </dev/tty; echo; echo "$val"
    else
      local val; read -r val </dev/tty; echo "$val"
    fi
  fi
}

# ── Banner ─────────────────────────────────────────────────────────────────────
echo ""
echo -e "${CYAN}${BOLD}"
echo "  ██████╗ ███████╗       ██████╗ ███████╗"
echo "  ██╔══██╗██╔════╝      ██╔══██╗██╔════╝"
echo "  ███████║███████╗ ████╗ ██████╔╝███████╗"
echo "  ██╔══██║╚════██║       ██╔══██╗╚════██║"
echo "  ██║  ██║███████║       ██║  ██║███████║"
echo "  ╚═╝  ╚═╝╚══════╝       ╚═╝  ╚═╝╚══════╝"
echo -e "${NC}"
echo -e "${DIM}  Pure-Rust agent OS — Pi Zero 2W → Titan${NC}"
echo ""

# ── Hardware auto-detect ───────────────────────────────────────────────────────
ARCH=$(uname -m)
RAM_MB=$(awk '/MemTotal/ { printf "%d", $2/1024 }' /proc/meminfo)
IS_PI=false
PI_MODEL="Unknown"
if [[ -f /proc/device-tree/model ]]; then
  PI_MODEL=$(tr -d '\0' < /proc/device-tree/model 2>/dev/null || echo "Unknown")
  echo "$PI_MODEL" | grep -qi "raspberry" && IS_PI=true
fi

# Restore prior resolved choices first (so apexos-update is idempotent — it won't
# re-auto-detect and flip the deployment shape), THEN let a freshly-plugged boot/USB
# file override them, THEN auto-detect fills any remaining gaps below.
# Precedence: CLI flags > USB provisioning file > /etc/agentd/install.conf > auto.
load_persisted_config
load_boot_provisioning

if [[ "$TIER" == "auto" ]]; then
  if   (( RAM_MB <  768 )); then TIER="nano"
  elif (( RAM_MB < 2048 )); then TIER="micro"
  elif (( RAM_MB < 8192 )); then TIER="standard"
  else                           TIER="pro"
  fi
fi

if [[ "$MODE" == "auto" ]]; then
  $IS_PI && MODE="kiosk" || MODE="headless"
fi

TIER_DESC=""
case "$TIER" in
  nano)     TIER_DESC="Nano — FTS5 search only, ~23 MB RSS (no embeddings)" ;;
  micro)    TIER_DESC="Micro — bge-small embeddings, ~275 MB RSS" ;;
  standard) TIER_DESC="Standard — bge-small + local 7–13B models via Ollama" ;;
  pro)      TIER_DESC="Pro — bge-small + 30–70B local models (GPU)" ;;
esac

# ── TUI: Install style — the first thing anyone sees ──────────────────────────
# "auto"   = detect everything, ask only for the API key (the one unavoidable step)
# "manual" = the full picker flow (mode / components / tier / keys)
STYLE="auto"
if ! $YES; then
  STYLE=$(tui_menu "ApexOS-RS  ·  Welcome aboard 🚀" \
"Found this device:

  $PI_MODEL
  $ARCH  ·  ${RAM_MB} MB RAM  ·  tier '$TIER'  ·  '$MODE' mode

How would you like to set it up?" \
    "auto"   "Automatic — recommended, I'll handle everything" \
    "manual" "Manual    — let me pick mode, components & tier")
  [[ -z "$STYLE" ]] && STYLE="auto"
fi
# (Sensor head defaults OFF globally — see NO_SENSOR default — so a boot-file
#  APEXOS_NO_SENSOR=false or the manual checklist can switch it on; nothing here
#  clobbers a provisioned choice.)

# ── TUI: Welcome detail (manual only) ─────────────────────────────────────────
if ! $YES && [[ "$STYLE" == "manual" ]]; then
  WELCOME_MSG="Welcome to the ApexOS-RS installer.

This will install the pure-Rust agent OS on your device:
  • agentd       — AI agent daemon + WebSocket gateway
  • cerebro-mcp  — cognitive memory ($(echo "$TIER_DESC" | cut -d— -f1 | xargs))
  • apexos-tools — system tool plugins
  • apexos-rs-ui — native Slint UI (kiosk KMS/DRM, or desktop winit window)

Detected device:
  Model : $PI_MODEL
  Arch  : $ARCH
  RAM   : ${RAM_MB} MB
  Tier  : $TIER_DESC
  Mode  : $MODE

The build takes ~8 min on Pi 5, ~2 min on x86.
Full log → $LOG"
  tui_msg "ApexOS-RS Installer" "$WELCOME_MSG"
fi

# ── TUI: Mode picker (manual only — auto keeps the detected mode) ─────────────
if ! $YES && [[ "$STYLE" == "manual" ]]; then
  MODE_CHOICE=$(tui_menu "Deployment Mode" \
    "How will you use this device?" \
    "kiosk"    "Kiosk    — dedicated HDMI display (Pi + monitor)" \
    "headless" "Headless — browser/mobile UI, no local display" \
    "desktop"  "Desktop  — shared monitor, native window (x86/Mac)")
  [[ -n "$MODE_CHOICE" ]] && MODE="$MODE_CHOICE"
fi
# Headless = no local UI. Desktop = build the UI but run it as a winit window in the
# user's session (app-menu + autostart launcher), NOT the root KMS/DRM kiosk service.
# Slice-3e auth means it shows a login screen → no token plumbing needed in the launcher.
[[ "$MODE" == "headless" ]] && NO_UI=true
[[ "$MODE" == "desktop"  ]] && IS_DESKTOP=true
info "Mode: $MODE | Tier: $TIER | Arch: $ARCH"

# ── TUI: Addon checklist (manual only — auto uses sensible defaults) ──────────
if ! $YES && [[ "$STYLE" == "manual" ]]; then
  SENSOR_STATE="ON"; $NO_SENSOR && SENSOR_STATE="OFF"
  API_STATE="ON";    $NO_CEREBRO_API && API_STATE="OFF"
  UI_STATE="ON";     $NO_UI && UI_STATE="OFF"
  OCC_STATE="ON";    $NO_OCCIPITAL && OCC_STATE="OFF"

  ADDONS=$(tui_checklist "Components" \
    "Select the components to install:\n(Space to toggle, Enter to confirm)" \
    "ui"        "apexos-rs-ui     Native Slint UI — kiosk display / desktop window" "$UI_STATE" \
    "cerebro"   "Cerebro API      REST dashboard + memory UI on :8765"        "$API_STATE" \
    "occipital" "Web Cortex       web_search/fetch + semantic recall"         "$OCC_STATE" \
    "sensor"    "Sensor Head      BME688 air quality + MLX90640 thermal cam"  "$SENSOR_STATE" \
    "voice"     "Voice            Wake-word + whisper transcription"          "OFF")

  # Parse whiptail checklist output (space-separated quoted tags)
  echo "$ADDONS" | grep -q '"ui"'        || NO_UI=true
  echo "$ADDONS" | grep -q '"cerebro"'   || NO_CEREBRO_API=true
  echo "$ADDONS" | grep -q '"occipital"' && NO_OCCIPITAL=false || NO_OCCIPITAL=true
  echo "$ADDONS" | grep -q '"sensor"'    && NO_SENSOR=false || NO_SENSOR=true
  echo "$ADDONS" | grep -q '"voice"'     && NO_VOICE=false  || NO_VOICE=true
fi

# ── API keys ──────────────────────────────────────────────────────────────────
# Priority: --api-key flag  >  key file on USB/SD-boot  >  manual TUI entry.
# The key is ~100 chars of "alien glyphs", so a pre-written file beats typing it.

# Convenience: pick up an ANTHROPIC_API_KEY already exported in the environment
# (eases onboarding for operators who keep their key in their shell). Best-effort:
# `sudo` resets the environment by default, so this only fires under a root shell,
# `sudo -E`, or a non-sudo run. A deliberate --api-key / boot-USB key still wins.
if [[ -z "$API_KEY" && -n "${ANTHROPIC_API_KEY:-}" ]]; then
  API_KEY="$ANTHROPIC_API_KEY"; API_KEY_SRC="environment (\$ANTHROPIC_API_KEY)"
  info "Found ANTHROPIC_API_KEY in the environment."
fi

# A key from --api-key / the environment / a boot file is surfaced + live-verified
# here; a rejected one is cleared (interactive) so the picker below re-prompts.
finalize_key_check

if [[ -z "$API_KEY" ]]; then
  info "Looking for a key file (apexos.env) on USB / SD-boot media …"
  if find_key_file; then
    API_KEY="$FOUND_ANTHROPIC"; API_KEY_SRC="boot/USB: ${FOUND_KEY_SRC}"
    [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
    ok "Loaded API key from ${FOUND_KEY_SRC}"
    finalize_key_check   # surface (masked) + live-verify; clears it if rejected
  else
    info "No key file found on removable media."
  fi
fi

# Still no key and we can ask interactively → scan-again / type / skip / abort.
if [[ -z "$API_KEY" ]] && ! $YES; then
  while true; do
    KEYCHOICE=$(tui_menu "Anthropic API Key" \
"Anthropic keys are ~100 characters — no fun to type here.

Easiest path (no typing):
  1. On any PC/phone, make a text file named  apexos.env
  2. Put one line in it:  ANTHROPIC_API_KEY=sk-ant-...
  3. Save it to a USB stick, OR straight onto the SD card's
     boot partition (it shows up as a normal drive on any PC).
  4. Plug into the Pi and choose 'Scan again'." \
      "scan"  "Scan USB / SD-boot for apexos.env again" \
      "type"  "Type the key by hand" \
      "skip"  "Skip — add it later in /etc/agentd/env" \
      "abort" "Abort the install")
    case "$KEYCHOICE" in
      scan)
        if find_key_file; then
          API_KEY="$FOUND_ANTHROPIC"; API_KEY_SRC="boot/USB: ${FOUND_KEY_SRC}"
          [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
          # Verify before accepting — a rejected key is cleared, so we re-loop.
          finalize_key_check; [[ -n "$API_KEY" ]] && break
        else
          tui_msg "Nothing found" \
            "Still no apexos.env on any USB or SD-boot partition.\n\nCheck the filename and that the device is plugged in,\nthen try again."
        fi ;;
      type)
        API_KEY=$(tui_input "Anthropic API Key" \
          "Paste or type your key (sk-ant-...):" "password") || true
        API_KEY_SRC="typed by hand"
        finalize_key_check; [[ -n "$API_KEY" ]] && break ;;
      skip)  break ;;
      abort) die "Install aborted — no API key provided." ;;
      *)     break ;;
    esac
  done
fi

# OpenRouter (optional) — manual mode only, and only if not already supplied.
if ! $YES && [[ "$STYLE" == "manual" && -z "$OPENROUTER_KEY" ]]; then
  OPENROUTER_KEY=$(tui_input "OpenRouter API Key (optional)" \
    "Enter your OpenRouter key for access to alternative models\n(GPT-4o, Gemini, Llama, etc.).\n\nLeave blank to skip." \
    "password") || true
fi

# ── TUI: Pre-build summary + confirm ─────────────────────────────────────────
if ! $YES; then
  ADDONS_LIST=""
  ! $NO_UI          && ADDONS_LIST+="  ✓ apexos-rs-ui  ($($IS_DESKTOP && echo 'desktop window' || echo 'KMS/DRM display'))\n"
  ! $NO_CEREBRO_API && ADDONS_LIST+="  ✓ cerebro-api   (REST dashboard :8765)\n"
  OCC_RECALL="FTS5 keyword recall"
  case "$TIER" in micro|standard|pro) OCC_RECALL="semantic recall (bge-small)" ;; esac
  ! $NO_OCCIPITAL   && ADDONS_LIST+="  ✓ occipital     (web cortex — $OCC_RECALL)\n"
  ! $NO_SENSOR      && ADDONS_LIST+="  ✓ sensor-head   (BME688 + MLX90640)\n"
  ! $NO_VOICE       && ADDONS_LIST+="  ✓ voice         (whisper transcription)\n"
  [[ -n "$API_KEY" ]]        && KEY_STATUS="Anthropic key: set" \
                             || KEY_STATUS="Anthropic key: NOT SET (add later)"
  [[ -n "$OPENROUTER_KEY" ]] && KEY_STATUS+="\n  OpenRouter key: set" \
                             || KEY_STATUS+="\n  OpenRouter key: not set"

  CONFIRM_MSG="Ready to install ApexOS-RS:

  Device : $PI_MODEL ($ARCH)
  Tier   : $TIER_DESC
  Mode   : $MODE

Components:
  ✓ agentd + cerebro-mcp + apexos-tools (always)
${ADDONS_LIST}
API keys:
  ${KEY_STATUS}

Build time: ~8 min on Pi 5, ~2 min on x86
Log: $LOG

Proceed with installation?"

  tui_yesno "Confirm Installation" "$CONFIRM_MSG" || {
    echo "Installation cancelled."
    exit 0
  }
fi

# ── Repo ───────────────────────────────────────────────────────────────────────
hdr "Repository"

if [[ -z "$REPO_DIR" ]]; then
  REPO_DIR=/opt/ApexOS-RS
  ensure_bootstrap_deps          # git/curl/ca-certs must exist before we can clone
  # The clone runs as root (needs to write under /opt), but the build runs as
  # $BUILD_USER (the rustup toolchain owner). Cargo must write target/ + Cargo.lock,
  # so $BUILD_USER has to OWN the tree — otherwise: Permission denied (os error 13)
  # at .../target. Chown BEFORE pulling so an existing root-owned clone (left by a
  # prior run) can be updated as $BUILD_USER without git "dubious ownership".
  if [[ -d "$REPO_DIR/.git" ]]; then
    [[ "$BUILD_USER" != "root" ]] && chown -R "$BUILD_USER:" "$REPO_DIR"
    info "Updating existing clone at $REPO_DIR …"
    # A prior `cargo build` (without --locked) can rewrite the tracked Cargo.lock,
    # leaving the deploy tree dirty so the next `git pull --ff-only` aborts. Discard
    # that drift before pulling — the /opt clone mirrors remote, it is not a dev tree.
    # (target/ is gitignored; the build below now uses --locked so future builds stay
    # clean, but legacy clones still carry the old drift, so we self-heal here.)
    GIT_RUN=(git -C "$REPO_DIR")
    [[ "$BUILD_USER" != "root" ]] && GIT_RUN=(sudo -u "$BUILD_USER" git -C "$REPO_DIR")
    "${GIT_RUN[@]}" checkout -- Cargo.lock 2>/dev/null || true
    "${GIT_RUN[@]}" pull --ff-only
  else
    info "Cloning ApexOS-RS …"
    git clone --depth=1 https://github.com/buckster123/ApexOS-RS "$REPO_DIR"
    [[ "$BUILD_USER" != "root" ]] && chown -R "$BUILD_USER:" "$REPO_DIR"
  fi
fi
[[ -f "$REPO_DIR/Cargo.toml" ]] || die "Cargo.toml not found in $REPO_DIR"
ok "Repo at $REPO_DIR (owner: $(stat -c '%U' "$REPO_DIR"))"

# ── System deps ────────────────────────────────────────────────────────────────
hdr "System dependencies"

# ffmpeg (ffprobe + ffplay) — runtime dep for the audio tools + Audio Editor
# (/api/audio/* analyze/waveform/process) and Sonus playback (/api/sonus/play).
# avahi-daemon + avahi-utils — mesh discovery (mDNS): avahi-daemon advertises this
# node's _apexos._tcp service (see the service file dropped below) and avahi-utils
# provides avahi-browse, which agentd's discovery loop + /api/mesh/nodes shell out to.
# Both halves are mode-independent — a headless inference node still joins the mesh.
# jq: the self-update watchdog parses request.json/health.json with it (sed fallback
# exists, but jq makes the recovery path robust — see docs/self-update.md).
PKGS=(curl git pkg-config build-essential libssl-dev whiptail ffmpeg avahi-daemon avahi-utils jq)
if ! $NO_UI; then
  PKGS+=(libfontconfig1-dev libgbm-dev libegl-dev libudev-dev libinput-dev libxkbcommon-dev)
fi

apt-get update -qq
apt-get install -y --no-install-recommends "${PKGS[@]}" 2>&1 \
  | grep -E "(installed|upgraded|error)" || true
ok "System packages installed"

# Camera eyes: USB/laptop webcams are captured via ffmpeg's V4L2 input (installed
# above). The Raspberry Pi CSI camera needs rpicam-apps (older name: libcamera-apps).
# Best-effort + separate from the main install so a generic Debian without the RPi
# package feed can't fail the whole run — APEX just falls back to a USB cam there.
if $IS_PI; then
  apt-get install -y --no-install-recommends rpicam-apps >/dev/null 2>&1 \
    || apt-get install -y --no-install-recommends libcamera-apps >/dev/null 2>&1 \
    || warn "rpicam-apps not installed — Pi CSI camera unavailable until 'apt install rpicam-apps'"
fi

# Sensor head (BME688 + MLX90640): provision the I2C prerequisite on a Pi when the
# sensor head was selected. The sensors live on the ARM I2C bus, read by an EXTERNAL
# SensorHead dashboard (not -RS itself — see the sensor-head gotcha in CLAUDE.md), so
# here we only enable the bus + tools the dashboard needs. Pi-5 gotcha: `dtparam=
# i2c_arm=on` enables the controller but leaves NO /dev/i2c-* until the i2c-dev module
# loads (raspi-config's do_i2c adds both; a manual config.txt edit does not) — so we
# do both. Idempotent; needs a reboot to take effect. (Default installs skip this:
# NO_SENSOR defaults true.)
SENSOR_I2C_PROVISIONED=false
if $IS_PI && ! $NO_SENSOR; then
  I2C_CFG=/boot/firmware/config.txt; [[ -f "$I2C_CFG" ]] || I2C_CFG=/boot/config.txt
  if [[ -f "$I2C_CFG" ]]; then
    if grep -qE '^[[:space:]]*dtparam=i2c_arm=on' "$I2C_CFG"; then
      :                                                   # already enabled
    elif grep -qE '^[[:space:]]*#[[:space:]]*dtparam=i2c_arm=on' "$I2C_CFG"; then
      sed -i -E 's/^[[:space:]]*#[[:space:]]*dtparam=i2c_arm=on.*/dtparam=i2c_arm=on/' "$I2C_CFG"
    else
      printf '\n# ApexOS-RS: enable I2C for the SensorHead (BME688 + MLX90640)\ndtparam=i2c_arm=on\n' >> "$I2C_CFG"
    fi
    echo i2c-dev > /etc/modules-load.d/i2c-dev.conf      # load the bus driver at boot
    apt-get install -y --no-install-recommends i2c-tools >/dev/null 2>&1 || true
    SENSOR_I2C_PROVISIONED=true
    ok "I2C enabled for the SensorHead (reboot required to activate /dev/i2c-*)"
  else
    warn "Sensor head selected but no Pi config.txt found — enable I2C manually"
  fi
fi

# Mesh advertisement (mDNS): drop the _apexos._tcp service file so avahi-daemon
# advertises THIS node. Without it the node browses an empty mesh — nothing to find.
# avahi watches /etc/avahi/services live, so a reload (not restart) picks it up.
if systemctl list-unit-files avahi-daemon.service &>/dev/null; then
  mkdir -p /etc/avahi/services
  install -m 644 "$REPO_DIR/deploy/avahi/apexos-rs.service" /etc/avahi/services/apexos-rs.service
  systemctl enable --now avahi-daemon >/dev/null 2>&1 || true
  systemctl reload avahi-daemon >/dev/null 2>&1 \
    || systemctl restart avahi-daemon >/dev/null 2>&1 || true
  ok "Mesh advertisement registered (_apexos._tcp)"
else
  warn "avahi-daemon unavailable — this node won't appear on the mesh"
fi

# Monochrome emoji font for the native UI (📊 ☺ ツ render; the smiley/pictograph
# set too). ui-slint renders with femtovg, which can't rasterize colour-bitmap
# emoji fonts (outline glyphs only) — so we ship the OFL monochrome "Noto Emoji"
# and ui-slint rejects the colour font for its own process (see
# ensure_mono_emoji_fontconfig). Installed wherever fontconfig is present (kiosk +
# desktop); a bare headless node has nothing to render, so the skip is harmless.
if command -v fc-cache >/dev/null 2>&1; then
  install -d /usr/local/share/fonts/apexos-rs
  install -m 644 "$REPO_DIR/deploy/fonts/NotoEmoji-mono.ttf" \
    /usr/local/share/fonts/apexos-rs/NotoEmoji-mono.ttf
  fc-cache -f /usr/local/share/fonts/apexos-rs >/dev/null 2>&1 || true
  ok "Monochrome emoji font installed (native UI)"
fi

# ── Rust toolchain ─────────────────────────────────────────────────────────────
hdr "Rust toolchain"

CARGO="$BUILD_HOME/.cargo/bin/cargo"
if [[ ! -x "$CARGO" ]]; then
  info "Installing Rust for $BUILD_USER …"
  sudo -u "$BUILD_USER" bash -c \
    'curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path' \
    2>&1 | grep -E "(stable|installed|error)" || true
fi
[[ -x "$CARGO" ]] || die "Rust install failed — cargo not found at $CARGO"
CARGO_VER=$(sudo -u "$BUILD_USER" "$CARGO" --version 2>/dev/null || echo "unknown")
ok "Rust: $CARGO_VER"

# ── User + groups ──────────────────────────────────────────────────────────────
hdr "User and permissions"

id agentd &>/dev/null || useradd -r -s /sbin/nologin -d /var/lib/agentd agentd

# audio: TTS (/api/speak) + Sonus playback (/api/sonus/play) open the ALSA device
# directly. video: camera eyes (camera_capture tool + /api/snapshot) read /dev/video*
# and the Pi CSI camera. Both are display-independent — a headless laptop/USB-cam node
# needs them too — so they are NOT gated behind the UI.
for grp in audio video; do
  getent group "$grp" &>/dev/null && usermod -aG "$grp" agentd || true
done

if ! $NO_UI; then
  # render + input: KMS/DRM display only.
  for grp in render input; do
    getent group "$grp" &>/dev/null && usermod -aG "$grp" agentd || true
  done
fi

mkdir -p /etc/agentd /var/lib/agentd/{workspace,events,ui,cerebro/models,update}
chown -R agentd:agentd /var/lib/agentd
chmod 750 /var/lib/agentd
ok "User and directories ready"

# ── polkit power rule ──────────────────────────────────────────────────────────
mkdir -p /etc/polkit-1/rules.d
cat > /etc/polkit-1/rules.d/49-agentd-power.rules << 'EOF'
polkit.addRule(function(action, subject) {
    if (subject.user == "agentd" &&
        (action.id == "org.freedesktop.login1.reboot" ||
         action.id == "org.freedesktop.login1.reboot-multiple-sessions" ||
         action.id == "org.freedesktop.login1.power-off" ||
         action.id == "org.freedesktop.login1.power-off-multiple-sessions")) {
        return polkit.Result.YES;
    }
});
EOF
systemctl try-restart polkit 2>/dev/null || true

# ── Build ──────────────────────────────────────────────────────────────────────
hdr "Building ApexOS-RS (this takes ~8 min on Pi 5, ~2 min on x86)"

cd "$REPO_DIR"

# --locked: build strictly against the committed Cargo.lock so cargo never rewrites
# it (which would dirty the deploy tree and break the next `git pull --ff-only`). A
# genuinely stale lock now fails loudly here instead of silently — commit the lock.
BUILD_ARGS="--release --workspace --locked"
if $NO_UI; then
  BUILD_ARGS+=" --exclude ui-slint"
  info "Skipping ui-slint (headless/desktop mode)"
fi

# Temporary build swap — on a low-RAM UI node, guarantee enough headroom that a capped
# rustc still can't be OOM-killed mid-compile. Created only if existing swap is thin, and
# torn down after the build (no persistent change). The EXIT trap covers the die/ERR paths.
BUILD_SWAP=""
remove_build_swap() {
  [[ -n "$BUILD_SWAP" ]] || return 0
  sudo swapoff "$BUILD_SWAP" 2>/dev/null || true
  sudo rm -f "$BUILD_SWAP" 2>/dev/null || true
  info "Removed temporary build swap ($BUILD_SWAP)"
  BUILD_SWAP=""
}
trap remove_build_swap EXIT
ensure_build_swap() {                                   # $1 = desired total swap (KB)
  local need_kb=$1 have_kb sz_mb
  have_kb=$(awk '/^SwapTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)
  [[ "$have_kb" -ge "$need_kb" ]] && { info "Swap ($((have_kb / 1024)) MB) already sufficient for the build"; return 0; }
  sz_mb=$(( (need_kb - have_kb) / 1024 + 256 ))
  BUILD_SWAP="/var/tmp/apexos-build-swap"
  sudo rm -f "$BUILD_SWAP" 2>/dev/null || true
  warn "Adding ${sz_mb} MB temporary build swap ($BUILD_SWAP) — removed when the build finishes"
  if ! sudo fallocate -l "${sz_mb}M" "$BUILD_SWAP" 2>/dev/null; then
    sudo dd if=/dev/zero of="$BUILD_SWAP" bs=1M count="$sz_mb" status=none 2>/dev/null \
      || { warn "Could not allocate build swap (low disk?) — continuing without it"; BUILD_SWAP=""; return 0; }
  fi
  sudo chmod 600 "$BUILD_SWAP" || true
  if ! sudo mkswap "$BUILD_SWAP" >/dev/null 2>&1 || ! sudo swapon "$BUILD_SWAP" 2>/dev/null; then
    warn "Could not enable build swap — continuing without it"
    sudo rm -f "$BUILD_SWAP" 2>/dev/null || true; BUILD_SWAP=""
  fi
}

# Low-memory build guard. The ui-slint compile (the single heaviest unit in the workspace)
# peaks well past available RAM on a small device — on a 4 GB Pi the kernel OOM-killer
# SIGKILLs rustc (signal 9), so a UI node fails *every* UI-touching update. We bound peak
# compiler memory three ways — fewer parallel jobs, opt-level 2 (the big lever: opt-3
# inlining on the ~5k-line unit is the real hog), LTO off — AND top up swap so the build
# can only be slowed, never killed. High-RAM and headless (no ui-slint) builds are untouched.
BUILD_ENV=()
RAM_KB=$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)
if ! $NO_UI && [[ "$RAM_KB" -gt 0 && "$RAM_KB" -lt 6291456 ]]; then   # < 6 GiB
  BUILD_JOBS=2
  [[ "$RAM_KB" -lt 4718592 ]] && BUILD_JOBS=1                          # ≤ 4 GiB → serialize fully
  warn "Low-RAM node ($((RAM_KB / 1024)) MB) — UI build capped: -j$BUILD_JOBS, opt-level 2, LTO off"
  BUILD_ENV=(
    CARGO_BUILD_JOBS="$BUILD_JOBS"
    CARGO_PROFILE_RELEASE_OPT_LEVEL=2
    CARGO_PROFILE_RELEASE_LTO=off
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
  )
  ensure_build_swap 4194304                                            # top up to ~4 GiB total swap
fi

BUILD_LOG=$(mktemp /tmp/apexos-cargo-build.XXXXXX.log)
info "Build log → $BUILD_LOG"

sudo -u "$BUILD_USER" env "${BUILD_ENV[@]}" "$CARGO" build $BUILD_ARGS 2>&1 \
  | tee "$BUILD_LOG" \
  | grep --line-buffered -E "(^Compiling (agentd|cerebro|apexos|ui-slint|apex)|Finished|^error)" \
  || true

if grep -q "^error" "$BUILD_LOG"; then
  grep "^error" "$BUILD_LOG" | head -5
  if grep -q "signal: 9" "$BUILD_LOG"; then
    warn "rustc was OOM-killed (signal 9) despite the low-RAM guard — the temporary build"
    warn "swap likely failed to allocate (check free disk on /var/tmp), or other load is"
    warn "competing for RAM (stop agentd and re-run)."
  fi
  die "Build failed — see $BUILD_LOG for full output"
fi

ok "Build complete"
remove_build_swap
cp "$BUILD_LOG" "$LOG.build" 2>/dev/null || true

# ── Install binaries ───────────────────────────────────────────────────────────
hdr "Installing binaries"

BIN_DIR="$REPO_DIR/target/release"

install_bin() {
  local name="$1" dest="${2:-/usr/local/bin/$1}"
  [[ -x "$BIN_DIR/$name" ]] || die "$name not found in $BIN_DIR — build may have failed"
  install -m 755 "$BIN_DIR/$name" "$dest"
  ok "$name → $dest ($(du -sh "$BIN_DIR/$name" | cut -f1))"
}

install_bin agentd
install_bin cerebro-mcp
install_bin cerebro-api
install_bin cerebro
install_bin apexos-tools
install_bin apex-sensor-bridge

if ! $NO_UI; then
  install_bin ui-slint
  install -m 755 "$BIN_DIR/ui-slint" /usr/local/bin/apexos-rs-ui
  ok "apexos-rs-ui → /usr/local/bin/apexos-rs-ui"
  if $IS_DESKTOP; then
    # Desktop mode: a winit window in the user's session (app menu + autostart),
    # NOT the root KMS kiosk service. The same binary, launched differently. Both
    # launcher writes are best-effort — the binary is already installed, so a
    # launcher hiccup shouldn't abort the whole install.
    install -Dm 644 "$REPO_DIR/deploy/apexos-rs-ui.desktop" /usr/share/applications/apexos-rs-ui.desktop \
      && ok "Desktop launcher → app menu (ApexOS-RS)" \
      || warn "Could not install the app-menu launcher — launch /usr/local/bin/apexos-rs-ui manually"
    DESK_HOME=$(getent passwd "$BUILD_USER" 2>/dev/null | cut -d: -f6)
    if [[ -n "$DESK_HOME" && -d "$DESK_HOME" && "$BUILD_USER" != "root" ]]; then
      if install -Dm 644 "$REPO_DIR/deploy/apexos-rs-ui.desktop" \
           "$DESK_HOME/.config/autostart/apexos-rs-ui.desktop"; then
        chown -R "$BUILD_USER":"$BUILD_USER" "$DESK_HOME/.config/autostart" 2>/dev/null || true
        ok "Autostart for $BUILD_USER (rm ~/.config/autostart/apexos-rs-ui.desktop to disable)"
      fi
    fi
  fi
fi

# ── Web UI (browser + PWA) ────────────────────────────────────────────────────
# The browser + mobile-PWA frontend agentd serves at http://<node>:8787/ — login
# (3e session-token auth), streaming chat, tool cards + inline approvals. -RS owns
# this now: a fresh, lean, installable PWA (NOT the legacy ../ApexOS web app).
# Deployed to AGENTD_UI (/var/lib/agentd/ui) on EVERY node, headless included — on a
# headless node it's the *only* human interface. Copy-always (these are -RS-owned
# static assets, like a binary, not seed-if-absent config), so an apexos-update
# refreshes the web client too.
hdr "Installing web UI (browser + PWA)"
WEB_DST=/var/lib/agentd/ui
if [[ -d "$REPO_DIR/web" ]]; then
  for f in index.html style.css app.js sw.js manifest.json icon.svg; do
    [[ -f "$REPO_DIR/web/$f" ]] && install -Dm 644 "$REPO_DIR/web/$f" "$WEB_DST/$f"
  done
  chown -R agentd:agentd "$WEB_DST" 2>/dev/null || true
  ok "Web UI → $WEB_DST (open http://<node>:8787/ — log in with your profile)"
else
  warn "web/ not found in repo — browser/PWA UI not installed"
fi

# ── USB exo-workspace (removable portable workspace) ──────────────────────────────
# A USB stick prepared as an ApexOS exo-workspace (filesystem LABEL APEX-*, see
# docs/usb-workspace.md) is claimed on plug and own-mounted UNDER the agent workspace
# at <workspace>/media/<label>, so the agent + Explorer + desktop apps reach it.
# Marker-gated: ONLY APEX-* sticks are claimed — every other USB is left to the DE
# (desktop) / ignored (kiosk), so this is safe on a daily-driver laptop. Uniform
# across modes. Provisioned on every node; harmless where no exo-stick is ever used.
hdr "Installing USB exo-workspace support"
if [[ -d "$REPO_DIR/deploy/usb" ]]; then
  # Helper scripts (all root-run by the systemd units — never via sudo).
  install -Dm 755 "$REPO_DIR/deploy/usb/usb-mount"             /usr/local/lib/apexos/usb-mount
  install -Dm 755 "$REPO_DIR/deploy/usb/usb-umount"            /usr/local/lib/apexos/usb-umount
  install -Dm 755 "$REPO_DIR/deploy/usb/usb-eject-drain"       /usr/local/lib/apexos/usb-eject-drain
  install -Dm 755 "$REPO_DIR/deploy/usb/usb-prep"              /usr/local/lib/apexos/usb-prep
  install -Dm 755 "$REPO_DIR/deploy/usb/usb-prep-drain"        /usr/local/lib/apexos/usb-prep-drain
  install -Dm 755 "$REPO_DIR/deploy/usb/apexos-workspace-init" /usr/local/bin/apexos-workspace-init
  # Relabel tools for "Use this drive" (exFAT preferred; FAT fallback). Best-effort.
  apt-get install -y --no-install-recommends exfatprogs dosfstools >/dev/null 2>&1 || true
  # udev rule (claim APEX-* sticks + defer udisks) + the templated mount service.
  install -Dm 644 "$REPO_DIR/deploy/udev/99-apexos-usb.rules"  /etc/udev/rules.d/99-apexos-usb.rules
  install -Dm 644 "$REPO_DIR/deploy/systemd/apexos-usb-mount@.service" /etc/systemd/system/apexos-usb-mount@.service
  # UI/agent eject = privilege separation, NOT sudo: agentd runs NoNewPrivileges=true so
  # the kernel blocks setuid sudo. Instead it drops an APEX-<label> request file into the
  # (agentd-owned) eject dir; the path unit fires the root drain service that umounts it.
  install -Dm 644 "$REPO_DIR/deploy/systemd/apexos-usb-eject.path"    /etc/systemd/system/apexos-usb-eject.path
  install -Dm 644 "$REPO_DIR/deploy/systemd/apexos-usb-eject.service" /etc/systemd/system/apexos-usb-eject.service
  # "Use this drive" prep = the same privilege-separated pattern: agentd drops a *.req file,
  # the root drain runs usb-prep (which re-validates the device — USB, never a system disk).
  install -Dm 644 "$REPO_DIR/deploy/systemd/apexos-usb-prep.path"     /etc/systemd/system/apexos-usb-prep.path
  install -Dm 644 "$REPO_DIR/deploy/systemd/apexos-usb-prep.service"  /etc/systemd/system/apexos-usb-prep.service
  # The old sudoers drop-in is now dead (sudo can't escalate under NoNewPrivileges) — remove it.
  rm -f /etc/sudoers.d/apexos-usb
  # The mount target + the request dirs live under /var/lib/agentd (agentd-owned).
  install -d -o agentd -g agentd /var/lib/agentd/workspace/media 2>/dev/null \
    || mkdir -p /var/lib/agentd/workspace/media
  install -d -o agentd -g agentd /var/lib/agentd/usb-eject 2>/dev/null \
    || mkdir -p /var/lib/agentd/usb-eject
  install -d -o agentd -g agentd /var/lib/agentd/usb-prep 2>/dev/null \
    || mkdir -p /var/lib/agentd/usb-prep
  systemctl daemon-reload >/dev/null 2>&1 || true
  udevadm control --reload >/dev/null 2>&1 || true
  # Arm the eject + prep watchers now (idempotent on re-runs); the dirs above must exist first.
  systemctl enable --now apexos-usb-eject.path >/dev/null 2>&1 || true
  systemctl enable --now apexos-usb-prep.path  >/dev/null 2>&1 || true
  ok "USB exo-workspace ready (or use the Explorer 'Use this drive' button on a plugged stick)"
else
  warn "deploy/usb not found — USB exo-workspace support not installed"
fi

# ── Occipital (web reading cortex) ───────────────────────────────────────────────
# The agent's reading cortex — web_search / web_fetch / web_recall / web_save /
# web_forget — lives in a SEPARATE sibling repo (github.com/buckster123/Occipital-RS),
# NOT a workspace member, so the build above doesn't produce it. Clone + build + deploy
# occipital-mcp alongside the workspace binaries (the plugin block is enabled in the
# Config section below, only on success). Default ON; skip with --no-occipital.
# Tier split mirrors cerebro's: Micro+ build `--features embeddings` for bge-small
# semantic recall (web_recall by meaning); Nano stays FTS5 keyword recall (no ONNX).
# Best-effort: a clone/build failure WARNS and continues — occipital is an enhancement,
# not core (agentd runs fine without it), and apexos-update retries next run.
if ! $NO_OCCIPITAL; then
  hdr "Occipital (web reading cortex)"
  OCCIPITAL_DIR="$(dirname "$REPO_DIR")/Occipital-RS"   # sibling of the ApexOS-RS clone
  case "$TIER" in
    micro|standard|pro) OCC_FEATURES="--features embeddings" ;;   # semantic recall
    *)                  OCC_FEATURES="" ;;                        # nano → FTS5 only
  esac

  occipital_provision() {
    ensure_bootstrap_deps                              # git/curl/ca-certs (idempotent)
    if [[ -d "$OCCIPITAL_DIR/.git" ]]; then
      [[ "$BUILD_USER" != "root" ]] && chown -R "$BUILD_USER:" "$OCCIPITAL_DIR"
      info "Updating Occipital-RS clone at $OCCIPITAL_DIR …"
      local GIT_OCC=(git -C "$OCCIPITAL_DIR")
      [[ "$BUILD_USER" != "root" ]] && GIT_OCC=(sudo -u "$BUILD_USER" git -C "$OCCIPITAL_DIR")
      "${GIT_OCC[@]}" checkout -- Cargo.lock 2>/dev/null || true   # self-heal build drift
      "${GIT_OCC[@]}" pull --ff-only
    else
      info "Cloning Occipital-RS …"
      git clone --depth=1 https://github.com/buckster123/Occipital-RS "$OCCIPITAL_DIR"
      [[ "$BUILD_USER" != "root" ]] && chown -R "$BUILD_USER:" "$OCCIPITAL_DIR"
    fi
    info "Building occipital-mcp${OCC_FEATURES:+ ($OCC_FEATURES)} …"
    # NOT --locked: a foreign repo whose committed lock we don't gate-keep — a stale
    # lock shouldn't fail the build (the checkout above keeps re-runs from dirtying it).
    sudo -u "$BUILD_USER" "$CARGO" build --release -p occipital-mcp $OCC_FEATURES \
      --manifest-path "$OCCIPITAL_DIR/Cargo.toml" 2>&1 \
      | grep --line-buffered -E "(^[[:space:]]*Compiling occipital|Finished|^error)" || true
    [[ -x "$OCCIPITAL_DIR/target/release/occipital-mcp" ]] \
      || { warn "occipital-mcp build produced no binary"; return 1; }
    install -m 755 "$OCCIPITAL_DIR/target/release/occipital-mcp" /usr/local/bin/occipital-mcp
    install -d -o agentd -g agentd /var/lib/agentd/occipital
    [[ -n "$OCC_FEATURES" ]] && install -d -o agentd -g agentd /var/lib/agentd/occipital/models
    ok "occipital-mcp → /usr/local/bin/occipital-mcp ($([[ -n "$OCC_FEATURES" ]] && echo 'semantic recall' || echo 'FTS5 keyword recall'))"
    return 0
  }

  if occipital_provision; then
    OCCIPITAL_INSTALLED=true
  else
    warn "Occipital not installed — agentd runs without the web cortex; apexos-update retries"
  fi
fi

# ── Voice (Kokoro TTS sidecar) ────────────────────────────────────────────────
# apex-tts: the Kokoro-82M neural voice that replaces robotic piper for /api/speak.
# A workspace-EXCLUDED crate (its own Cargo.lock pins a different ort rc than
# cerebro's fastembed — see tools/crates/apex-tts/Cargo.toml), so it's built
# separately here, like occipital. Voice is opt-in (default OFF): the TUI add-on,
# a boot/USB APEXOS_VOICE=1, or --voice. Best-effort — a build/download failure
# WARNS and continues; /api/speak then falls back to espeak-ng, so the node still
# talks. This is what finally makes the long-inert --no-voice/voice flag *do* something.
VOICE_INSTALLED=false; STT_INSTALLED=false
if ! $NO_VOICE; then
  hdr "Voice (Kokoro TTS)"
  KOKORO_DIR=/var/lib/agentd/kokoro
  KOKORO_MODEL_URL="${KOKORO_MODEL_URL:-https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.int8.onnx}"
  KOKORO_VOICES_URL="${KOKORO_VOICES_URL:-https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin}"

  voice_provision() {
    ensure_bootstrap_deps                              # git/curl/ca-certs (idempotent)
    # espeak-ng is BOTH Kokoro's phonemizer (apex-tts shells it) and the final
    # /api/speak fallback — needed either way.
    apt-get install -y --no-install-recommends espeak-ng >/dev/null 2>&1 \
      || warn "espeak-ng install failed — Kokoro phonemization + the TTS fallback need it"

    info "Building apex-tts (Kokoro sidecar; pulls onnxruntime — first build is heavy) …"
    # Excluded crate → build by its own manifest. NOT --locked (pinned ort lives in
    # its own lock; a foreign onnxruntime fetch shouldn't fail on a lock nicety).
    sudo -u "$BUILD_USER" "$CARGO" build --release \
      --manifest-path "$REPO_DIR/tools/crates/apex-tts/Cargo.toml" 2>&1 \
      | grep --line-buffered -E "(^[[:space:]]*Compiling (apex-tts|tts-rs|ort)|Finished|^error)" || true
    local bin="$REPO_DIR/tools/crates/apex-tts/target/release/apex-tts"
    [[ -x "$bin" ]] || { warn "apex-tts build produced no binary"; return 1; }
    install -m 755 "$bin" /usr/local/bin/apex-tts
    install -d -o agentd -g agentd "$KOKORO_DIR"

    # Fetch the int8 model (~92MB) + voices (~28MB) if absent (idempotent; atomic via .tmp).
    local onnx="$KOKORO_DIR/kokoro-v1.0.int8.onnx" voices="$KOKORO_DIR/voices-v1.0.bin"
    if [[ ! -f "$onnx" ]]; then
      info "Downloading Kokoro model (~92MB) …"
      curl -fSL --retry 3 -o "$onnx.tmp" "$KOKORO_MODEL_URL" \
        && mv "$onnx.tmp" "$onnx" || { rm -f "$onnx.tmp"; warn "Kokoro model download failed"; return 1; }
    fi
    if [[ ! -f "$voices" ]]; then
      info "Downloading Kokoro voices (~28MB) …"
      curl -fSL --retry 3 -o "$voices.tmp" "$KOKORO_VOICES_URL" \
        && mv "$voices.tmp" "$voices" || { rm -f "$voices.tmp"; warn "Kokoro voices download failed"; return 1; }
    fi
    chown -R agentd:agentd "$KOKORO_DIR" 2>/dev/null || true
    ok "apex-tts → /usr/local/bin/apex-tts (Kokoro model in $KOKORO_DIR)"
    return 0
  }

  if voice_provision; then
    VOICE_INSTALLED=true
  else
    warn "Voice not installed — /api/speak uses espeak-ng; apexos-update retries"
  fi

  # Local STT (apex-stt Whisper sidecar) — independent of the TTS build above, so a
  # failure of one doesn't sink the other. whisper.cpp has no ort, so its excluded
  # workspace is purely for build isolation + a resident model.
  WHISPER_DIR=/var/lib/agentd/whisper
  WHISPER_GGML_URL="${WHISPER_GGML_URL:-https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin}"

  stt_provision() {
    info "Building apex-stt (Whisper sidecar; compiles whisper.cpp — heavy first build) …"
    sudo -u "$BUILD_USER" "$CARGO" build --release \
      --manifest-path "$REPO_DIR/tools/crates/apex-stt/Cargo.toml" 2>&1 \
      | grep --line-buffered -E "(^[[:space:]]*Compiling (apex-stt|whisper-rs)|Finished|^error)" || true
    local bin="$REPO_DIR/tools/crates/apex-stt/target/release/apex-stt"
    [[ -x "$bin" ]] || { warn "apex-stt build produced no binary"; return 1; }
    install -m 755 "$bin" /usr/local/bin/apex-stt
    install -d -o agentd -g agentd "$WHISPER_DIR"

    # Fetch the ggml model (base.en ~148MB) if absent (idempotent; atomic via .tmp).
    local model="$WHISPER_DIR/ggml-base.en.bin"
    if [[ ! -f "$model" ]]; then
      info "Downloading Whisper model (base.en, ~148MB) …"
      curl -fSL --retry 3 -o "$model.tmp" "$WHISPER_GGML_URL" \
        && mv "$model.tmp" "$model" || { rm -f "$model.tmp"; warn "Whisper model download failed"; return 1; }
    fi
    chown -R agentd:agentd "$WHISPER_DIR" 2>/dev/null || true
    ok "apex-stt → /usr/local/bin/apex-stt (Whisper model in $WHISPER_DIR)"
    return 0
  }

  if stt_provision; then
    STT_INSTALLED=true
  else
    warn "Local STT not installed — /api/transcribe needs whisper-cpp or cloud STT; apexos-update retries"
  fi
fi

# ── Config ─────────────────────────────────────────────────────────────────────
hdr "Configuration"

# plugins.toml
# bge-small (384-dim) for every embed-enabled tier — it's the only model cerebro
# wires through and is plenty accurate for memory recall. (nano stays FTS5-only.)
# bge-large was set for pro but cerebro rejected it → embeddings silently disabled;
# see cerebro vector.rs. Revisit if/when a larger model is actually wired in.
EMBED_MODEL=""
case "$TIER" in
  micro|standard|pro) EMBED_MODEL="BAAI/bge-small-en-v1.5" ;;
esac

# Seed from the template only if absent. `apexos-update` re-runs this script, and
# agentd self-evolution can register_mcp_server into the live plugins.toml at
# runtime — re-installing the template every update would silently drop those
# entries. Same seed-if-absent contract as policy.toml / soul.md / peers.toml below.
# Tradeoff: a NEW built-in plugin shipped in a future release won't reach an already-
# deployed node until the operator merges it in (acceptable — inert until plugin
# self-evolution ships; revisit with a name-keyed merge then).
if [[ ! -f /etc/agentd/plugins.toml ]]; then
  install -m 644 "$REPO_DIR/config/plugins.toml" /etc/agentd/plugins.toml
  if [[ -n "$EMBED_MODEL" ]]; then
    sed -i "/FASTEMBED_CACHE_DIR/a CEREBRO_EMBED_MODEL = \"$EMBED_MODEL\"" /etc/agentd/plugins.toml
  fi
fi

# Enable the Occipital plugin only when occipital-mcp actually installed — the
# template ships the block COMMENTED so agentd is never pointed at a missing binary.
# Additive + idempotent: the grep is anchored to an UNcommented `id = "occipital"`,
# so it skips both the commented template line AND a prior run / an APEX
# register_mcp_server entry (preserving the seed-if-absent + self-evolution contract).
# This is what brings the web cortex to already-deployed nodes on apexos-update.
if $OCCIPITAL_INSTALLED && [[ -f /etc/agentd/plugins.toml ]] \
   && ! grep -qE '^[[:space:]]*id[[:space:]]*=[[:space:]]*"occipital"' /etc/agentd/plugins.toml; then
  {
    echo ""
    echo "[[plugin]]"
    echo 'id      = "occipital"'
    echo 'cmd     = "/usr/local/bin/occipital-mcp"'
    echo "args    = []"
    echo 'restart = "always"'
    echo "[plugin.env]"
    echo 'OCCIPITAL_DB        = "/var/lib/agentd/occipital/occipital.db"'
    echo 'OCCIPITAL_KEYS_FILE = "/var/lib/agentd/occipital/keys.toml"'
    echo 'RUST_LOG            = "warn"'
    if [[ -n "$OCC_FEATURES" ]]; then
      echo 'OCCIPITAL_EMBED_MODEL = "BAAI/bge-small-en-v1.5"'
      echo 'FASTEMBED_CACHE_DIR   = "/var/lib/agentd/occipital/models"'
    fi
  } >> /etc/agentd/plugins.toml
  ok "Occipital plugin registered in /etc/agentd/plugins.toml"
fi

# ── policy-sync ───────────────────────────────────────────────────────────────
# policy.toml is seed-if-absent (self-evolved rules must survive updates), which
# meant a rule shipped AFTER a node's first install never reached it — the tool
# then gates "unknown → ask" in suggest mode on approvals nobody watches (the
# 2026-07 sweep found live nodes missing 27–35 rules each). sync_policy_rules
# closes that gap ADDITIVELY on every run: a [rules] key present in the shipped
# config but absent from the live file is appended (inside [rules], before the
# next section header); an existing key is NEVER touched — the split is: soul =
# self-evolved, policy = follows the repo additively, self-evolved values win.
sync_policy_rules() {
  local shipped="$1" live="$2"
  [[ -f "$shipped" && -f "$live" ]] || return 0
  local missing=() key
  while IFS= read -r key; do
    grep -qE "^\"${key}\"[[:space:]]*=" "$live" || missing+=("$key")
  done < <(grep -oE '^"[a-z_]+"' "$shipped" | tr -d '"')
  [[ ${#missing[@]} -gt 0 ]] || return 0
  local block; block=$(mktemp)
  {
    echo ""
    echo "# --- rules added by apexos policy-sync $(date -u +%F) (new in this release; existing values never touched) ---"
    for key in "${missing[@]}"; do
      grep -E "^\"${key}\"[[:space:]]*=" "$shipped" | head -1
    done
  } > "$block"
  # Insert inside [rules]: before the first section header after it, else EOF.
  # (A degenerate live file with no [rules] gets the header appended first.)
  local rules_ln next_ln=""
  rules_ln=$(grep -n '^\[rules\]' "$live" | head -1 | cut -d: -f1 || true)
  if [[ -n "$rules_ln" ]]; then
    next_ln=$(awk -v s="$rules_ln" 'NR>s && /^\[/{print NR; exit}' "$live")
  else
    printf '\n[rules]' >> "$live"
  fi
  if [[ -n "$next_ln" ]]; then
    local tmp; tmp=$(mktemp)
    head -n $((next_ln-1)) "$live" > "$tmp"
    cat "$block" >> "$tmp"
    echo "" >> "$tmp"
    tail -n +"$next_ln" "$live" >> "$tmp"
    cat "$tmp" > "$live"   # cat-over keeps the live file's owner + mode
    rm -f "$tmp"
  else
    cat "$block" >> "$live"
  fi
  rm -f "$block"
  ok "policy-sync: ${#missing[@]} new rule(s) → $live (${missing[*]})"
}

# policy.toml (don't overwrite an existing policy — but DO additively sync new rules)
if [[ ! -f /etc/agentd/policy.toml ]]; then
  install -m 644 "$REPO_DIR/config/policy.toml" /etc/agentd/policy.toml
else
  sync_policy_rules "$REPO_DIR/config/policy.toml" /etc/agentd/policy.toml
fi

# soul.md — APEX's identity / system prompt (created from the default if missing).
if [[ ! -f /etc/agentd/soul.md ]]; then
  install -m 644 "$REPO_DIR/config/soul.md" /etc/agentd/soul.md
fi

# parts/inventory.toml — EDK on-hand parts inventory (docs/edk.md). agentd READS it for
# the embodiment "Extensions on hand" hint; operator-curated, so don't overwrite edits.
# Root-owned + world-readable is fine — the daemon only reads it (no self-write path yet).
mkdir -p /etc/agentd/parts
if [[ ! -f /etc/agentd/parts/inventory.toml ]]; then
  install -m 644 "$REPO_DIR/config/parts/inventory.toml" /etc/agentd/parts/inventory.toml
fi

# peers.toml — mesh registry (agentd writes it at runtime; seed empty if missing).
# Holds per-peer a2a tokens (secrets), so keep it owner-only (0600); agentd re-clamps
# the mode on every save() too.
if [[ ! -f /etc/agentd/peers.toml ]]; then
  echo "# ApexOS mesh peers" > /etc/agentd/peers.toml
fi
chmod 600 /etc/agentd/peers.toml

# Agent-mutable configs must be owned by the agentd user so the daemon can rewrite
# them: Settings save (soul), and self-evolution (update_system_prompt / update_policy_rule
# / register_mcp_server) writing soul.md, policy.toml, plugins.toml, peers.toml.
# /etc/agentd itself stays root-owned so the env file (auth token, 600 root:root) is
# protected — we chown the individual files, not the directory. (write_atomic falls
# back to an in-place write when the root-owned dir blocks the temp+rename path.)
chown agentd:agentd /etc/agentd/soul.md /etc/agentd/policy.toml \
                    /etc/agentd/plugins.toml /etc/agentd/peers.toml
# Identity registry (multi-agent boot flow, docs/agent-identity.md): agentd seeds
# + the identity API writes identities.toml and per-agent souls/. Pre-create &
# chown so the root-owned /etc/agentd dir doesn't block those writes.
touch /etc/agentd/identities.toml
mkdir -p /etc/agentd/souls
chown agentd:agentd /etc/agentd/identities.toml
chown -R agentd:agentd /etc/agentd/souls

# install.conf — persist the resolved deployment shape so a re-run restores it
# (load_persisted_config above) instead of re-auto-detecting. Non-secret (mode/tier/
# component flags), so root-owned 0644. Rewritten every install: a manual mode/
# component change here becomes the new sticky default for the next apexos-update.
write_install_conf() {
  local tmp; tmp=$(mktemp "${CONF_FILE}.XXXXXX")
  {
    echo "# ApexOS-RS resolved install choices — written by install.sh."
    echo "# apexos-update re-reads this so a re-run keeps the same deployment shape"
    echo "# (no re-auto-detect → a headless node won't flip to kiosk). Override with a"
    echo "# CLI flag, a freshly-plugged apexos.conf on USB, or by deleting this file."
    echo "APEXOS_MODE=$MODE"
    echo "APEXOS_TIER=$TIER"
    echo "APEXOS_NO_UI=$NO_UI"
    echo "APEXOS_NO_SENSOR=$NO_SENSOR"
    echo "APEXOS_NO_OCCIPITAL=$NO_OCCIPITAL"
    echo "APEXOS_NO_CEREBRO_API=$NO_CEREBRO_API"
    echo "APEXOS_VOICE=$( $NO_VOICE && echo false || echo true )"
  } > "$tmp"
  chmod 644 "$tmp"; chown root:root "$tmp"
  mv "$tmp" "$CONF_FILE"
  ok "Install choices saved → $CONF_FILE (mode=$MODE tier=$TIER)"
}
write_install_conf

# env file — API keys (don't overwrite existing keys)
ENV_FILE=/etc/agentd/env
touch "$ENV_FILE"; chmod 600 "$ENV_FILE"; chown root:root "$ENV_FILE"

write_env_key() {
  local key="$1" val="$2"
  [[ -z "$val" ]] && return
  # Rewrite via a temp file so the value can contain any character (sed-special
  # chars like '|' or '&' in API keys would corrupt an in-place s/// otherwise).
  local tmp; tmp=$(mktemp "${ENV_FILE}.XXXXXX")
  chmod 600 "$tmp"; chown root:root "$tmp"
  grep -v "^${key}=" "$ENV_FILE" 2>/dev/null > "$tmp" || true
  echo "${key}=${val}" >> "$tmp"
  mv "$tmp" "$ENV_FILE"
}

# Generate gateway token once — never overwrite an existing one
if ! grep -q "^AGENTD_TOKEN=" "$ENV_FILE" 2>/dev/null; then
  # openssl/xxd aren't guaranteed on a minimal image; od is coreutils (always present).
  _tok=$(openssl rand -hex 32 2>/dev/null || true)
  [[ -n "$_tok" ]] || _tok=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
  write_env_key "AGENTD_TOKEN" "$_tok"
  ok "AGENTD_TOKEN generated (bearer auth enabled)"
fi

# AGENTD_BIND — default the gateway to the LAN (0.0.0.0) so mesh peers can reach
# this node. A mesh listener on loopback is never useful for multi-node operation
# (it silently breaks inbound a2a / pairing — the peer connects out but nothing
# routes back). Safe here because a token is ALWAYS generated above: every /api +
# /ws route is bearer-gated, and agentd's F036 gate refuses a non-loopback bind
# without a token. The agentd *code* default stays loopback (protects a token-less
# raw `cargo run`); install.sh is the layer that guarantees a token, so it's where
# the LAN default belongs. Seed-if-absent: an operator who pinned 127.0.0.1 for a
# deliberately-private node is preserved, and an already-deployed loopback-only
# node gains LAN reach on its next `apexos-update`.
if ! grep -q "^AGENTD_BIND=" "$ENV_FILE" 2>/dev/null; then
  write_env_key "AGENTD_BIND" "0.0.0.0:8787"
  ok "AGENTD_BIND=0.0.0.0:8787 (mesh-reachable; token-gated)"
fi

write_env_key "ANTHROPIC_API_KEY"  "$API_KEY"
write_env_key "OPENROUTER_API_KEY" "$OPENROUTER_KEY"

# No new key resolved this run → either an existing env key is being PRESERVED
# (surface + verify it, so a stale one doesn't ride along silently) or there's none.
if [[ -z "$API_KEY" ]]; then
  _existing=$(_envval "$ENV_FILE" ANTHROPIC_API_KEY)
  if [[ -n "$_existing" ]]; then
    info "Keeping the existing Anthropic key in ${ENV_FILE}: $(mask_key "$_existing")"
    [[ "$(check_anthropic_key "$_existing")" == invalid ]] && \
      warn "…but Anthropic REJECTED that existing key (invalid/revoked). Update ANTHROPIC_API_KEY in ${ENV_FILE}."
  else
    warn "No Anthropic API key set. Add ANTHROPIC_API_KEY to $ENV_FILE before starting."
  fi
fi

ok "Config written"

# ── Systemd services ───────────────────────────────────────────────────────────
hdr "Systemd services"

install_svc() {
  local name="$1"
  local svc_file="$REPO_DIR/deploy/$name.service"
  [[ -f "$svc_file" ]] || die "Service file not found: $svc_file"
  install -m 644 "$svc_file" "/etc/systemd/system/$name.service"
  # Disable first so systemctl enable re-creates the symlink in the correct target.wants/
  systemctl disable "$name" 2>/dev/null || true
  ok "Service: $name.service installed"
}

install_svc agentd
install_svc apex-sensor-bridge
! $NO_CEREBRO_API && install_svc cerebro-api   || true
# The root KMS/DRM kiosk service is kiosk-mode only; desktop mode launches the UI
# as a user-session winit window (the .desktop launcher above), never this service.
! $NO_UI && ! $IS_DESKTOP && install_svc apexos-rs-ui  || true
# apex-tts only when voice provisioned (binary + model present) — else the service
# would crash-loop on a missing model.
$VOICE_INSTALLED && install_svc apex-tts || true
$STT_INSTALLED   && install_svc apex-stt || true

systemctl daemon-reload

systemctl enable agentd apex-sensor-bridge
! $NO_CEREBRO_API && systemctl enable cerebro-api  || true
! $NO_UI && ! $IS_DESKTOP && systemctl enable apexos-rs-ui || true
$VOICE_INSTALLED && systemctl enable --now apex-tts || true
$STT_INSTALLED   && systemctl enable --now apex-stt || true

ok "Services enabled"

# ── Self-update watchdog ────────────────────────────────────────────────────────
# The privileged (root) half of the daemon self-update loop (docs/self-update.md).
# agentd (non-root) can only WRITE /var/lib/agentd/update/request.json; the .path
# unit notices it and runs the watchdog as root to do the binary swap + rollback.
# Pre-installed + fixed here so the privileged code is auditable, never agent-authored.
hdr "Self-update watchdog"
install -d /usr/local/lib/apexos
install -m 755 "$REPO_DIR/deploy/apexos-self-update.sh"      /usr/local/lib/apexos/self-update.sh
install -m 644 "$REPO_DIR/deploy/apexos-self-update.service" /etc/systemd/system/apexos-self-update.service
install -m 644 "$REPO_DIR/deploy/apexos-self-update.path"    /etc/systemd/system/apexos-self-update.path
# Probation crash-loop guard (slice 5): agentd.service OnFailure → this rollback,
# fired when systemd's StartLimit trips on a latent post-confirm crash-loop.
install -m 755 "$REPO_DIR/deploy/apexos-rollback.sh"         /usr/local/lib/apexos/rollback.sh
install -m 644 "$REPO_DIR/deploy/apexos-rollback.service"    /etc/systemd/system/apexos-rollback.service
systemctl daemon-reload
# enable --now arms the .path watcher immediately (and idempotently on re-runs).
systemctl enable --now apexos-self-update.path >/dev/null 2>&1 || true
ok "Self-update watchdog + probation rollback installed"

# The opt-in provisioning command (option B — agentd-owned toolchain + repo). NOT
# run automatically: it installs ~1.5 GB of Rust toolchain and is Standard+ only.
# An operator runs `sudo apexos-provision-selfupdate` once on a node meant to
# self-evolve; then APEX can `apply_daemon_update`. See docs/self-update.md.
install -m 755 "$REPO_DIR/deploy/apexos-provision-selfupdate.sh" /usr/local/bin/apexos-provision-selfupdate
ok "Self-update provisioning command available (run: apexos-provision-selfupdate)"

# ── Self-update command ────────────────────────────────────────────────────────
# Drop an `apexos-update` so updates need zero cargo/git knowledge: it just re-runs
# this installer against the existing clone (pull → rebuild → hot-swap → restart).
# Self-escalates with sudo, so a normal user (or the UI as root) can invoke it.
hdr "Update command"
cat > /usr/local/bin/apexos-update <<'UPD'
#!/usr/bin/env bash
# ApexOS-RS self-update — pull latest, rebuild, hot-swap binaries + restart.
set -euo pipefail
if [[ $EUID -ne 0 ]]; then exec sudo -E "$0" "$@"; fi
echo "── ApexOS-RS update — pulling, building, hot-swapping… ──"
curl -fsSL https://raw.githubusercontent.com/buckster123/ApexOS-RS/main/install.sh | bash
UPD
chmod 755 /usr/local/bin/apexos-update
ok "apexos-update installed — run 'apexos-update' anytime to pull + rebuild"

# ── fastembed pre-warm ─────────────────────────────────────────────────────────
if [[ "$TIER" != "nano" ]]; then
  hdr "Embedding model pre-warm"
  info "Downloading $EMBED_MODEL (~128 MB first run) …"

  CEREBRO_ENV=(
    CEREBRO_DATA_DIR=/var/lib/agentd/cerebro
    FASTEMBED_CACHE_DIR=/var/lib/agentd/cerebro/models
    RUST_LOG=warn
  )
  [[ -n "$EMBED_MODEL" ]] && CEREBRO_ENV+=(CEREBRO_EMBED_MODEL="$EMBED_MODEL")

  printf '%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"prewarm","version":"0.1"}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"content":"ApexOS-RS install prewarm","memory_type":"semantic"}}}' \
  | sudo -u agentd env "${CEREBRO_ENV[@]}" /usr/local/bin/cerebro-mcp 2>/dev/null \
  | grep -q '"result"' \
    && ok "Embedding model cached" \
    || warn "Pre-warm incomplete — model downloads on first agentd start"
fi

# ── Start services ─────────────────────────────────────────────────────────────
hdr "Starting services"

svc_start() {
  local name="$1" label="${2:-$1}"
  systemctl restart "$name"
  local tries=0
  while (( tries < 8 )); do
    systemctl is-active "$name" &>/dev/null && { ok "$label running"; return 0; }
    sleep 1; (( tries++ ))
  done
  warn "$label failed to start — check: journalctl -u $name -n 20 --no-pager"
  return 1
}

svc_start agentd            "agentd"
$NO_SENSOR || svc_start apex-sensor-bridge "sensor-bridge"
$NO_CEREBRO_API || svc_start cerebro-api   "cerebro-api"
{ $NO_UI || $IS_DESKTOP; } || svc_start apexos-rs-ui  "apexos-rs-ui"

# ── Health check ──────────────────────────────────────────────────────────────
hdr "Health check"

HEALTH_PASS=0; HEALTH_FAIL=0
check() {
  local label="$1" result="$2"
  if [[ "$result" == "pass" ]]; then
    ok "$label"; (( HEALTH_PASS++ ))
  else
    warn "$label — $result"; (( HEALTH_FAIL++ ))
  fi
}

# agentd WebSocket responds — pass token if set
_hc_token=$(grep "^AGENTD_TOKEN=" "$ENV_FILE" 2>/dev/null | cut -d= -f2 || true)
_hc_auth=()
[[ -n "$_hc_token" ]] && _hc_auth=(-H "Authorization: Bearer $_hc_token")
if curl -sf --max-time 4 -o /dev/null \
    "${_hc_auth[@]}" \
    -H "Upgrade: websocket" -H "Connection: Upgrade" \
    -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
    -H "Sec-WebSocket-Version: 13" \
    "http://localhost:8787/ws" 2>/dev/null \
   || systemctl is-active agentd &>/dev/null; then
  check "agentd WS on :8787" "pass"
else
  check "agentd WS on :8787" "not responding"
fi

# cerebro tools loaded — check journal
if systemctl is-active agentd &>/dev/null; then
  TOOLS=$(journalctl -u agentd -n 30 --no-pager 2>/dev/null | grep -oP "\d+ tools" | head -1)
  [[ -n "$TOOLS" ]] \
    && check "cerebro-mcp ($TOOLS loaded)" "pass" \
    || check "cerebro-mcp tools" "not confirmed — check journalctl -u agentd"
fi

$NO_SENSOR      || { systemctl is-active apex-sensor-bridge &>/dev/null \
    && check "apex-sensor-bridge" "pass" \
    || check "apex-sensor-bridge" "not running"; }

$NO_CEREBRO_API || { systemctl is-active cerebro-api &>/dev/null \
    && check "cerebro-api on :8765" "pass" \
    || check "cerebro-api on :8765" "not running"; }

{ $NO_UI || $IS_DESKTOP; } || { systemctl is-active apexos-rs-ui &>/dev/null \
    && check "apexos-rs-ui (KMS display)" "pass" \
    || check "apexos-rs-ui (KMS display)" "not running"; }
$IS_DESKTOP && { [[ -x /usr/local/bin/apexos-rs-ui ]] \
    && check "apexos-rs-ui (desktop window — launch from app menu)" "pass" \
    || check "apexos-rs-ui (desktop)" "binary missing"; } || true

API_CHECK=""
grep -q "^ANTHROPIC_API_KEY=sk-" /etc/agentd/env 2>/dev/null \
  && check "Anthropic API key" "pass" \
  || { API_CHECK="not set"; check "Anthropic API key" "not set — add to $ENV_FILE"; }

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}  ✓ ApexOS-RS installed — tier: $TIER / mode: $MODE${NC}"
echo -e "  ${DIM}Health: ${HEALTH_PASS} passed, ${HEALTH_FAIL} warnings${NC}"
echo ""
echo -e "  ${DIM}Agent UI:         http://$(hostname -I | awk '{print $1}'):8787${NC}"
! $NO_CEREBRO_API && echo -e "  ${DIM}Cerebro dash:     http://localhost:8765${NC}" || true
! $NO_UI          && echo -e "  ${DIM}Display:          KMS/DRM on /dev/tty7 (1920×1080)${NC}" || true
[[ -n "$API_CHECK" ]] && \
  echo -e "  ${YELLOW}  ⚠  Set API key:  echo 'ANTHROPIC_API_KEY=sk-...' >> $ENV_FILE${NC}" || true
echo ""
echo -e "  ${DIM}Logs:    sudo journalctl -u agentd -f${NC}"
echo -e "  ${DIM}Install: $LOG${NC}"
echo -e "  ${DIM}Update:  apexos-update   (pull + rebuild + hot-swap)${NC}"
echo ""
if $SENSOR_I2C_PROVISIONED; then
  echo -e "  ${YELLOW}  🌡  Sensor head: I2C enabled — ${BOLD}reboot${NC}${YELLOW} to activate /dev/i2c-*.${NC}"
  echo -e "  ${DIM}     BME688/MLX90640 readings also need the external SensorHead dashboard"
  echo -e "       (github.com/buckster123/SensorHead) running + SENSORHEAD_URL set on the"
  echo -e "       bridge — see the sensor-head recipe in CLAUDE.md.${NC}"
  echo ""
fi

# ── TUI: Final summary ────────────────────────────────────────────────────────
if ! $YES && $HAVE_WHIPTAIL; then
  STATUS_BODY="ApexOS-RS is live on your device!\n\n"
  STATUS_BODY+="Health: ${HEALTH_PASS} checks passed"
  (( HEALTH_FAIL > 0 )) && STATUS_BODY+=", ${HEALTH_FAIL} warnings" || STATUS_BODY+="."
  STATUS_BODY+="\n\nAccess points:\n"
  STATUS_BODY+="  • Agent UI:  http://$(hostname -I | awk '{print $1}'):8787\n"
  ! $NO_CEREBRO_API && STATUS_BODY+="  • Cerebro:   http://localhost:8765\n" || true
  ! $NO_UI          && STATUS_BODY+="  • Display:   KMS/DRM on HDMI (auto-started)\n" || true
  [[ -n "$API_CHECK" ]] && STATUS_BODY+="\n⚠  Add your Anthropic key to $ENV_FILE\n   to enable LLM calls." || true
  $SENSOR_I2C_PROVISIONED && STATUS_BODY+="\n\n🌡  Sensor head: I2C enabled — REBOOT to activate.\n   BME688/MLX90640 also need the external SensorHead\n   dashboard + SENSORHEAD_URL (see CLAUDE.md)." || true
  STATUS_BODY+="\n\nInstall log: $LOG"
  tui_msg "Installation Complete" "$STATUS_BODY"
fi

echo "── ApexOS-RS install finished $(date) ──"
# Give the tee pipe time to flush before the process exits
sleep 1
