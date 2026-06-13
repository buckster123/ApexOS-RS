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
#   APEXOS_MODE=kiosk|headless|desktop      APEXOS_TIER=nano|micro|standard|pro
#   APEXOS_NO_UI=1   APEXOS_NO_SENSOR=0   APEXOS_NO_CEREBRO_API=0   APEXOS_VOICE=1
#
# Options:
#   -y / --yes              Force unattended (also implied when stdin isn't a TTY)
#   --tui                   Force the interactive menus even under curl|bash
#   --no-ui                 Skip apexos-rs-ui (headless / server mode)
#   --no-cerebro-api        Skip cerebro-api REST dashboard
#   --no-sensor             Skip apex-sensor-bridge (no sensorhead attached)
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
KEYFILE_NAMES=(apexos.env apexos-rs.env agentd.env apex.env apexos.txt apexos-key.txt env.txt)

# Set by find_key_file on success — keys plus optional install settings:
FOUND_ANTHROPIC=""; FOUND_OPENROUTER=""; FOUND_KEY_SRC=""
FOUND_MODE=""; FOUND_TIER=""; FOUND_NO_UI=""; FOUND_NO_SENSOR=""
FOUND_NO_CEREBRO_API=""; FOUND_VOICE=""

# Echo the value of KEY= in an env-style file (handles `export `, quotes, CRLF).
_envval() {
  grep -aE "^[[:space:]]*(export[[:space:]]+)?$2=" "$1" 2>/dev/null \
    | head -1 | sed -E 's/^[^=]*=//; s/^["'"'"']//; s/["'"'"']$//' | tr -d ' \r' || true
}

# Truthy test for config flags: yes/y/1/true/on (case-insensitive).
_truthy() { [[ "$1" =~ ^([Yy]([Ee][Ss])?|1|[Tt][Rr][Uu][Ee]|[Oo][Nn])$ ]]; }

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
  [[ -n "${FOUND_ANTHROPIC}${FOUND_OPENROUTER}${FOUND_MODE}${FOUND_TIER}${FOUND_NO_UI}${FOUND_NO_SENSOR}${FOUND_NO_CEREBRO_API}${FOUND_VOICE}" ]]
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
  [[ -z "$API_KEY"        && -n "$FOUND_ANTHROPIC"  ]] && API_KEY="$FOUND_ANTHROPIC"
  [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
  [[ "$TIER" == "auto" && -n "$FOUND_TIER" ]] && TIER="$FOUND_TIER"
  [[ "$MODE" == "auto" && -n "$FOUND_MODE" ]] && MODE="$FOUND_MODE"
  [[ -n "$FOUND_NO_UI"          ]] && { _truthy "$FOUND_NO_UI"          && NO_UI=true          || NO_UI=false; }
  [[ -n "$FOUND_NO_SENSOR"      ]] && { _truthy "$FOUND_NO_SENSOR"      && NO_SENSOR=true      || NO_SENSOR=false; }
  [[ -n "$FOUND_NO_CEREBRO_API" ]] && { _truthy "$FOUND_NO_CEREBRO_API" && NO_CEREBRO_API=true || NO_CEREBRO_API=false; }
  [[ -n "$FOUND_VOICE"          ]] && { _truthy "$FOUND_VOICE"          && NO_VOICE=false      || NO_VOICE=true; }
  local what="settings"; [[ -n "$FOUND_ANTHROPIC" ]] && what="key + settings"
  ok "Provisioned from ${FOUND_KEY_SRC} ($what)"
}

# ── Args ───────────────────────────────────────────────────────────────────────
YES=false; TUI_FORCE=false
# Sensor head OFF by default (most devices have no BME688/MLX90640 attached); a
# boot-file APEXOS_NO_SENSOR=false or the manual checklist turns it on.
NO_UI=false; NO_CEREBRO_API=false; NO_SENSOR=true; NO_VOICE=true
API_KEY=""; OPENROUTER_KEY=""
TIER="auto"; MODE="auto"; REPO_DIR=""

for arg in "$@"; do
  case "$arg" in
    -y|--yes)              YES=true ;;
    --tui)                 TUI_FORCE=true ;;
    --no-ui)               NO_UI=true ;;
    --no-cerebro-api)      NO_CEREBRO_API=true ;;
    --no-sensor)           NO_SENSOR=true ;;
    --no-voice)            NO_VOICE=true ;;
    --api-key=*)           API_KEY="${arg#*=}" ;;
    --openrouter-key=*)    OPENROUTER_KEY="${arg#*=}" ;;
    --tier=*)              TIER="${arg#*=}" ;;
    --mode=*)              MODE="${arg#*=}" ;;
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

# Load key + optional settings from a boot/USB file BEFORE filling tier/mode
# defaults, so an apexos.env / apexos.conf can steer an unattended install.
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
  pro)      TIER_DESC="Pro — bge-large + 30–70B local models (GPU)" ;;
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
  • apexos-rs-ui — native Slint UI (kiosk mode only)

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
[[ "$MODE" == "headless" || "$MODE" == "desktop" ]] && NO_UI=true
info "Mode: $MODE | Tier: $TIER | Arch: $ARCH"

# ── TUI: Addon checklist (manual only — auto uses sensible defaults) ──────────
if ! $YES && [[ "$STYLE" == "manual" ]]; then
  SENSOR_STATE="ON"; $NO_SENSOR && SENSOR_STATE="OFF"
  API_STATE="ON";    $NO_CEREBRO_API && API_STATE="OFF"
  UI_STATE="ON";     $NO_UI && UI_STATE="OFF"

  ADDONS=$(tui_checklist "Components" \
    "Select the components to install:\n(Space to toggle, Enter to confirm)" \
    "ui"      "apexos-rs-ui     Native Slint UI — KMS/DRM display"         "$UI_STATE" \
    "cerebro" "Cerebro API      REST dashboard + memory UI on :8765"        "$API_STATE" \
    "sensor"  "Sensor Head      BME688 air quality + MLX90640 thermal cam"  "$SENSOR_STATE" \
    "voice"   "Voice            Wake-word + whisper transcription"          "OFF")

  # Parse whiptail checklist output (space-separated quoted tags)
  echo "$ADDONS" | grep -q '"ui"'      || NO_UI=true
  echo "$ADDONS" | grep -q '"cerebro"' || NO_CEREBRO_API=true
  echo "$ADDONS" | grep -q '"sensor"'  && NO_SENSOR=false || NO_SENSOR=true
  echo "$ADDONS" | grep -q '"voice"'   && NO_VOICE=false  || NO_VOICE=true
fi

# ── API keys ──────────────────────────────────────────────────────────────────
# Priority: --api-key flag  >  key file on USB/SD-boot  >  manual TUI entry.
# The key is ~100 chars of "alien glyphs", so a pre-written file beats typing it.

if [[ -z "$API_KEY" ]]; then
  info "Looking for a key file (apexos.env) on USB / SD-boot media …"
  if find_key_file; then
    API_KEY="$FOUND_ANTHROPIC"
    [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
    ok "Loaded API key from ${FOUND_KEY_SRC}"
    ! $YES && tui_msg "Key file found 🎉" \
      "Loaded your Anthropic API key from:\n\n  ${FOUND_KEY_SRC}\n\nNo glyph-typing required."
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
          API_KEY="$FOUND_ANTHROPIC"
          [[ -z "$OPENROUTER_KEY" && -n "$FOUND_OPENROUTER" ]] && OPENROUTER_KEY="$FOUND_OPENROUTER"
          tui_msg "Key file found 🎉" "Loaded from:\n  ${FOUND_KEY_SRC}"
          break
        else
          tui_msg "Nothing found" \
            "Still no apexos.env on any USB or SD-boot partition.\n\nCheck the filename and that the device is plugged in,\nthen try again."
        fi ;;
      type)
        API_KEY=$(tui_input "Anthropic API Key" \
          "Paste or type your key (sk-ant-...):" "password") || true
        [[ -n "$API_KEY" ]] && break ;;
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
  ! $NO_UI          && ADDONS_LIST+="  ✓ apexos-rs-ui  (KMS/DRM display)\n"
  ! $NO_CEREBRO_API && ADDONS_LIST+="  ✓ cerebro-api   (REST dashboard :8765)\n"
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
    if [[ "$BUILD_USER" != "root" ]]; then
      sudo -u "$BUILD_USER" git -C "$REPO_DIR" pull --ff-only
    else
      git -C "$REPO_DIR" pull --ff-only
    fi
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
PKGS=(curl git pkg-config build-essential libssl-dev whiptail ffmpeg)
if ! $NO_UI; then
  PKGS+=(libfontconfig1-dev libgbm-dev libegl-dev libudev-dev libinput-dev libxkbcommon-dev)
fi

apt-get update -qq
apt-get install -y --no-install-recommends "${PKGS[@]}" 2>&1 \
  | grep -E "(installed|upgraded|error)" || true
ok "System packages installed"

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

if ! $NO_UI; then
  for grp in render video input; do
    getent group "$grp" &>/dev/null && usermod -aG "$grp" agentd || true
  done
fi

mkdir -p /etc/agentd /var/lib/agentd/{workspace,events,ui,cerebro/models}
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

BUILD_ARGS="--release --workspace"
if $NO_UI; then
  BUILD_ARGS+=" --exclude ui-slint"
  info "Skipping ui-slint (headless/desktop mode)"
fi

BUILD_LOG=$(mktemp /tmp/apexos-cargo-build.XXXXXX.log)
info "Build log → $BUILD_LOG"

sudo -u "$BUILD_USER" "$CARGO" build $BUILD_ARGS 2>&1 \
  | tee "$BUILD_LOG" \
  | grep --line-buffered -E "(^Compiling (agentd|cerebro|apexos|ui-slint|apex)|Finished|^error)" \
  || true

if grep -q "^error" "$BUILD_LOG"; then
  cat "$BUILD_LOG" | grep "^error" | head -5
  die "Build failed — see $BUILD_LOG for full output"
fi

ok "Build complete"
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
fi

# ── Config ─────────────────────────────────────────────────────────────────────
hdr "Configuration"

# plugins.toml
EMBED_MODEL=""
case "$TIER" in
  micro|standard) EMBED_MODEL="BAAI/bge-small-en-v1.5" ;;
  pro)            EMBED_MODEL="BAAI/bge-large-en-v1.5" ;;
esac

install -m 644 "$REPO_DIR/config/plugins.toml" /etc/agentd/plugins.toml
if [[ -n "$EMBED_MODEL" ]]; then
  sed -i "/FASTEMBED_CACHE_DIR/a CEREBRO_EMBED_MODEL = \"$EMBED_MODEL\"" /etc/agentd/plugins.toml
fi

# policy.toml (don't overwrite an existing policy)
if [[ ! -f /etc/agentd/policy.toml ]]; then
  install -m 644 "$REPO_DIR/config/policy.toml" /etc/agentd/policy.toml
fi

# soul.md — APEX's identity / system prompt (created from the default if missing).
if [[ ! -f /etc/agentd/soul.md ]]; then
  install -m 644 "$REPO_DIR/config/soul.md" /etc/agentd/soul.md
fi

# peers.toml — mesh registry (agentd writes it at runtime; seed empty if missing).
if [[ ! -f /etc/agentd/peers.toml ]]; then
  echo "# ApexOS mesh peers" > /etc/agentd/peers.toml
fi

# Agent-mutable configs must be owned by the agentd user so the daemon can rewrite
# them: Settings save (soul), and self-evolution (update_system_prompt / update_policy_rule
# / register_mcp_server) writing soul.md, policy.toml, plugins.toml, peers.toml.
# /etc/agentd itself stays root-owned so the env file (auth token, 600 root:root) is
# protected — we chown the individual files, not the directory. (write_atomic falls
# back to an in-place write when the root-owned dir blocks the temp+rename path.)
chown agentd:agentd /etc/agentd/soul.md /etc/agentd/policy.toml \
                    /etc/agentd/plugins.toml /etc/agentd/peers.toml

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

write_env_key "ANTHROPIC_API_KEY"  "$API_KEY"
write_env_key "OPENROUTER_API_KEY" "$OPENROUTER_KEY"

if [[ -z "$API_KEY" ]] && ! grep -q "^ANTHROPIC_API_KEY=" "$ENV_FILE" 2>/dev/null; then
  warn "No Anthropic API key set. Add ANTHROPIC_API_KEY to $ENV_FILE before starting."
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
! $NO_UI          && install_svc apexos-rs-ui  || true

systemctl daemon-reload

systemctl enable agentd apex-sensor-bridge
! $NO_CEREBRO_API && systemctl enable cerebro-api  || true
! $NO_UI          && systemctl enable apexos-rs-ui || true

ok "Services enabled"

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
$NO_UI          || svc_start apexos-rs-ui  "apexos-rs-ui"

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

$NO_UI          || { systemctl is-active apexos-rs-ui &>/dev/null \
    && check "apexos-rs-ui (KMS display)" "pass" \
    || check "apexos-rs-ui (KMS display)" "not running"; }

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
  STATUS_BODY+="\n\nInstall log: $LOG"
  tui_msg "Installation Complete" "$STATUS_BODY"
fi

echo "── ApexOS-RS install finished $(date) ──"
# Give the tee pipe time to flush before the process exits
sleep 1
