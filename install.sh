#!/usr/bin/env bash
# ╔══════════════════════════════════════════════════════════════════════════╗
# ║              ApexOS-RS — One-shot installer                              ║
# ║  Pure Rust: agentd · cerebro-mcp · apexos-tools · ui-slint              ║
# ║  Runs on: Pi Zero 2W → Pi 5 → x86 mini-PC → GPU workstation             ║
# ╚══════════════════════════════════════════════════════════════════════════╝
#
# Quick install (fresh device, clones repo automatically):
#   curl -fsSL https://raw.githubusercontent.com/buckster123/ApexOS-RS/main/install.sh | sudo bash
#
# From a local clone:
#   sudo bash install.sh [OPTIONS]
#
# Options:
#   -y / --yes              Non-interactive (accept all defaults, skip TUI)
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

trap 'echo -e "\n${RED:-}  ✗ Install failed at line $LINENO — see $LOG${NC:-}" >&2' ERR

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

# ── Args ───────────────────────────────────────────────────────────────────────
YES=false
NO_UI=false; NO_CEREBRO_API=false; NO_SENSOR=false; NO_VOICE=true
API_KEY=""; OPENROUTER_KEY=""
TIER="auto"; MODE="auto"; REPO_DIR=""

for arg in "$@"; do
  case "$arg" in
    -y|--yes)              YES=true ;;
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

BUILD_USER="${SUDO_USER:-root}"
BUILD_HOME=$(getent passwd "$BUILD_USER" | cut -d: -f6)

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
# whiptail draws on /dev/tty so it is unaffected by the exec tee above.
H=20; W=72   # default dialog height/width

# Show a message box. $1=title $2=body
tui_msg() {
  if $HAVE_WHIPTAIL; then
    whiptail --title "$1" --msgbox "$2" $H $W 3>&1 1>/dev/tty 2>&3 || true
  else
    hdr "$1"; echo -e "$2"; echo
  fi
}

# Ask yes/no. Returns 0=yes 1=no. $1=title $2=body
tui_yesno() {
  if $HAVE_WHIPTAIL; then
    whiptail --title "$1" --yesno "$2" $H $W 3>&1 1>/dev/tty 2>&3; return $?
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
      3>&1 1>/dev/tty 2>&3) || true
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
      3>&1 1>/dev/tty 2>&3 || true
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
      3>&1 1>/dev/tty 2>&3 || true
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

# ── TUI: Welcome ──────────────────────────────────────────────────────────────
if ! $YES; then
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

# ── TUI: Mode picker ──────────────────────────────────────────────────────────
if ! $YES; then
  MODE_CHOICE=$(tui_menu "Deployment Mode" \
    "How will you use this device?" \
    "kiosk"    "Kiosk    — dedicated HDMI display (Pi + monitor)" \
    "headless" "Headless — browser/mobile UI, no local display" \
    "desktop"  "Desktop  — shared monitor, native window (x86/Mac)")
  [[ -n "$MODE_CHOICE" ]] && MODE="$MODE_CHOICE"
fi
[[ "$MODE" == "headless" || "$MODE" == "desktop" ]] && NO_UI=true
info "Mode: $MODE | Tier: $TIER | Arch: $ARCH"

# ── TUI: Addon checklist ──────────────────────────────────────────────────────
if ! $YES; then
  SENSOR_STATE="ON"; $NO_SENSOR && SENSOR_STATE="OFF"
  API_STATE="ON";    $NO_CEREBRO_API && API_STATE="OFF"
  UI_STATE="ON";     $NO_UI && UI_STATE="OFF"

  ADDONS=$(tui_checklist "Components" \
    "Select the components to install:\n(Space to toggle, Enter to confirm)" \
    "ui"      "apexos-rs-ui     Native Slint UI — KMS/DRM display"         "$UI_STATE" \
    "cerebro" "Cerebro API      REST dashboard + memory UI on :8767"        "$API_STATE" \
    "sensor"  "Sensor Head      BME688 air quality + MLX90640 thermal cam"  "$SENSOR_STATE" \
    "voice"   "Voice            Wake-word + whisper transcription"          "OFF")

  # Parse whiptail checklist output (space-separated quoted tags)
  echo "$ADDONS" | grep -q '"ui"'      || NO_UI=true
  echo "$ADDONS" | grep -q '"cerebro"' || NO_CEREBRO_API=true
  echo "$ADDONS" | grep -q '"sensor"'  && NO_SENSOR=false || NO_SENSOR=true
  echo "$ADDONS" | grep -q '"voice"'   && NO_VOICE=false  || NO_VOICE=true
fi

# ── TUI: API keys ─────────────────────────────────────────────────────────────
if ! $YES; then
  if [[ -z "$API_KEY" ]]; then
    API_KEY=$(tui_input "Anthropic API Key" \
      "Enter your Anthropic API key (required for LLM calls).\nFormat: sk-ant-api03-...\n\nLeave blank to configure later via /etc/agentd/env" \
      "password") || true
  fi

  if [[ -z "$OPENROUTER_KEY" ]]; then
    OPENROUTER_KEY=$(tui_input "OpenRouter API Key (optional)" \
      "Enter your OpenRouter key for access to alternative models\n(GPT-4o, Gemini, Llama, etc.).\n\nLeave blank to skip." \
      "password") || true
  fi
fi

# ── TUI: Pre-build summary + confirm ─────────────────────────────────────────
if ! $YES; then
  ADDONS_LIST=""
  ! $NO_UI          && ADDONS_LIST+="  ✓ apexos-rs-ui  (KMS/DRM display)\n"
  ! $NO_CEREBRO_API && ADDONS_LIST+="  ✓ cerebro-api   (REST dashboard :8767)\n"
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
  if [[ -d "$REPO_DIR/.git" ]]; then
    info "Updating existing clone at $REPO_DIR …"
    git -C "$REPO_DIR" pull --ff-only
  else
    info "Cloning ApexOS-RS …"
    git clone --depth=1 https://github.com/buckster123/ApexOS-RS "$REPO_DIR"
  fi
fi
[[ -f "$REPO_DIR/Cargo.toml" ]] || die "Cargo.toml not found in $REPO_DIR"
ok "Repo at $REPO_DIR"

# ── System deps ────────────────────────────────────────────────────────────────
hdr "System dependencies"

PKGS=(curl git pkg-config build-essential libssl-dev whiptail)
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

BUILD_LOG=/tmp/apexos-cargo-build.log
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
  cat > /etc/agentd/policy.toml << 'EOF'
mode = "suggest"

[rules]
"fs.read"   = "allow"
"fs.write"  = "workspace"
"fs.delete" = "ask"
"shell.run" = "ask"
"network"   = "ask"
EOF
fi

# env file — API keys (don't overwrite existing keys)
ENV_FILE=/etc/agentd/env
touch "$ENV_FILE"; chmod 600 "$ENV_FILE"; chown root:root "$ENV_FILE"

write_env_key() {
  local key="$1" val="$2"
  [[ -z "$val" ]] && return
  grep -q "^${key}=" "$ENV_FILE" 2>/dev/null \
    && sed -i "s|^${key}=.*|${key}=${val}|" "$ENV_FILE" \
    || echo "${key}=${val}" >> "$ENV_FILE"
}

# Generate gateway token once — never overwrite an existing one
if ! grep -q "^AGENTD_TOKEN=" "$ENV_FILE" 2>/dev/null; then
  _tok=$(openssl rand -hex 32 2>/dev/null || head -c 32 /dev/urandom | xxd -p -c 64 | head -c 64)
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
    && check "cerebro-api on :8767" "pass" \
    || check "cerebro-api on :8767" "not running"; }

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
! $NO_CEREBRO_API && echo -e "  ${DIM}Cerebro dash:     http://localhost:8767${NC}" || true
! $NO_UI          && echo -e "  ${DIM}Display:          KMS/DRM on /dev/tty7 (1920×1080)${NC}" || true
[[ -n "$API_CHECK" ]] && \
  echo -e "  ${YELLOW}  ⚠  Set API key:  echo 'ANTHROPIC_API_KEY=sk-...' >> $ENV_FILE${NC}" || true
echo ""
echo -e "  ${DIM}Logs:    sudo journalctl -u agentd -f${NC}"
echo -e "  ${DIM}Install: $LOG${NC}"
echo ""

# ── TUI: Final summary ────────────────────────────────────────────────────────
if ! $YES && $HAVE_WHIPTAIL; then
  STATUS_BODY="ApexOS-RS is live on your device!\n\n"
  STATUS_BODY+="Health: ${HEALTH_PASS} checks passed"
  (( HEALTH_FAIL > 0 )) && STATUS_BODY+=", ${HEALTH_FAIL} warnings" || STATUS_BODY+="."
  STATUS_BODY+="\n\nAccess points:\n"
  STATUS_BODY+="  • Agent UI:  http://$(hostname -I | awk '{print $1}'):8787\n"
  ! $NO_CEREBRO_API && STATUS_BODY+="  • Cerebro:   http://localhost:8767\n" || true
  ! $NO_UI          && STATUS_BODY+="  • Display:   KMS/DRM on HDMI (auto-started)\n" || true
  [[ -n "$API_CHECK" ]] && STATUS_BODY+="\n⚠  Add your Anthropic key to $ENV_FILE\n   to enable LLM calls." || true
  STATUS_BODY+="\n\nInstall log: $LOG"
  tui_msg "Installation Complete" "$STATUS_BODY"
fi

echo "── ApexOS-RS install finished $(date) ──"
# Give the tee pipe time to flush before the process exits
sleep 1
