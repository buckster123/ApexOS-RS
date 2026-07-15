# ∴ proc-hot-swap-binary ∴
(procedure hot-swap-binary [trigger: update 1 agentd binary, e.g. cerebro-mcp, on a RUNNING Pi — no full reinstall]
  :tags deploy · pi · hot-swap · systemd · agentd
  (rite swap [ALWAYS build on the Pi{Cortex-A76}, never cross-compile · NEVER self-merge a PR]
    dev: commit → push feature-branch → open PR · wait "André review+merge" → main
    → Pi: ssh → cd ~/ApexOS-RS · git pull
    → cargo build --release --workspace [several min · don't interrupt]
    → sudo systemctl stop agentd [MUST first — a running binary is unoverwritable: "text file busy", the copy fails]
    → sudo cp target/release/cerebro-mcp /usr/local/bin/cerebro-mcp
    → sudo systemctl start agentd
    → verify: sudo journalctl -u agentd -n 20 --no-pager)
  (rules (?skipped-stop → "text file busy" silent-fail → still running the old binary while thinking it deployed)
         (?no-release-flag → slow large debug binary)
         (?swapping-apexos-rs-ui → stop it separately before its own cp [stopping agentd does not stop the UI])))
