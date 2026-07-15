//! PAC-2 Dense structural lint — the agentd-side "tiny Rust check" of
//! The-PAC spec §9 (reference implementation: `docs/pac-bench/pac2lint.py`).
//!
//! The L7 safety rail for self-evolution: a dense-formatted soul rewrite is
//! validated STRUCTURALLY before it ships — a broken artifact is caught at
//! `propose_evolution` apply time instead of discovered in behavior three days
//! later. **Format-gated**: only payloads that look dense (the `∴` seal or an
//! artifact-head opening form) are linted — prose and lean (`§`-block) souls
//! pass untouched, so there is no compliance tax for agents to route around
//! (colony red line 6). Errors refuse the apply (the honest failure mode for
//! an identity file that would reload broken — the H4-gate precedent);
//! warnings ride the deferred ack.
//!
//! Deliberately smaller than the Python reference: the pure text rules only
//! (parse/depth · head registry · glyph law · register strip rules · the L8
//! cache probe · emanation bounds · emphasis-CAPS placement). The `!ops`-vs-
//! embodiment and invariant-grounding checks need registry/config knowledge
//! and stay in the Python linter + the destination re-lint for now.

/// One lint finding. `error: true` refuses a dense soul rewrite; warnings are
/// appended to the apply ack.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub line:  usize, // 1-based; 0 = whole-artifact
    pub error: bool,
    pub msg:   String,
}

impl Finding {
    fn err(line: usize, msg: impl Into<String>) -> Self {
        Self { line, error: true, msg: msg.into() }
    }
    fn warn(line: usize, msg: impl Into<String>) -> Self {
        Self { line, error: false, msg: msg.into() }
    }
}

/// Render findings into the one-line-per-finding report carried by the
/// refusal / ack.
pub fn render_report(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| {
            let sev = if f.error { "error" } else { "warn" };
            if f.line > 0 {
                format!("line {}: {sev}: {}", f.line, f.msg)
            } else {
                format!("{sev}: {}", f.msg)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does this content claim to be a PAC-2 Dense artifact? True when the first
/// non-empty line is a `∴` seal header or opens with an artifact-head form.
/// Everything else (prose souls, lean `§`-block souls) is NOT dense and is
/// never linted.
pub fn is_dense_artifact(content: &str) -> bool {
    let first = match content.lines().find(|l| !l.trim().is_empty()) {
        Some(l) => l.trim(),
        None => return false,
    };
    if first.starts_with('#') && first.contains('∴') {
        return true;
    }
    ARTIFACT_HEADS
        .iter()
        .any(|h| first.starts_with(&format!("({h} ")) || first == &format!("({h}"))
}

const ARTIFACT_HEADS: [&str; 5] = ["soul", "procedure", "evolution", "skill", "engine"];
const BLOCK_HEADS: [&str; 6] = ["voice", "invariants", "register", "def", "rite", "rules"];
const FACTORY_HEADS: [&str; 4] = ["port", "engine", "module", "bootstrap"];

/// §6 alchemical lexicon — v0.1 plus the colony-ratified v0.2 verbs
/// (apex1·apex2·apex-4 veto-window deliberation, 2026-07-15): `graft` = a
/// dream-digest coinage crossing the mesh and attempting to root (a failed
/// graft is still a graft); `temper` = a soft invariant earning its plumbing,
/// the `~` drops — "can now be trusted to hold under conditions it hasn't yet
/// faced".
const ALCHEMICAL: [&str; 16] = [
    "solve", "distill", "transmute", "anneal", "quarantine", "calcine", "coagula",
    "amalgama", "athanor", "alembic", "nigredo", "albedo", "rubedo", "emanation",
    "graft", "temper",
];

const EMPHASIS: [&str; 6] = ["MUST", "NEVER", "ALWAYS", "MANDATORY", "REQUIRED", "FORBIDDEN"];

fn head_known(h: &str) -> bool {
    ARTIFACT_HEADS.contains(&h) || BLOCK_HEADS.contains(&h) || FACTORY_HEADS.contains(&h)
}

/// Banned everywhere, prose islands included (L3 "everywhere, always"):
/// styled math alphanumerics, emoji blocks, dingbats, variation selectors, ZWJ.
fn banned_codepoint(c: char) -> bool {
    matches!(c as u32,
        0x1D400..=0x1D7FF | 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE00..=0xFE0F | 0x200D)
}

/// Non-ASCII allowed in STRUCTURAL positions: the §3 registry glyphs plus the
/// measured identifier extension (Greek letters β=1/1, en-dash –=1/1 — same
/// two-family ≤2-token law as the registry).
fn structural_glyph_ok(c: char) -> bool {
    c.is_ascii()
        || "→·↔≡∴↦∮§–".contains(c)
        || ('\u{0370}'..='\u{03FF}').contains(&c)
        || c.is_whitespace()
}

#[derive(PartialEq)]
enum Island {
    None,
    Quote,
    Constraint,
    Comment,
}

/// Lint a dense artifact. Pure; call only after [`is_dense_artifact`].
pub fn lint(content: &str) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    // ── single-pass scan: islands, form stack, heads, structural text ──────
    let mut island = Island::None;
    let mut island_open = 0usize;
    // true = headed form (counts toward depth) · false = attached arg-group
    let mut stack: Vec<bool> = Vec::new();
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut pending_head: Option<usize> = None; // depth collecting a head
    let mut head_buf = String::new();
    let mut structural = String::new();
    let mut last_top_close = 0usize;
    let mut seal_lines = 0usize;

    for (ln0, raw) in lines.iter().enumerate() {
        let ln = ln0 + 1;
        // Rule 3a: banned codepoints anywhere, islands included.
        for c in raw.chars() {
            if banned_codepoint(c) {
                out.push(Finding::err(ln, format!(
                    "banned codepoint U+{:04X} ({c:?}) — styled/emoji glyphs, L3", c as u32)));
            }
        }
        // Seal lines are their own layer.
        if depth == 0 && island == Island::None && raw.trim_start().starts_with('#') {
            if raw.contains('∴') {
                seal_lines += 1;
                if seal_lines > 1 {
                    out.push(Finding::err(ln, "more than one ∴ seal"));
                }
            }
            continue;
        }
        let mut prev: Option<char> = None;
        for c in raw.chars() {
            match island {
                Island::Comment => { /* to EOL */ }
                Island::Quote => {
                    if c == '"' { island = Island::None; }
                }
                Island::Constraint => {
                    if c == ']' { island = Island::None; }
                }
                Island::None => match c {
                    ';' => island = Island::Comment,
                    '"' => { island = Island::Quote; island_open = ln; }
                    '[' => { island = Island::Constraint; island_open = ln; }
                    '(' => {
                        // Attached to a symbol tail = arg group (part of the op
                        // atom, no depth); whitespace-preceded = headed form.
                        let attached = prev.map_or(false, |p| p.is_alphanumeric() || p == '_' || p == '-');
                        stack.push(!attached);
                        if !attached {
                            depth += 1;
                            max_depth = max_depth.max(depth);
                            if depth <= 2 {
                                pending_head = Some(depth);
                                head_buf.clear();
                            }
                        }
                        structural.push(c);
                    }
                    ')' => {
                        if pending_head == Some(depth) {
                            check_head(&head_buf, ln, &mut out);
                            pending_head = None;
                        }
                        match stack.pop() {
                            None => out.push(Finding::err(ln, "unbalanced ')'")),
                            Some(true) => {
                                depth = depth.saturating_sub(1);
                                if depth == 0 { last_top_close = ln; }
                            }
                            Some(false) => {}
                        }
                        structural.push(c);
                    }
                    _ => {
                        if pending_head == Some(depth) {
                            if c.is_whitespace() {
                                if !head_buf.is_empty() {
                                    check_head(&head_buf, ln, &mut out);
                                    pending_head = None;
                                }
                            } else {
                                head_buf.push(c);
                            }
                        }
                        if !structural_glyph_ok(c) {
                            out.push(Finding::err(ln, format!(
                                "structural glyph {c:?} not in the §3 registry")));
                        }
                        if c == '∴' {
                            out.push(Finding::err(ln, "∴ outside the seal header (header only, one per artifact)"));
                        }
                        structural.push(c);
                    }
                },
            }
            prev = Some(c);
        }
        if island == Island::Comment {
            island = Island::None; // comments end at EOL
        }
        structural.push('\n');
    }

    if !stack.is_empty() {
        out.push(Finding::err(lines.len(), format!("{} unclosed '('", stack.len())));
    }
    match island {
        Island::Quote => out.push(Finding::err(island_open, "unclosed quote")),
        Island::Constraint => out.push(Finding::err(island_open, "unclosed constraint '['")),
        _ => {}
    }
    if max_depth > 3 {
        out.push(Finding::err(0, format!("form depth {max_depth} exceeds 3 (L6)")));
    }

    // ── emanation: non-empty lines after the final top-level close ─────────
    let emanation: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .skip(last_top_close)
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();

    // ── register (§6) + rule 7/10 ───────────────────────────────────────────
    let register = structural
        .split("(register")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(|s| s.trim().to_string());
    let register_on = match register.as_deref() {
        Some("alchemical") => true,
        Some("none") | None => false,
        Some(other) => {
            out.push(Finding::err(0, format!(
                "unknown register '{other}' (known: alchemical, none)")));
            false
        }
    };
    let verbs_used: Vec<&str> = ALCHEMICAL
        .iter()
        .copied()
        .filter(|v| {
            structural
                .split(|c: char| !(c.is_alphanumeric() || c == '-'))
                .any(|w| w == *v)
        })
        .collect();
    if !register_on && !verbs_used.is_empty() {
        out.push(Finding::err(0, format!(
            "register verbs used with no register declared: {} (R1)", verbs_used.join(", "))));
    }
    if !emanation.is_empty() {
        if !register_on {
            out.push(Finding::err(emanation[0].0, "emanation present with register off (R2/R3)"));
        }
        if emanation.len() > 3 {
            out.push(Finding::err(emanation[3].0, format!(
                "emanation is {} lines (max 3)", emanation.len())));
        }
        for (ln, t) in &emanation {
            if t.contains('!') || t.contains('(') {
                out.push(Finding::err(*ln, "emanation must be prose only — no ops, no forms (R3)"));
            }
        }
    }

    // ── L8 cache probe (rule 9): dates + clocks anywhere in the artifact ───
    for (ln0, raw) in lines.iter().enumerate() {
        let ln = ln0 + 1;
        if has_date(raw) {
            out.push(Finding::err(ln,
                "date string inside the artifact (L8 — live state rides injection, not the soul)"));
        }
        if has_clock_outside_cadence(raw) {
            out.push(Finding::err(ln,
                "clock string inside the artifact (L8; ∮-cadences are exempt)"));
        }
    }

    // ── rule 8 (warn): emphasis caps outside constraints ────────────────────
    for w in EMPHASIS {
        if structural
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '\''))
            .any(|t| t == w)
        {
            out.push(Finding::warn(0, format!(
                "hard-rule marker '{w}' outside a [ … ] constraint — put it in the constraint of the form it governs")));
        }
    }

    out
}

fn check_head(head: &str, ln: usize, out: &mut Vec<Finding>) {
    if head.is_empty() || head.starts_with('!') || head.starts_with('?') || head.starts_with('~') {
        return; // op arg group / trigger / soft form
    }
    if !head_known(head) {
        out.push(Finding::err(ln, format!(
            "unknown form head '{head}' (extend the §2 registry first)")));
    }
}

/// `\d{4}-\d{2}-\d{2}` without pulling in a regex dependency.
fn has_date(s: &str) -> bool {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(9)).any(|i| {
        b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 5].is_ascii_digit() && b[i + 6].is_ascii_digit()
            && b[i + 7] == b'-'
            && b[i + 8].is_ascii_digit() && b[i + 9].is_ascii_digit()
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && (i + 10 >= b.len() || !b[i + 10].is_ascii_digit())
    })
}

/// `H:MM` / `HH:MM(:SS)?` clock strings, except inside a `∮`-prefixed cadence
/// token (a static schedule declaration, not live state).
fn has_clock_outside_cadence(s: &str) -> bool {
    s.split_whitespace()
        .filter(|tok| !tok.starts_with('∮'))
        .any(|tok| {
            let t: Vec<char> = tok.chars().collect();
            (0..t.len()).any(|i| {
                // digit(s) ':' digit digit, not part of a longer digit run
                t[i].is_ascii_digit()
                    && (i == 0 || !t[i - 1].is_ascii_digit())
                    && {
                        let j = if i + 1 < t.len() && t[i + 1].is_ascii_digit() { i + 2 } else { i + 1 };
                        j + 2 < t.len() + 1
                            && t.get(j) == Some(&':')
                            && t.get(j + 1).map_or(false, |c| c.is_ascii_digit())
                            && t.get(j + 2).map_or(false, |c| c.is_ascii_digit())
                            && t.get(j + 3).map_or(true, |c| !c.is_ascii_digit())
                    }
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &str = r#"# ∴ APEX-mini ∴
(soul APEX-mini
  (voice "You are APEX — steady heat, no theatre."
         "Less performative, more being.")
  (invariants (salience .8–.95) ~(anomaly z>2.5))
  (register alchemical)
  (rite startup [each session · skip iff context clear · salience band holds]
    !cognitive_bootstrap(:query task :mode standard) → !session_recall)
  (rite dream ∮03:00UTC [autonomous — calcine · anomaly watch]
    !dream_run → darwin)
  (rules (?idle → rite startup)
         (?unfamiliar-task → !find_relevant_procedures(:limit 3))))
*From steady heat the athanor holds its shape.*
"#;

    fn errors(s: &str) -> Vec<Finding> {
        lint(s).into_iter().filter(|f| f.error).collect()
    }

    #[test]
    fn clean_artifact_passes() {
        let e = errors(CLEAN);
        assert!(e.is_empty(), "unexpected errors: {}", render_report(&e));
    }

    #[test]
    fn dense_detection() {
        assert!(is_dense_artifact(CLEAN));
        assert!(is_dense_artifact("(soul X (voice \"v\"))"));
        // Prose and lean souls are NOT dense — never linted.
        assert!(!is_dense_artifact("# APEX\n\nYou are APEX — an agent."));
        assert!(!is_dense_artifact("# APEX\n§startup : !boot → !save"));
        assert!(!is_dense_artifact(""));
    }

    #[test]
    fn structural_breakage_is_an_error() {
        assert!(!errors("(soul X (voice \"hi\")").is_empty(), "unclosed form");
        assert!(!errors("(soul X (voice \"hi))").is_empty(), "unclosed quote");
        assert!(!errors("(soul X (rite go [x hi))").is_empty(), "unclosed constraint");
        assert!(!errors("(soul X (rite go [x] (a (b c))))").is_empty(), "depth 4");
        assert!(!errors("(banquet X (voice \"hi\"))").is_empty(), "unknown head");
    }

    #[test]
    fn arg_groups_do_not_count_toward_depth() {
        // rules(2) → clause(3) → attached arg-group: raw paren level 4, form
        // depth 3 — the spec's own §11 shape must pass.
        let s = "(soul X (rules (?t → !find_relevant_procedures(:limit 3))))";
        assert!(errors(s).is_empty(), "{}", render_report(&lint(s)));
    }

    #[test]
    fn glyph_law() {
        assert!(!errors("(soul X (voice \"𝔸lchemy\"))").is_empty(), "blackletter banned everywhere");
        assert!(!errors("(soul X (voice \"hot 🔥\"))").is_empty(), "emoji banned everywhere");
        assert!(!errors("(soul X (rite go [x] a ⇒ b))").is_empty(), "⇒ not in registry");
        // β and – are the measured identifier extension; é hides in quotes.
        assert!(errors("(soul X (invariants ~(mercy β.04)) (rite go [x] mercy · .8–.95 · \"André\"))").is_empty());
    }

    #[test]
    fn seal_discipline() {
        assert!(!errors("(soul X (rite go [x] a ∴ b))").is_empty(), "∴ outside seal");
        assert!(!errors("# ∴ A ∴\n# ∴ B ∴\n(soul X (voice \"v\"))").is_empty(), "two seals");
    }

    #[test]
    fn register_strip_rules() {
        // Register verbs with register none/absent → error.
        assert!(!errors("(soul X (register none) (rite go [x] calcine → !save))").is_empty());
        // Emanation with register off → error.
        assert!(!errors("(soul X (register none) (voice \"v\"))\n*coda*").is_empty());
        // Unknown register → error.
        assert!(!errors("(soul X (register gothic) (voice \"v\"))").is_empty());
        // Emanation over 3 lines → error even with register on.
        let e = errors("(soul X (register alchemical) (voice \"v\"))\n*a*\n*b*\n*c*\n*d*");
        assert!(!e.is_empty());
    }

    #[test]
    fn cache_probe() {
        assert!(!errors("(soul X (voice \"born 2026-07-15\"))").is_empty(), "date");
        assert!(!errors("(soul X (rite go [x] !save at 03:00))").is_empty(), "clock");
        // ∮-cadence is a static declaration, exempt.
        assert!(errors("(soul X (rite dream ∮03:00UTC [ok] !dream_run))").is_empty());
    }

    #[test]
    fn emphasis_caps_is_a_warning_not_an_error() {
        let f = lint("(soul X (rite go [x] NEVER skip → !save))");
        assert!(f.iter().any(|x| !x.error && x.msg.contains("NEVER")));
        assert!(f.iter().filter(|x| x.error).count() == 0);
        // Acronyms are spelling, not emphasis.
        let f2 = lint("(soul X (rite go [x] GPIO · LUFS · JSONL → !save))");
        assert!(f2.iter().all(|x| x.error == false && !x.msg.contains("GPIO")) || f2.is_empty());
    }

    #[test]
    fn report_renders_lines() {
        let r = render_report(&[Finding::err(3, "boom"), Finding::warn(0, "meh")]);
        assert!(r.contains("line 3: error: boom"));
        assert!(r.contains("warn: meh"));
    }

    /// Forensic hook (mirrors the bench's `APEXOS_REPAIR_CHECK_FILE` idiom):
    /// point `APEXOS_PAC_LINT_FILE` at a dense artifact to run it through the
    /// exact gate a `propose_evolution` soul rewrite would hit. Skips silently
    /// when unset.
    ///
    ///   APEXOS_PAC_LINT_FILE=docs/pac-bench/samples/soul.dense.md \
    ///     cargo test -p agentd lint_check_file -- --nocapture
    #[test]
    fn lint_check_file() {
        let Ok(path) = std::env::var("APEXOS_PAC_LINT_FILE") else { return };
        let text = std::fs::read_to_string(&path).expect("readable artifact");
        assert!(is_dense_artifact(&text), "{path}: not detected as dense");
        let findings = lint(&text);
        let errs: Vec<Finding> = findings.iter().filter(|f| f.error).cloned().collect();
        println!("{path}: {} findings ({} errors)\n{}", findings.len(), errs.len(),
                 render_report(&findings));
        assert!(errs.is_empty(), "the gate would refuse this artifact");
    }
}
