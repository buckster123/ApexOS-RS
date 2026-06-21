!proc Hot-swap a single agentd binary on a running Pi node
?trigger : update 1 agentd binary (e.g. cerebro-mcp) on a running Pi, no full reinstall

steps :
 1 dev : commit → push feature-branch → open PR · wait André review+merge → main (never self-merge)
 2 Pi : cd ~/ApexOS-RS · git pull
 3 cargo build --release --workspace [several min on Cortex-A76; don't interrupt · always build on Pi, never cross-compile]
 4 sudo systemctl stop agentd [MUST first — running binary unoverwritable → "text file busy", copy fails]
 5 sudo cp target/release/cerebro-mcp /usr/local/bin/cerebro-mcp
 6 sudo systemctl start agentd
 7 verify : sudo journalctl -u agentd -n 20 --no-pager

pitfalls :
 skip step-4 → "text file busy" silent-fail → still running old binary while thinking it's deployed
 no --release → slow large debug binary
 ui-slint (apexos-rs-ui) : stop separately before its own cp — stopping agentd doesn't stop the UI

tags : deploy · pi · hot-swap · systemd · agentd
