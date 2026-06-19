#!/bin/sh
# apexos-rollback.sh — probation crash-loop guard (self-update slice 5).
#
# Triggered by `agentd.service`'s `OnFailure=` when systemd's StartLimit trips —
# i.e. agentd crash-looped. This catches the one case the in-window watchdog can't:
# a LATENT crash — a freshly self-updated binary that booted healthy (so the
# watchdog already CONFIRMED + exited), then died minutes later.
#
# It rolls back to agentd.prev ONLY when this is plausibly a self-update regression:
# a RECENT `confirmed.json` exists and agentd.prev is present. An unrelated
# crash-loop (no recent confirm) is left to systemd — we don't mask non-update bugs,
# and we NEVER loop: the confirm marker is consumed, so a crash-loop of the restored
# binary can't re-trigger a rollback.
#
# Runs as root (oneshot). Paths + systemctl are env-overridable for the drill; the
# systemd unit runs with a clean env, so production uses the hard-coded defaults.
set -u

UPDATE_DIR="${AGENTD_UPDATE_DIR:-/var/lib/agentd/update}"
BIN="${APEXOS_SELF_UPDATE_BIN:-/usr/local/bin/agentd}"
PREV="${BIN}.prev"
SYSTEMCTL="${APEXOS_SELF_UPDATE_SYSTEMCTL:-systemctl}"
# A confirm older than this isn't "recent" — a crash long after an update is not
# that update's fault. Default 10 min (generous for a latent crash to surface).
PROBATION_WINDOW="${APEXOS_PROBATION_WINDOW:-600}"
CONFIRMED="$UPDATE_DIR/confirmed.json"

log()  { echo "[probation] $*"; }
now()  { date +%s; }
sctl() { "$SYSTEMCTL" "$@"; }

jget() { # $1=file $2=key  (jq if present; sed fallback for pretty JSON)
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg k "$2" '.[$k] // empty' "$1" 2>/dev/null
  else
    grep -m1 "\"$2\"" "$1" 2>/dev/null \
      | sed 's/^[^:]*:[[:space:]]*//; s/^"//; s/"\{0,1\},\{0,1\}[[:space:]]*$//'
  fi
}

# ── guards: only a RECENT confirmed self-update justifies a probation rollback ──
[ -f "$CONFIRMED" ] || { log "no confirmed.json — not a self-update regression; leaving agentd to systemd"; exit 0; }
[ -f "$PREV" ]      || { log "no agentd.prev to restore; leaving to systemd"; exit 0; }

TS=$(jget "$CONFIRMED" ts)
case "$TS" in ""|*[!0-9]*) TS=0 ;; esac
AGE=$(( $(now) - TS ))
if [ "$AGE" -gt "$PROBATION_WINDOW" ]; then
  log "last confirm was ${AGE}s ago (> ${PROBATION_WINDOW}s) — not a fresh-update crash; leaving to systemd"
  exit 0
fi
TARGET=$(jget "$CONFIRMED" target_commit)
PREVC=$(jget "$CONFIRMED" prev_commit)

# Anti-loop: CONSUME the confirm marker first — at most one probation rollback per
# confirmed update. If the restored binary itself crash-loops, the guard above now
# fails (no confirmed.json) and we exit cleanly instead of ping-ponging.
mv -f "$CONFIRMED" "$UPDATE_DIR/confirmed.superseded.json" 2>/dev/null

log "PROBATION ROLLBACK: agentd crash-looped ${AGE}s after confirming $TARGET — restoring agentd.prev ($PREVC)"
sctl stop agentd 2>/dev/null
if cp -f "$PREV" "$BIN.rollback.$$" && mv -f "$BIN.rollback.$$" "$BIN"; then
  sctl reset-failed agentd 2>/dev/null   # clear the StartLimit so it can start again
  sctl start agentd 2>/dev/null
  cat > "$UPDATE_DIR/rolled-back.json" <<EOF
{
  "outcome": "rolled-back",
  "reason": "probation crash-loop (latent crash ${AGE}s after a passing health probe)",
  "target_commit": "$TARGET",
  "prev_commit": "$PREVC",
  "ts": $(now)
}
EOF
  log "restored known-good agentd.prev"
else
  sctl reset-failed agentd 2>/dev/null
  sctl start agentd 2>/dev/null
  log "WARNING: restore of agentd.prev failed"
fi
exit 0
