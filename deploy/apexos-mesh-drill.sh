#!/usr/bin/env bash
# ApexNET chaos drill — does this node degrade honestly when its lanes die?
#
# Charter: docs/apexnet.md §9 (P5). The claim under test is not "the mesh
# works" but something stricter: when connectivity drops, the node must SAY so,
# and must say so within a bounded time rather than pretending until something
# fails.
#
# This script OBSERVES and VERIFIES; it does not cut your network. Killing a
# link needs root, and a drill that quietly reconfigures the machine it is
# auditing is a drill nobody runs twice. It tells you what to do and checks
# what happened.
#
#   ./apexos-mesh-drill.sh                    # against localhost
#   ./apexos-mesh-drill.sh 192.168.0.158      # against a colony node
#
# Exit 0 = the node reported every transition it should have.
set -uo pipefail

NODE="${1:-127.0.0.1}"
PORT="${APEXOS_PORT:-8787}"
BASE="http://${NODE}:${PORT}"
# The latch is deliberately slow (D7: a flapping filter would rebuild the
# prompt-cache prefix on every probe), so give it room before calling a miss.
DEADLINE_S="${DRILL_DEADLINE_S:-240}"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }

state() { curl -fsS --max-time 5 "${BASE}/api/connectivity" 2>/dev/null | sed -n 's/.*"state":"\([a-z]*\)".*/\1/p'; }

# Wait for the node to report one of the given states. Returns the state it
# settled on, or empty on timeout.
await_state() {
  local want="$1" start now s
  start=$(date +%s)
  while :; do
    s="$(state)"
    if [[ -n "$s" ]] && grep -qw "$s" <<<"$want"; then printf '%s' "$s"; return 0; fi
    now=$(date +%s)
    (( now - start >= DEADLINE_S )) && return 1
    sleep 5
  done
}

say "ApexNET chaos drill — ${BASE}"

if ! curl -fsS --max-time 5 "${BASE}/api/ping" >/dev/null 2>&1; then
  bad "no answer from ${BASE}/api/ping — is agentd running and reachable?"
  exit 2
fi
BASELINE="$(state)"
if [[ -z "$BASELINE" ]]; then
  bad "/api/connectivity returned nothing — this agentd predates ApexNET P5b"
  exit 2
fi
ok "node is up, reporting: ${BASELINE}"

if [[ "$BASELINE" != "full" ]]; then
  info "expected 'full' to start from; drilling from '${BASELINE}' instead."
fi

say "1/3 — cut the WAN, and watch the node admit it"
cat <<'EOS'
  Run ONE of these on the node (they need root, so they are yours to run):

    # cleanest: drop the default route
    sudo ip route del default

    # or, if the node is on Wi-Fi
    sudo nmcli radio wifi off

  Leave the LAN up if you want to see 'degraded'; cut everything for 'isolated'.
EOS
read -r -p "  press enter once the link is down… " _ || true

info "waiting up to ${DEADLINE_S}s for the latch to settle…"
if GOT="$(await_state "degraded minimal isolated")"; then
  ok "node reports '${GOT}' — it noticed, and said so"
else
  bad "still reporting '$(state)' after ${DEADLINE_S}s"
  bad "a node that cannot tell it is cut off will keep offering tools that cannot work"
  exit 1
fi

say "2/3 — confirm the degradation is visible where it matters"
info "the agent's tool list should have lost its WAN-dependent tools,"
info "and its ambient line should carry the connectivity notice."
info "check on the board, or: journalctl -u agentd -n 40 --no-pager"

say "3/3 — restore, and watch it come back"
cat <<'EOS'
  Undo whatever you did:

    sudo ip route add default via <your-gateway>     # or: sudo dhclient -r && sudo dhclient
    sudo nmcli radio wifi on
EOS
read -r -p "  press enter once the link is back… " _ || true

info "waiting up to ${DEADLINE_S}s for recovery…"
if GOT="$(await_state "full")"; then
  ok "node reports '${GOT}' — recovered"
else
  bad "stuck at '$(state)' after ${DEADLINE_S}s"
  bad "a latch that drops but never lifts is worse than no latch"
  exit 1
fi

say "drill passed"
info "the node degraded honestly and recovered on its own."
cat <<'EOS'

  Not yet covered, because the lanes do not exist yet (docs/apexnet.md §6.1):
    · a2a continuing over BLE while Tier 1 is down
    · a heavy artifact landing in the outbox and draining on recovery
  Those arms light up when real transports are registered with the router.
EOS
