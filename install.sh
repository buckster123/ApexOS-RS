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
#   -y / --yes          Non-interactive (accept defaults)
#   --no-ui             Skip apexos-rs-ui (headless / server mode)
#   --no-cerebro-api    Skip cerebro-api REST dashboard
#   --no-voice          Skip whisper + piper wake-word
#   --api-key=KEY       Set ANTHROPIC_API_KEY non-interactively
#   --tier=TIER         nano | micro | standard | pro (default: auto-detect)
#   --mode=MODE         kiosk | headless | desktop (default: kiosk on Pi, headless otherwise)
#   --repo-dir=PATH     Use a local clone instead of fetching

set -euo pipefail
trap 'echo -e "\n${RED}  ✗ Install failed at line $LINENO${NC}" >&2' ERR

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
YES=false; NO_UI=false; NO_CEREBRO_API=false; NO_VOICE=false
API_KEY=""; TIER="auto"; MODE="auto"; REPO_DIR=""

for arg in "$@"; do
  case "$arg" in
    -y|--yes)              YES=true ;;
    --no-ui)               NO_UI=true ;;
    --no-cerebro-api)      NO_CEREBRO_API=true ;;
    --no-voice)            NO_VOICE=true ;;
    --api-key=*)           API_KEY="${arg#*=}" ;;
    --tier=*)              TIER="${arg#*=}" ;;
    --mode=*)              MODE="${arg#*=}" ;;
    --repo-dir=*)          REPO_DIR="${arg#*=}" ;;
    *) warn "unknown option: $arg" ;;
  esac
done

[[ $EUID -eq 0 ]] || die "Run as root: sudo bash install.sh"

BUILD_USER="${SUDO_USER:-root}"
BUILD_HOME=$(getent passwd "$BUILD_USER" | cut -d: -f6)

confirm() {
  $YES && return 0
  echo -e "${BOLD}  ?${NC} $1 [Y/n] "
  read -r reply </dev/tty
  [[ "${reply:-y}" =~ ^[Yy]$ ]]
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
[[ -f /proc/device-tree/model ]] && grep -qi "raspberry" /proc/device-tree/model 2>/dev/null && IS_PI=true

if [[ "$TIER" == "auto" ]]; then
  if   (( RAM_MB < 768  )); then TIER="nano"
  elif (( RAM_MB < 2048 )); then TIER="micro"
  elif (( RAM_MB < 8192 )); then TIER="standard"
  else                           TIER="pro"
  fi
fi

if [[ "$MODE" == "auto" ]]; then
  $IS_PI && MODE="kiosk" || MODE="headless"
fi

[[ "$MODE" == "headless" || "$MODE" == "desktop" ]] && NO_UI=true

info "Arch: $ARCH | RAM: ${RAM_MB}MB | Tier: $TIER | Mode: $MODE"
[[ "$TIER" == "nano" ]] && info "Nano tier: embedding disabled (FTS5 only), RSS target ~23 MB"
[[ "$TIER" == "micro" ]] && info "Micro tier: bge-small embedder (~275 MB RSS)"

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

PKGS=(curl git pkg-config build-essential)
if ! $NO_UI; then
  PKGS+=(libfontconfig1-dev)
fi

apt-get update -qq
apt-get install -y --no-install-recommends "${PKGS[@]}" 2>&1 | grep -E "(installed|upgraded|error)" || true
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
[[ -x "$CARGO" ]] || die "Rust install failed"
ok "Rust: $($CARGO --version)"

# ── User + groups ──────────────────────────────────────────────────────────────
hdr "User and permissions"

id agentd &>/dev/null || useradd -r -s /sbin/nologin -d /var/lib/agentd agentd

# UI group membership for KMS/DRM (kiosk mode only)
if ! $NO_UI; then
  for grp in render video input; do
    getent group "$grp" &>/dev/null && usermod -aG "$grp" agentd || true
  done
fi

mkdir -p \
  /etc/agentd \
  /var/lib/agentd/{workspace,events,ui,cerebro/models}

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
hdr "Building ApexOS-RS (single workspace)"
info "This takes ~5 min on Pi 5, ~2 min on x86 …"

cd "$REPO_DIR"

if $NO_UI; then
  EXCLUDE="--exclude ui-slint"
  info "Skipping ui-slint (headless/desktop mode)"
else
  EXCLUDE=""
fi

sudo -u "$BUILD_USER" "$CARGO" build --release --workspace $EXCLUDE 2>&1 \
  | grep -E "(Compiling agentd|Compiling cerebro|Compiling apexos|Finished|^error)" || true

ok "Build complete"

# ── Install binaries ───────────────────────────────────────────────────────────
hdr "Installing binaries"

BIN_DIR="$REPO_DIR/target/release"

install_bin() {
  local name="$1"
  [[ -x "$BIN_DIR/$name" ]] || die "$name not found — build may have failed"
  install -m 755 "$BIN_DIR/$name" "/usr/local/bin/$name"
  ok "$name → /usr/local/bin/$name ($(du -sh "$BIN_DIR/$name" | cut -f1))"
}

install_bin agentd
install_bin cerebro-mcp
install_bin cerebro-api
install_bin cerebro
install_bin apexos-tools
install_bin apex-sensor-bridge
! $NO_UI && install_bin ui-slint && install -m 755 "$BIN_DIR/ui-slint" /usr/local/bin/apexos-rs-ui || true

# ── Config ─────────────────────────────────────────────────────────────────────
hdr "Configuration"

# plugins.toml — cerebro embed model by tier
EMBED_MODEL=""
case "$TIER" in
  micro|standard) EMBED_MODEL="BAAI/bge-small-en-v1.5" ;;
  pro)            EMBED_MODEL="BAAI/bge-large-en-v1.5" ;;
esac

install -m 644 "$REPO_DIR/config/plugins.toml" /etc/agentd/plugins.toml

# Add embed model env to cerebro plugin if non-empty
if [[ -n "$EMBED_MODEL" ]]; then
  sed -i "/FASTEMBED_CACHE_DIR/a CEREBRO_EMBED_MODEL = \"$EMBED_MODEL\"" /etc/agentd/plugins.toml
fi

# policy.toml
if [[ ! -f /etc/agentd/policy.toml ]]; then
  cat > /etc/agentd/policy.toml << 'EOF'
mode = "suggest"

[rules]
"fs.read"     = "allow"
"fs.write"    = "workspace"
"fs.delete"   = "ask"
"shell.run"   = "ask"
"network"     = "ask"
EOF
fi

# env file — API key
if [[ -n "$API_KEY" ]]; then
  echo "ANTHROPIC_API_KEY=$API_KEY" > /etc/agentd/env
  chmod 600 /etc/agentd/env
  chown root:root /etc/agentd/env
elif [[ ! -f /etc/agentd/env ]]; then
  if confirm "Set ANTHROPIC_API_KEY now?"; then
    echo -n "  API key: "
    read -rs key </dev/tty; echo
    echo "ANTHROPIC_API_KEY=$key" > /etc/agentd/env
    chmod 600 /etc/agentd/env
    chown root:root /etc/agentd/env
    ok "API key saved to /etc/agentd/env"
  else
    warn "No API key set. Add ANTHROPIC_API_KEY to /etc/agentd/env before starting."
    touch /etc/agentd/env
    chmod 600 /etc/agentd/env
  fi
fi

ok "Config written"

# ── Systemd services ───────────────────────────────────────────────────────────
hdr "Systemd services"

install_svc() {
  local name="$1"
  install -m 644 "$REPO_DIR/deploy/$name.service" "/etc/systemd/system/$name.service"
  ok "Service: $name.service"
}

install_svc agentd
install_svc apex-sensor-bridge
! $NO_CEREBRO_API && install_svc cerebro-api || true
! $NO_UI          && install_svc apexos-rs-ui || true

systemctl daemon-reload

systemctl enable agentd apex-sensor-bridge
! $NO_CEREBRO_API && systemctl enable cerebro-api || true
! $NO_UI          && systemctl enable apexos-rs-ui || true

ok "Services enabled"

# ── fastembed pre-warm ─────────────────────────────────────────────────────────
if [[ "$TIER" != "nano" ]]; then
  hdr "fastembed model pre-warm"
  info "Downloading embedding model (~128 MB on first run) …"

  CEREBRO_ENV=(
    CEREBRO_DATA_DIR=/var/lib/agentd/cerebro
    FASTEMBED_CACHE_DIR=/var/lib/agentd/cerebro/models
    RUST_LOG=warn
  )
  [[ -n "$EMBED_MODEL" ]] && CEREBRO_ENV+=(CEREBRO_EMBED_MODEL="$EMBED_MODEL")

  printf '%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"prewarm","version":"0.1"}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remember","arguments":{"content":"ApexOS-RS install prewarm","memory_type":"semantic"}}}' \
  | sudo -u agentd env "${CEREBRO_ENV[@]}" /usr/local/bin/cerebro-mcp 2>/dev/null | grep -q '"result"' \
    && ok "fastembed model cached" \
    || warn "Pre-warm incomplete — model will download on first agentd start"
fi

# ── Start ──────────────────────────────────────────────────────────────────────
hdr "Starting services"

systemctl restart agentd
sleep 2
systemctl is-active agentd &>/dev/null && ok "agentd running" || warn "agentd failed to start — check: journalctl -u agentd -n 20"

systemctl restart apex-sensor-bridge
ok "apex-sensor-bridge started"

! $NO_CEREBRO_API && { systemctl restart cerebro-api; ok "cerebro-api started (http://localhost:8767)"; } || true
! $NO_UI          && { systemctl restart apexos-rs-ui; ok "apexos-rs-ui started"; } || true

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}  ✓ ApexOS-RS installed — tier: $TIER / mode: $MODE${NC}"
echo ""
echo -e "  ${DIM}Agent UI:       http://localhost:8787${NC}"
! $NO_CEREBRO_API && echo -e "  ${DIM}Cerebro dash:   http://localhost:8767${NC}" || true
! $NO_UI          && echo -e "  ${DIM}Display:        KMS/DRM on /dev/tty7${NC}" || true
echo ""
echo -e "  ${DIM}Logs: sudo journalctl -u agentd -f${NC}"
echo ""
