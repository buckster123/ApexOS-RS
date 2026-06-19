#!/usr/bin/env bash
# apexos-provision-selfupdate — enable APEX daemon self-update on this node.
#
# Option B (docs/self-update.md, slice 3.2): give the sandboxed non-root `agentd`
# user its OWN build path — a Rust toolchain + a self-update repo clone under
# /var/lib/agentd, plus the /etc/agentd/env wiring — so `apply_daemon_update` can
# actually compile a new agentd. Nothing here is shared with the human dev user or
# the operator's apexos-update clone, so the two never fight over git ownership.
#
# Opt-in + idempotent: re-running fetches/skips. Standard+ tier only — a Nano board
# can't compile a Rust workspace. Installs ~1.5 GB of toolchain, so it is NOT part
# of the default install; an operator runs this once on a node meant to self-evolve.
set -euo pipefail
[[ $EUID -eq 0 ]] || exec sudo "$0" "$@"

REPO_URL="${APEXOS_REPO_URL:-https://github.com/buckster123/ApexOS-RS.git}"
SU_HOME=/var/lib/agentd
SU_DIR="$SU_HOME/self-update"
SU_REPO="$SU_DIR/ApexOS-RS"
CARGO_HOME="$SU_HOME/.cargo"
RUSTUP_HOME="$SU_HOME/.rustup"
CARGO_BIN="$CARGO_HOME/bin/cargo"
ENV_FILE=/etc/agentd/env

id agentd &>/dev/null || { echo "✗ no 'agentd' user — run install.sh first"; exit 1; }
command -v curl >/dev/null || { echo "✗ curl required"; exit 1; }
command -v git  >/dev/null || { echo "✗ git required"; exit 1; }

# Run a command as the agentd user with the sandboxed toolchain env.
as_agentd() {
  sudo -u agentd env HOME="$SU_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
    CARGO_HOME="$CARGO_HOME" PATH="$CARGO_HOME/bin:/usr/bin:/bin" "$@"
}

echo "── Provisioning agentd self-update (option B) ──"
install -d -o agentd -g agentd "$SU_DIR"

# 1. Rust toolchain in the agentd sandbox (idempotent). nologin shell is fine —
#    sudo runs the given command, not a login shell.
if [[ -x "$CARGO_BIN" ]]; then
  echo "✓ toolchain present: $CARGO_BIN"
else
  echo "→ installing Rust toolchain as agentd (downloads a few hundred MB)…"
  as_agentd bash -c \
    'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path'
  echo "✓ toolchain installed"
fi

# 2. agentd-owned self-update clone — APEX's own evolution repo (idempotent).
if [[ -d "$SU_REPO/.git" ]]; then
  as_agentd git -C "$SU_REPO" fetch --quiet origin || true
  echo "✓ self-update repo present: $SU_REPO"
else
  echo "→ cloning $REPO_URL → $SU_REPO…"
  as_agentd git clone --quiet "$REPO_URL" "$SU_REPO"
  echo "✓ cloned"
fi

# 3. Wire /etc/agentd/env (idempotent marker block). The file stays root:root 600
#    (it holds AGENTD_TOKEN) — systemd reads it as root before dropping to agentd.
touch "$ENV_FILE"; chmod 600 "$ENV_FILE"
sed -i '/# >>> apexos self-update >>>/,/# <<< apexos self-update <<</d' "$ENV_FILE"
cat >> "$ENV_FILE" <<EOF
# >>> apexos self-update >>>
AGENTD_CARGO=$CARGO_BIN
CARGO_HOME=$CARGO_HOME
RUSTUP_HOME=$RUSTUP_HOME
AGENTD_SELF_UPDATE_REPO=$SU_REPO
AGENTD_GIT_ROOTS=$SU_REPO
# <<< apexos self-update <<<
EOF
echo "✓ wired $ENV_FILE"

# 4. Warm build — proves agentd can self-compile AND primes the cache so the first
#    real apply_daemon_update is incremental, not a 20-min cold build. Non-fatal:
#    the env is wired regardless so the failure is visible + retryable.
echo "→ warm build (first compile is slow; this is the real option-B validation)…"
if as_agentd bash -c "cd '$SU_REPO' && '$CARGO_BIN' build --release -p agentd"; then
  echo "✓ warm build OK — agentd CAN self-compile"
  WARM_OK=1
else
  echo "⚠ warm build FAILED — check the toolchain/deps above; env is still wired"
  WARM_OK=0
fi

# 5. Restart agentd to pick up the env.
systemctl restart agentd 2>/dev/null || true
echo
echo "── Self-update provisioned ──"
echo "  repo:      $SU_REPO   (agentd-owned; APEX edits + commits + builds here)"
echo "  toolchain: $CARGO_BIN"
echo "  warm build: $([[ ${WARM_OK:-0} = 1 ]] && echo OK || echo FAILED)"
echo "  next: have APEX run  apply_daemon_update(commit=<HEAD>, dry_run=true)"
