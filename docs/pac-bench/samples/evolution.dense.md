# ∴ evo-git-push-allow ∴
(evolution git-push-allow [kind: update_policy_rule · scope: live policy.toml, THIS node]
  git_push: ask → allow
  why: git-root confinement{workspace + AGENTD_GIT_ROOTS opt-in} IS the gate, not the prompt
  · ask now = every push waits on interactive operator approval · suggest-mode: the ask stalls unwatched in a turn nobody answers → the autonomous turn hangs indefinitely
  · the self-update loop is trusted now · confinement already blocks any push outside opted-in repos → the prompt = friction, not safety
  (rules (?unintended-push → rollback: git_push rule → ask)))
