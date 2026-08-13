#!/bin/sh
# apexos-self-update.sh — the agentd self-update watchdog (docs/self-update.md, slice 2).
#
# Runs as ROOT (systemd oneshot, triggered by apexos-self-update.path the moment
# /var/lib/agentd/update/request.json appears). This is the SURVIVOR: deliberately
# simpler + more robust than the daemon it supervises — POSIX sh, no compilation,
# external tools limited to coreutils + systemctl (+ jq if present, sed fallback
# otherwise). agentd (non-root) only ever *writes a request*; this privileged swap
# lives behind that boundary so a buggy/compromised agentd cannot brick the node.
#
# INVARIANT: recoverability. Every exit path leaves /usr/local/bin/agentd a
# known-good binary. Pipeline: verify staged sha → back up the live binary →
# atomic swap → restart → poll the health marker → CONFIRM, or ROLL BACK by
# restoring the kept agentd.prev artifact (instant, can't fail to compile).
#
# POWER-LOSS SAFE + IDEMPOTENT via a phase file ($STATE = BACKED_UP | SWAPPED):
# a reboot mid-swap re-triggers this script, which re-enters at the recorded phase
# and never re-backs-up (which would overwrite the good backup with the new binary).

set -u

# Production defaults; all overridable for the forced-rollback / power-loss DRILLS
# (the systemd oneshot runs with a clean env, so production always uses defaults —
# no accidental redirect). Keep AGENTD_UPDATE_DIR in sync with agentd's if you ever
# override it there (see docs/self-update.md).
UPDATE_DIR="${AGENTD_UPDATE_DIR:-/var/lib/agentd/update}"
BIN="${APEXOS_SELF_UPDATE_BIN:-/usr/local/bin/agentd}"
PREV="${BIN}.prev"
SYSTEMCTL="${APEXOS_SELF_UPDATE_SYSTEMCTL:-systemctl}"
REQ="$UPDATE_DIR/request.json"
HEALTH="$UPDATE_DIR/health.json"
STATE="$UPDATE_DIR/state"           # phase marker: BACKED_UP | SWAPPED
LOCK="$UPDATE_DIR/watchdog.lock"
# Fixed staging path — NEVER read a `staged` field from request.json. An
# agentd-supplied path was a root-as-rm primitive (finding 5).
STAGED="$UPDATE_DIR/agentd.staged"
POLL_INTERVAL="${APEXOS_SELF_UPDATE_POLL:-2}"

log()  { echo "[self-update] $*"; }
now()  { date +%s; }
sctl() { "$SYSTEMCTL" "$@"; }       # systemctl, override for drills

# Read one flat string/number field from a JSON file. jq when available; a
# line-oriented sed fallback for the pretty-printed JSON agentd emits.
jget() { # $1=file $2=key
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg k "$2" '.[$k] // empty' "$1" 2>/dev/null
  else
    grep -m1 "\"$2\"" "$1" 2>/dev/null \
      | sed 's/^[^:]*:[[:space:]]*//; s/^"//; s/"\{0,1\},\{0,1\}[[:space:]]*$//'
  fi
}

write_outcome() { # $1=basename(confirmed.json|rolled-back.json|rejected.json) $2=reason
  cat > "$UPDATE_DIR/$1" <<EOF
{
  "outcome": "$(printf '%s' "${1%.json}")",
  "reason": "$(printf '%s' "$2")",
  "target_commit": "$(printf '%s' "$TARGET")",
  "prev_commit": "$(printf '%s' "$PREVC")",
  "ts": $(now)
}
EOF
}

# Clear the request + phase so the .path unit can re-fire on the next request, and
# drop the (now consumed) staged binary. Only the hard-coded path is removed —
# never a caller-selected one.
finish() {
  rm -f "$STAGED" 2>/dev/null
  rm -f "$REQ" "$STATE"
}

is_sha256() {
  printf '%s' "$1" | grep -Eq '^[0-9a-fA-F]{64}$'
}

# Regular file, not a symlink, same uid as request.json (the agentd user in
# prod; the drill user in the harness). lstat via -L before -f so we never
# follow a planted symlink.
staged_file_ok() {
  [ -e "$STAGED" ] || return 1
  [ ! -L "$STAGED" ] || return 1
  [ -f "$STAGED" ] || return 1
  # Owner check needs stat (always present in prod). The sed-fallback drill
  # strips PATH to coreutils used for JSON parse — skip if stat is absent.
  if command -v stat >/dev/null 2>&1; then
    _ru=$(stat -c %u "$REQ" 2>/dev/null) || return 1
    _su=$(stat -c %u "$STAGED" 2>/dev/null) || return 1
    [ "$_ru" = "$_su" ] || return 1
  fi
  return 0
}

# The running daemon's marker proves the NEW binary booted healthy: status healthy,
# the requested target commit, and a boot timestamp AFTER the request was filed
# (so a stale marker from before the swap can never read as success).
is_healthy_target() {
  [ -f "$HEALTH" ] || return 1
  _st=$(jget "$HEALTH" status)
  _cm=$(jget "$HEALTH" commit)
  _ba=$(jget "$HEALTH" booted_at)
  [ "$_st" = "healthy" ] || return 1
  [ "$_cm" = "$TARGET" ] || return 1
  [ -n "$_ba" ] || return 1
  [ "$_ba" -ge "$CREATED" ] 2>/dev/null || return 1
  return 0
}

poll_health() {
  _deadline=$(( $(now) + TIMEOUT ))
  while [ "$(now)" -lt "$_deadline" ]; do
    is_healthy_target && return 0
    # A cleanly-dead unit (not auto-restarting) means a crash — stop waiting early.
    if [ "$(sctl is-active agentd 2>/dev/null)" = "failed" ]; then
      log "agentd unit failed during probe"
      return 1
    fi
    sleep "$POLL_INTERVAL"
  done
  return 1
}

# Atomic same-directory rename: copy to a temp name on the target filesystem, then
# rename over the live binary. The binary is never observed half-written.
# When $2 is a sha256, the COPY is re-hashed so a symlink swap between the
# pre-check and cp cannot land unattested bytes.
swap_in() { # $1=source binary  $2=optional expected sha256
  cp -f "$1" "$BIN.new.$$" || return 1
  if [ -n "${2:-}" ]; then
    _got=$(sha256sum "$BIN.new.$$" 2>/dev/null | cut -d' ' -f1)
    if [ "$_got" != "$2" ]; then
      rm -f "$BIN.new.$$"
      return 1
    fi
  fi
  mv -f "$BIN.new.$$" "$BIN"
}

rollback() { # $1=reason
  log "ROLLBACK: $1"
  if [ -f "$PREV" ]; then
    sctl stop agentd 2>/dev/null
    if swap_in "$PREV"; then
      sctl start agentd 2>/dev/null
      write_outcome rolled-back.json "$1"
      log "restored known-good agentd.prev ($PREVC)"
    else
      sctl start agentd 2>/dev/null
      write_outcome rolled-back.json "$1 (restore mv failed!)"
      log "WARNING: restore of agentd.prev failed"
    fi
  else
    write_outcome rolled-back.json "$1 (no agentd.prev to restore!)"
    log "WARNING: no agentd.prev present — cannot restore"
  fi
  finish
}

# ── serialize concurrent triggers ──────────────────────────────────────────────
if command -v flock >/dev/null 2>&1; then
  exec 9>"$LOCK"
  flock -n 9 || { log "another watchdog run holds the lock — exiting"; exit 0; }
fi

[ -f "$REQ" ] || { log "no request.json — nothing to do"; exit 0; }

# `staged` in the JSON is ignored on purpose. Path is $STAGED above.
STAGED_SHA=$(jget "$REQ" staged_sha256)
TARGET=$(jget "$REQ" target_commit)
PREVC=$(jget "$REQ" prev_commit)
CREATED=$(jget "$REQ" created_at)
TIMEOUT=$(jget "$REQ" timeout)
[ -n "$TIMEOUT" ] || TIMEOUT=120

reject() { # $1=log  $2=outcome reason
  log "$1 — rejecting, daemon untouched"
  write_outcome rejected.json "$2"
  finish
  exit 0
}

if ! is_sha256 "$STAGED_SHA"; then
  reject "staged_sha256 missing or not 64 hex" "staged_sha256 required"
fi
if [ -z "$TARGET" ]; then
  reject "target_commit missing" "target_commit required"
fi
echo "$CREATED" | grep -Eq '^[0-9]+$' || reject "created_at not numeric" "created_at required"
echo "$TIMEOUT" | grep -Eq '^[0-9]+$' || reject "timeout not numeric" "timeout required"

PHASE=""
[ -f "$STATE" ] && PHASE=$(cat "$STATE" 2>/dev/null)
log "request: target=$TARGET prev=$PREVC timeout=${TIMEOUT}s phase='${PHASE:-fresh}'"

# ── reconciliation: post-swap power loss ────────────────────────────────────────
# If we already swapped and the new binary is up + healthy at target, the update
# succeeded even though we never recorded it — confirm without touching anything.
if [ "$PHASE" = "SWAPPED" ] && is_healthy_target; then
  write_outcome confirmed.json "healthy after swap (reconciled at boot)"
  log "confirmed (reconciled): $TARGET healthy"
  finish
  exit 0
fi

# Resuming a completed swap (power loss during the health probe): do NOT re-backup
# or re-swap — agentd.prev is the good old binary, the new one is already in place.
# Just judge health and confirm or roll back.
if [ "$PHASE" = "SWAPPED" ]; then
  log "resuming post-swap health poll for $TARGET"
  if poll_health; then
    write_outcome confirmed.json "healthy after swap"
    log "confirmed: $TARGET healthy"
    finish
  else
    rollback "health timeout after swap"
  fi
  exit 0
fi

# ── stage A: verify staged binary (PRE-SWAP — live daemon untouched on failure) ──
if ! staged_file_ok; then
  reject "staged binary missing, symlink, or wrong owner ($STAGED)" "staged binary missing or unsafe"
fi
actual=$(sha256sum "$STAGED" 2>/dev/null | cut -d' ' -f1)
if [ "$actual" != "$STAGED_SHA" ]; then
  reject "sha256 mismatch (want=$STAGED_SHA got=$actual)" "staged sha256 mismatch"
fi
log "staged sha256 verified"

# ── stage B: back up the current known-good binary (FRESH runs only) ─────────────
if [ "$PHASE" != "BACKED_UP" ]; then
  if ! cp -f "$BIN" "$PREV"; then
    log "backup of live binary failed — rejecting, daemon untouched"
    write_outcome rejected.json "backup failed"
    finish; exit 0
  fi
  echo BACKED_UP > "$STATE"
  log "backed up current binary → agentd.prev"
fi

# ── stage C: swap (stop → atomic rename → start) + mark SWAPPED ──────────────────
sctl stop agentd 2>/dev/null
if ! swap_in "$STAGED" "$STAGED_SHA"; then
  log "swap failed — restoring backup"
  swap_in "$PREV"
  sctl start agentd 2>/dev/null
  write_outcome rolled-back.json "swap failed"
  finish; exit 0
fi
echo SWAPPED > "$STATE"
sctl start agentd 2>/dev/null
log "swapped in staged binary; polling health (timeout ${TIMEOUT}s)"

# ── stage D: health-gated confirm / rollback ─────────────────────────────────────
if poll_health; then
  write_outcome confirmed.json "healthy after swap"
  log "confirmed: $TARGET healthy"
  finish
else
  rollback "health timeout"
fi
exit 0
