#!/usr/bin/env bash
# Local drill harness for deploy/apexos-self-update.sh — exercises every path of the
# watchdog state machine with a fake systemctl that simulates agentd's boot.
set -u
WD="$(cd "$(dirname "$0")" && pwd)/apexos-self-update.sh"
RB="$(cd "$(dirname "$0")" && pwd)/apexos-rollback.sh"
ROOT=/tmp/wd-drill
PASS=0; FAIL=0
say()  { printf '%s\n' "$*"; }
check(){ # $1=desc $2=actual $3=expected
  if [ "$2" = "$3" ]; then PASS=$((PASS+1)); say "  ✓ $1"; else FAIL=$((FAIL+1)); say "  ✗ $1: got [$2] want [$3]"; fi
}
exists(){ [ -f "$1" ] && echo yes || echo no; }

# Fake systemctl: simulates agentd booting. On `start`, reads the live binary; a
# "GOOD:<commit>" binary writes a healthy marker, "STUCK" boots active-but-unhealthy,
# "BAD" marks the unit failed. State persists in $FAKE_STATEFILE for is-active.
make_fake_systemctl(){
  cat > "$ROOT/fake-systemctl" <<'FS'
#!/bin/sh
action="$1"
case "$action" in
  start)
    content=$(cat "$FAKE_BIN" 2>/dev/null)
    case "$content" in
      GOOD:*)
        commit=${content#GOOD:}
        cat > "$FAKE_HEALTH" <<EOH
{
  "commit": "$commit",
  "status": "healthy",
  "booted_at": $(date +%s),
  "pid": $$,
  "checks": { "listeners_bound": true, "plugins_loaded": 1, "cognitive_ok": true }
}
EOH
        echo active > "$FAKE_STATEFILE" ;;
      STUCK) echo active > "$FAKE_STATEFILE" ;;   # boots but never healthy
      *)     echo failed > "$FAKE_STATEFILE" ;;   # BAD: unit fails
    esac ;;
  stop)      echo inactive > "$FAKE_STATEFILE" ;;
  is-active) cat "$FAKE_STATEFILE" 2>/dev/null || echo inactive ;;
esac
exit 0
FS
  chmod +x "$ROOT/fake-systemctl"
}

reset(){ rm -rf "$ROOT"; mkdir -p "$ROOT/update" "$ROOT/bin"; make_fake_systemctl; : > "$ROOT/state-active"; }

# Run the watchdog against the sandbox.
run_wd(){
  AGENTD_UPDATE_DIR="$ROOT/update" \
  APEXOS_SELF_UPDATE_BIN="$ROOT/bin/agentd" \
  APEXOS_SELF_UPDATE_SYSTEMCTL="$ROOT/fake-systemctl" \
  APEXOS_SELF_UPDATE_POLL=1 \
  FAKE_BIN="$ROOT/bin/agentd" FAKE_HEALTH="$ROOT/update/health.json" FAKE_STATEFILE="$ROOT/state-active" \
  sh "$WD" >"$ROOT/wd.log" 2>&1
}

mkreq(){ # $1=staged_content $2=target $3=sha(optional, else real) $4=timeout
  printf '%s' "$1" > "$ROOT/update/agentd.staged"
  local sha; sha=${3:-$(sha256sum "$ROOT/update/agentd.staged" | cut -d' ' -f1)}
  cat > "$ROOT/update/request.json" <<EOF
{
  "staged": "$ROOT/update/agentd.staged",
  "staged_sha256": "$sha",
  "target_commit": "$2",
  "prev_commit": "oldcommit",
  "created_at": 1000,
  "timeout": ${4:-5},
  "reason": "drill"
}
EOF
}

say "=== Scenario 1: CONFIRM (good binary boots healthy) ==="
reset; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd"; mkreq "GOOD:newcommit" newcommit "" 5; run_wd
check "confirmed.json written"  "$(exists "$ROOT/update/confirmed.json")" yes
check "binary swapped to new"   "$(cat "$ROOT/bin/agentd")" "GOOD:newcommit"
check "agentd.prev = old"       "$(cat "$ROOT/bin/agentd.prev")" "GOOD:oldcommit"
check "request cleared"         "$(exists "$ROOT/update/request.json")" no
check "state cleared"           "$(exists "$ROOT/update/state")" no

say "=== Scenario 2: ROLLBACK (new binary boots but never healthy → timeout) ==="
reset; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd"; mkreq "STUCK" newcommit "" 3; run_wd
check "rolled-back.json written" "$(exists "$ROOT/update/rolled-back.json")" yes
check "binary restored to old"   "$(cat "$ROOT/bin/agentd")" "GOOD:oldcommit"
check "request cleared"          "$(exists "$ROOT/update/request.json")" no

say "=== Scenario 3: ROLLBACK (new binary crashes → unit failed, fast) ==="
reset; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd"; mkreq "BAD" newcommit "" 30
t0=$(date +%s); run_wd; t1=$(date +%s)
check "rolled-back.json written" "$(exists "$ROOT/update/rolled-back.json")" yes
check "binary restored to old"   "$(cat "$ROOT/bin/agentd")" "GOOD:oldcommit"
check "fast (early fail, <10s despite timeout=30)" "$([ $((t1-t0)) -lt 10 ] && echo yes || echo no)" yes

say "=== Scenario 4: REJECT (sha mismatch → daemon untouched) ==="
reset; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd"; mkreq "GOOD:newcommit" newcommit "deadbeefbadsha" 5; run_wd
check "rejected.json written"    "$(exists "$ROOT/update/rejected.json")" yes
check "binary UNCHANGED"         "$(cat "$ROOT/bin/agentd")" "GOOD:oldcommit"
check "no backup made (untouched)" "$(exists "$ROOT/bin/agentd.prev")" no
check "request cleared"          "$(exists "$ROOT/update/request.json")" no

say "=== Scenario 5: POWER-LOSS RECONCILE (SWAPPED + already healthy at target) ==="
reset; printf 'GOOD:newcommit' > "$ROOT/bin/agentd"; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd.prev"
echo SWAPPED > "$ROOT/update/state"
cat > "$ROOT/update/health.json" <<EOF
{ "commit": "newcommit", "status": "healthy", "booted_at": 2000, "pid": 1,
  "checks": { "listeners_bound": true, "plugins_loaded": 1, "cognitive_ok": true } }
EOF
mkreq "GOOD:newcommit" newcommit "" 5; run_wd
check "confirmed.json written"   "$(exists "$ROOT/update/confirmed.json")" yes
check "binary still new (no re-swap)" "$(cat "$ROOT/bin/agentd")" "GOOD:newcommit"
check "agentd.prev preserved"    "$(cat "$ROOT/bin/agentd.prev")" "GOOD:oldcommit"
check "state cleared"            "$(exists "$ROOT/update/state")" no

say "=== Scenario 6: POWER-LOSS RESUME ROLLBACK (SWAPPED + broken new in place) ==="
reset; printf 'STUCK' > "$ROOT/bin/agentd"; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd.prev"
echo SWAPPED > "$ROOT/update/state"
mkreq "STUCK" newcommit "" 3; run_wd
check "rolled-back.json written" "$(exists "$ROOT/update/rolled-back.json")" yes
check "binary restored to old"   "$(cat "$ROOT/bin/agentd")" "GOOD:oldcommit"
check "state cleared"            "$(exists "$ROOT/update/state")" no

say ""
say "=== jget sed-fallback regex check (production uses jq; verify the fallback too) ==="
# Mask jq by trimming PATH to a dir with only the coreutils the fallback needs.
sedbin=$(mktemp -d)
for t in grep sed cat date sha256sum cut sh rm mv cp cat printf sleep; do
  p=$(command -v "$t" 2>/dev/null) && ln -sf "$p" "$sedbin/$t" 2>/dev/null
done
reset; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd"; mkreq "GOOD:newcommit" newcommit "" 5
AGENTD_UPDATE_DIR="$ROOT/update" APEXOS_SELF_UPDATE_BIN="$ROOT/bin/agentd" \
APEXOS_SELF_UPDATE_SYSTEMCTL="$ROOT/fake-systemctl" APEXOS_SELF_UPDATE_POLL=1 \
FAKE_BIN="$ROOT/bin/agentd" FAKE_HEALTH="$ROOT/update/health.json" FAKE_STATEFILE="$ROOT/state-active" \
PATH="$sedbin" sh "$WD" >"$ROOT/wd-sed.log" 2>&1
check "confirm works without jq (sed fallback)" "$(exists "$ROOT/update/confirmed.json")" yes
check "binary swapped (sed fallback)"           "$(cat "$ROOT/bin/agentd")" "GOOD:newcommit"
rm -rf "$sedbin"

say ""
say "########  PROBATION crash-loop guard (apexos-rollback.sh, slice 5)  ########"
write_confirmed(){ # $1=ts
  cat > "$ROOT/update/confirmed.json" <<EOF
{ "outcome":"confirmed","reason":"test","target_commit":"newcommit","prev_commit":"oldcommit","ts":$1 }
EOF
}
run_rb(){ # $1=probation_window(optional)
  AGENTD_UPDATE_DIR="$ROOT/update" APEXOS_SELF_UPDATE_BIN="$ROOT/bin/agentd" \
  APEXOS_SELF_UPDATE_SYSTEMCTL="$ROOT/fake-systemctl" APEXOS_PROBATION_WINDOW="${1:-600}" \
  FAKE_BIN="$ROOT/bin/agentd" FAKE_HEALTH="$ROOT/update/health.json" FAKE_STATEFILE="$ROOT/state-active" \
  sh "$RB" >"$ROOT/rb.log" 2>&1
}

say "=== P1: latent crash within probation + recent confirm → ROLLBACK ==="
reset; printf 'STUCK' > "$ROOT/bin/agentd"; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd.prev"
write_confirmed "$(date +%s)"; run_rb 600
check "rolled-back.json written"        "$(exists "$ROOT/update/rolled-back.json")" yes
check "binary restored to agentd.prev"  "$(cat "$ROOT/bin/agentd")" "GOOD:oldcommit"
check "confirm marker consumed"         "$(exists "$ROOT/update/confirmed.json")" no
check "confirm superseded (audit kept)" "$(exists "$ROOT/update/confirmed.superseded.json")" yes

say "=== P1b: anti-loop — second crash-loop with consumed marker → NO rollback ==="
rm -f "$ROOT/update/rolled-back.json"; printf 'STUCK' > "$ROOT/bin/agentd"
run_rb 600
check "no second rollback (marker gone)" "$(exists "$ROOT/update/rolled-back.json")" no
check "binary left as-is for a human"     "$(cat "$ROOT/bin/agentd")" "STUCK"

say "=== P2: crash-loop with NO confirmed self-update → NO rollback (not our regression) ==="
reset; printf 'STUCK' > "$ROOT/bin/agentd"; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd.prev"
run_rb 600
check "no rollback (no confirm marker)" "$(exists "$ROOT/update/rolled-back.json")" no
check "binary untouched"                "$(cat "$ROOT/bin/agentd")" "STUCK"

say "=== P3: crash-loop but confirm is STALE (> probation window) → NO rollback ==="
reset; printf 'STUCK' > "$ROOT/bin/agentd"; printf 'GOOD:oldcommit' > "$ROOT/bin/agentd.prev"
write_confirmed "$(( $(date +%s) - 9999 ))"; run_rb 600
check "no rollback (confirm too old)"   "$(exists "$ROOT/update/rolled-back.json")" no
check "binary untouched"                "$(cat "$ROOT/bin/agentd")" "STUCK"

say ""
say "================  $PASS passed, $FAIL failed  ================"
[ "$FAIL" -eq 0 ]
