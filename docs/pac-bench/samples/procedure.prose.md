Title: Hot-swap a single agentd binary on a running Pi node

Trigger: When you need to update one agentd binary (for example cerebro-mcp) on a
running Pi node without performing a full reinstall of the whole stack.

Steps:
1. On the dev machine, commit and push your change to a feature branch, then open a
   pull request. Wait for André to review it and merge it into main — never merge it
   yourself.
2. SSH into the Pi and pull the latest main: change into the repo directory with
   `cd ~/ApexOS-RS` and run `git pull`.
3. Build the whole workspace in release mode with `cargo build --release --workspace`.
   This takes several minutes on the Pi because it is a Cortex-A76; do not interrupt it,
   and always build on the Pi rather than cross-compiling.
4. Stop the agentd service before copying the binary. A running binary cannot be
   overwritten on Linux — you will get a "text file busy" error and the copy fails.
   Run `sudo systemctl stop agentd`.
5. Copy the freshly built binary into place over the old one:
   `sudo cp target/release/cerebro-mcp /usr/local/bin/cerebro-mcp`.
6. Start the service again with `sudo systemctl start agentd`.
7. Verify it came back up cleanly by tailing the last twenty log lines:
   `sudo journalctl -u agentd -n 20 --no-pager`.

Pitfalls:
- Forgetting to stop the service first gives "text file busy" and the copy silently
  fails, so you end up running the old binary while thinking you deployed the new one.
- Building without the `--release` flag produces a slow, large debug binary.
- The UI binary (apexos-rs-ui) must also be stopped before copying it, as a separate
  step — stopping agentd does not stop the UI.

Tags: deploy, pi, hot-swap, systemd, agentd
