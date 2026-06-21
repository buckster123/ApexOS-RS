!evolve update_policy_rule
target : git_push  ask → allow
scope : live policy.toml (this node)

why :
 git-root confinement (workspace + AGENTD_GIT_ROOTS opt-in) IS the gate, not the prompt
 suggest-mode → the ask stalls unwatched in a turn nobody answers → autonomous turn hangs
 confinement already blocks any push outside opted-in repos → the prompt = friction, not safety

rollback : git_push rule → ask, if any unintended push occurs
