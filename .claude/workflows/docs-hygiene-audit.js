export const meta = {
  name: 'docs-hygiene-audit',
  description: 'Read-only staleness audit of docs/, BACKLOG.md, PATTERNS.md vs shipped code',
  whenToUse: 'Periodic doc-hygiene sweep (every few weeks / after a big arc). Findings only — review them, then run an apply wave on a branch and ship via PR.',
  phases: [
    { title: 'Audit', detail: 'one agent per doc cluster, findings only — no edits' },
  ],
}

// Reusable sweep: agents derive "what shipped recently" from git themselves, so this
// script never goes stale. Pass { since: 'YYYY-MM-DD' } as args to widen/narrow the
// lookback (default ~6 weeks is a good cadence).
const SINCE = (args && args.since) || '6 weeks ago'

const PREAMBLE = `You are auditing developer docs in /home/andre/Projects/ApexOS-RS (a pure-Rust distro: agentd daemon + cerebro memory + apexos-tools + Slint ui-slint) for staleness.
First, orient: run \`git log --oneline --since='${SINCE}' origin/main | head -60\` to see what shipped recently — that is your drift horizon.
STRICTLY READ-ONLY: do NOT edit, create, or touch any file. Audit only.
Method per assigned file: (1) read it fully; (2) verify its factual claims against the actual code (grep/Read) and recent history (git log --oneline --since='${SINCE}' -- <relevant paths>); (3) find: [a] claims that are now false or stale, [b] shipped work the doc should cover but doesn't, [c] status markers (planned/pending/deferred/TODO) for things that actually shipped, [d] dead file-path or doc cross-references. Cite doc line AND code/git evidence for each. Factual drift only — no style nits. If a file is fully current, say so (status "current", empty findings).
NOTE: docs/gotchas.md is the invariant ledger and CLAUDE.md is the lean core — on any conflict between an SDK/guide doc and those two, the guide is the stale side.`

const FINDINGS_SCHEMA = {
  type: 'object',
  required: ['file_findings'],
  properties: {
    file_findings: {
      type: 'array',
      items: {
        type: 'object',
        required: ['file', 'status', 'findings'],
        properties: {
          file: { type: 'string', description: 'repo-relative path' },
          status: { enum: ['current', 'minor-drift', 'stale', 'obsolete'] },
          findings: {
            type: 'array',
            items: {
              type: 'object',
              required: ['claim', 'reality', 'fix', 'severity'],
              properties: {
                claim: { type: 'string', description: 'what the doc says (with line ref)' },
                reality: { type: 'string', description: 'what code/git actually shows (with evidence ref)' },
                fix: { type: 'string', description: 'the one-line correction to make' },
                severity: { enum: ['low', 'medium', 'high'] },
              },
            },
          },
        },
      },
    },
  },
}

// Clusters are stable doc groupings; the final catch-all agent lists docs/ itself so
// files created after this script was written still get covered.
const CLUSTERS = [
  { key: 'core-ledgers', files: 'docs/gotchas.md TOPIC HEADS ONLY (spot-check 8-10 entries whose subsystems changed recently per git log — full re-verification is not expected), docs/env-vars.md, and docs/agentd-protocol.md' },
  { key: 'core-maps', files: 'docs/architecture.md and docs/repo-map.md — regrep every line count and anchor' },
  { key: 'ui-docs', files: 'docs/ui-glowup.md, docs/build-roadmap.md, docs/slint-notes.md, and docs/adaptive-ui.md' },
  { key: 'colony', files: 'docs/colony-mesh.md and docs/colony-federation.md' },
  { key: 'identity-welfare', files: 'docs/agent-identity.md and docs/model-welfare.md' },
  { key: 'voice-web', files: 'docs/voice.md and docs/web-ui.md' },
  { key: 'occipital-usb', files: 'docs/occipital.md and docs/usb-workspace.md' },
  { key: 'selfupdate-postmk1', files: 'docs/self-update.md, docs/post-mk1.md, and docs/porting-guide.md' },
  { key: 'evo-edk', files: 'docs/edk.md, docs/app-parity.md, docs/symbiosis.md, and docs/evolutionary-layer.md' },
  { key: 'pac-caching', files: 'docs/pac.md and docs/prompt-caching.md' },
  { key: 'sdk', files: 'every file under docs/sdk/ (ls it first) — the outsider-facing extension guides; tool/event counts and file anchors drift fastest here' },
  { key: 'backlog', files: 'BACKLOG.md — cross-check every open item against merged PRs (`gh pr list --state merged --limit 100 --json number,title,mergedAt`); flag shipped items with their PR#' },
  { key: 'patterns', files: 'PATTERNS.md — verify every "where it lives" pointer via grep; flag shipped-since patterns missing from the manifest' },
  { key: 'catch-all', files: 'ls docs/ and docs/ideas/ — audit any .md file NOT covered by the other clusters (skip docs/slint-reference, docs/colony, docs/pac-bench, docs/rust-ai-3d-hud-skill: vendored/archive by design). Other clusters cover: gotchas, env-vars, agentd-protocol, architecture, repo-map, ui-glowup, build-roadmap, slint-notes, adaptive-ui, colony-mesh, colony-federation, agent-identity, model-welfare, voice, web-ui, occipital, usb-workspace, self-update, post-mk1, porting-guide, edk, app-parity, symbiosis, evolutionary-layer, pac, prompt-caching, sdk/' },
]

phase('Audit')
log(`fanning out ${CLUSTERS.length} read-only audit agents (drift horizon: ${SINCE})`)
const results = await parallel(CLUSTERS.map(c => () =>
  agent(`${PREAMBLE}\n\nYour assigned files: ${c.files}`,
    { label: `audit:${c.key}`, phase: 'Audit', schema: FINDINGS_SCHEMA })
))

const flat = results.filter(Boolean).flatMap(r => r.file_findings)
const counts = { current: 0, 'minor-drift': 0, stale: 0, obsolete: 0 }
for (const f of flat) counts[f.status] = (counts[f.status] || 0) + 1
log(`audited ${flat.length} files: ${JSON.stringify(counts)}`)
return { files: flat }
