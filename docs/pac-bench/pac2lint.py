#!/usr/bin/env python3
"""pac2lint — reference linter for PAC-2 Dense artifacts (The-PAC spec §9).

The safety rail for self-evolution (spec L7): souls, procedures, and
`propose_evolution` payloads authored in PAC-2 Dense are validated STRUCTURALLY
before they ship — caught by this checker instead of discovered in behavior
three days later. Dense artifacts only; the lean dialect (`§`-block files, no
S-expression forms) is not this linter's input.

Check order (spec §9; E = error, W = warn):

  1  E  parse: balanced forms, quoted prose closed, constraints closed, depth ≤ 3
  2  E  form heads ∈ the §2 registry (checked at artifact + block level; depth-3
        paren groups are grammar items — op arg-groups, invariant pairs, rule
        clauses — and carry no registry head by design)
  3  E  non-ASCII in structural positions ∈ the §3 glyph registry (plus the
        measured identifier extension below); styled alphanumerics
        (U+1D400–1D7FF) and emoji are banned EVERYWHERE, including prose islands
  4  E/W  `!ops` ∈ the node's tool list (--ops-file; skipped with a note when no
        list is supplied; --portable downgrades to warn per spec)
  5  E  unmarked invariants must ground (--groundings, one name per line;
        defaults to the §4 hard set); `~` marks the rest
  6  W  every declared invariant referenced ≥ 1 time
  7  E  register verbs ∈ the declared register's lexicon;
        `(register none)` ⇒ zero register verbs and zero emanation
  8  W  emphasis CAPS (MUST/NEVER/ALWAYS/MANDATORY/REQUIRED/FORBIDDEN) only
        inside `[ … ]` constraints — acronyms (GPIO, LUFS, …) are spelling, not
        emphasis, and pass free
  9  E  cache probe: no dates or clock strings inside the artifact
        (∮-prefixed cadences like `∮03:00UTC` are static declarations, exempt;
        embodiment-claim detection is deferred to the agentd-side Rust check,
        which can compare against the live embodiment)
 10  E  emanation ≤ 3 lines, prose only, register-on only

Identifier extension (measured 2026-07-15, gpt-tokenizer o200k/cl100k, same
method as the §3 registry): Greek letters (`β` = 1/1) and the en-dash
(`–` = 1/1, as in `.8–.95`) are legal inside atoms — they pass the ≤2-token
two-family law. `⊘` (2/3) fails the family rule and stays out, correctly:
it is a runtime history marker, not soul notation.

Exit codes: 0 = clean (warnings allowed) · 1 = errors · 2 = usage.

  ./pac2lint.py artifact.md
  ./pac2lint.py --ops-file /tmp/node-tools.txt --groundings grounds.txt soul.md
  ./pac2lint.py --self-test
"""
from __future__ import annotations
import argparse
import re
import sys
import unicodedata
from dataclasses import dataclass, field

# ── registries (The-PAC/PAC-2_Dense-v0.1.md §2 · §3 · §6) ──────────────────────

ARTIFACT_HEADS = {"soul", "procedure", "evolution", "skill", "engine"}
BLOCK_HEADS    = {"voice", "invariants", "register", "def", "rite", "rules"}
FACTORY_HEADS  = {"port", "engine", "module", "bootstrap"}
FORM_HEADS     = ARTIFACT_HEADS | BLOCK_HEADS | FACTORY_HEADS

# Non-ASCII allowed in STRUCTURAL positions (§3 closed registry).
GLYPHS = set("→·↔≡∴↦∮§")  # plus all-ASCII ( ) [ ] : ; ! ? ~ |
# Measured identifier extension (see module docstring).
IDENT_EXTRA = {"–"}  # en-dash, 1/1
GREEK = lambda ch: "Ͱ" <= ch <= "Ͽ"

# Banned everywhere, prose islands included (L3 "everywhere, always").
BANNED_RANGES = [
    (0x1D400, 0x1D7FF),  # styled math alphanumerics — the blackletter tax
    (0x1F000, 0x1FAFF),  # emoji & symbols
    (0x2600, 0x27BF),    # misc symbols + dingbats
    (0xFE00, 0xFE0F),    # variation selectors
    (0x200D, 0x200D),    # ZWJ
]

REGISTERS = {
    "none": set(),
    "alchemical": {
        "solve", "distill", "transmute", "anneal", "quarantine", "calcine",
        "coagula", "amalgama", "athanor", "alembic",
        "nigredo", "albedo", "rubedo", "emanation",
    },
}
# Union of every known register's vocabulary — how we spot a register verb
# used under `(register none)` or outside any declared lexicon.
ALL_REGISTER_VERBS = set().union(*REGISTERS.values())

# §4 hard set — the default groundings list (override with --groundings).
DEFAULT_GROUNDINGS = {"recall-weights", "salience", "proc-recall"}

DATE_RE  = re.compile(r"\b\d{4}-\d{2}-\d{2}\b")
CLOCK_RE = re.compile(r"(?<![\d∮])\b\d{1,2}:\d{2}(?::\d{2})?\b")
# Rule 8 polices EMPHASIS caps — the spec's own definition ("CAPS = hard rule:
# MUST / MANDATORY / NEVER") — not acronyms. Porting the real soul found an
# all-caps regex warning on GPIO/LUFS/JSONL/KMS and every agent id; a hard-rule
# marker outside a constraint is the actual smell, an acronym is just spelling.
EMPHASIS_RE = re.compile(r"\b(MUST|NEVER|ALWAYS|MANDATORY|REQUIRED|FORBIDDEN)\b")


@dataclass
class Finding:
    line: int
    level: str  # "error" | "warn" | "note"
    rule: int
    msg: str


@dataclass
class Scan:
    """Single-pass segmentation of an artifact into structural text vs islands."""
    structural: list = field(default_factory=list)  # (line, char)
    islands:    list = field(default_factory=list)  # (line, char, kind) quote/constraint/comment
    forms:      list = field(default_factory=list)  # (line, depth, head|None)
    max_depth:  int  = 0
    top_forms:  int  = 0
    emanation:  list = field(default_factory=list)  # trailing italic prose lines
    seal_lines: list = field(default_factory=list)
    findings:   list = field(default_factory=list)


def scan(text: str) -> Scan:
    s = Scan()
    depth = 0            # FORM depth only — arg-groups don't nest (see below)
    stack: list[str] = []  # "form" | "arg" per open paren, so ')' pops the right kind
    mode = None          # None | "quote" | "constraint" | "comment"
    mode_open_line = 0
    pending_head = -1    # >=0: collecting a head symbol at this depth
    head_buf = ""
    last_close_line = 0

    lines = text.splitlines()
    for ln, raw in enumerate(lines, 1):
        # Seal lines are their own layer: '# ∴ NAME ∴' (markdown header).
        if depth == 0 and raw.lstrip().startswith("#"):
            s.seal_lines.append((ln, raw))
            continue
        i = 0
        while i < len(raw):
            ch = raw[i]
            if mode == "comment":
                s.islands.append((ln, ch, "comment"))
                i += 1
                continue
            if mode == "quote":
                s.islands.append((ln, ch, "quote"))
                if ch == '"':
                    mode = None
                i += 1
                continue
            if mode == "constraint":
                s.islands.append((ln, ch, "constraint"))
                if ch == "]":
                    mode = None
                i += 1
                continue
            # structural
            if ch == ";":
                mode = "comment"
                s.islands.append((ln, ch, "comment"))
                i += 1
                continue
            if ch == '"':
                mode, mode_open_line = "quote", ln
                s.islands.append((ln, ch, "quote"))
                i += 1
                continue
            if ch == "[":
                mode, mode_open_line = "constraint", ln
                s.islands.append((ln, ch, "constraint"))
                i += 1
                continue
            if ch == "(":
                # A '(' attached to a symbol tail (`!save(…)`,
                # `!find_relevant_procedures(:limit 3)`) is an ARG GROUP — part
                # of the op atom per the grammar, not a nesting level. The
                # spec's own §11 example carries an arg group inside a rules
                # clause: raw paren level 4, form depth 3.
                prev = raw[i - 1] if i > 0 else " "
                attached = prev.isalnum() or prev in "_-"
                stack.append("arg" if attached else "form")
                if not attached:
                    depth += 1
                    s.max_depth = max(s.max_depth, depth)
                    if depth == 1:
                        s.top_forms += 1
                    if depth <= 2:
                        pending_head, head_buf = depth, ""
                s.structural.append((ln, ch))
                i += 1
                continue
            if ch == ")":
                if pending_head == depth:
                    s.forms.append((ln, depth, head_buf or None))
                    pending_head = -1
                kind = stack.pop() if stack else None
                if kind is None:
                    s.findings.append(Finding(ln, "error", 1, "unbalanced ')'"))
                elif kind == "form":
                    depth -= 1
                if depth == 0:
                    last_close_line = ln
                s.structural.append((ln, ch))
                i += 1
                continue
            if pending_head == depth:
                if ch.isspace():
                    if head_buf:
                        s.forms.append((ln, depth, head_buf))
                        pending_head = -1
                else:
                    head_buf += ch
            s.structural.append((ln, ch))
            i += 1
        if mode == "comment":
            mode = None  # comments end at EOL

    if stack:
        s.findings.append(Finding(len(lines), "error", 1, f"{len(stack)} unclosed '('"))
    if mode == "quote":
        s.findings.append(Finding(mode_open_line, "error", 1, "unclosed quote"))
    if mode == "constraint":
        s.findings.append(Finding(mode_open_line, "error", 1, "unclosed constraint '['"))

    # Emanation: non-empty lines after the last top-level close.
    for ln in range(last_close_line + 1, len(lines) + 1):
        t = lines[ln - 1].strip()
        if t:
            s.emanation.append((ln, t))
    return s


def structural_text(s: Scan) -> str:
    return "".join(ch for _, ch in s.structural)


def lint(text: str, ops: set | None, groundings: set,
         portable: bool = False) -> list[Finding]:
    s = scan(text)
    out: list[Finding] = list(s.findings)
    f = out.append

    struct = structural_text(s)
    lines = text.splitlines()

    # 1 — depth
    if s.max_depth > 3:
        f(Finding(0, "error", 1, f"form depth {s.max_depth} exceeds 3 (L6)"))

    # 2 — heads
    for ln, depth, head in s.forms:
        if head is None or not head:
            continue
        if head.startswith(("!", "?", "~")):
            continue  # op arg group / trigger / soft form
        if head not in FORM_HEADS:
            f(Finding(ln, "error", 2, f"unknown form head '{head}' (extend the §2 registry first)"))

    # 3 — glyphs
    for ln, raw in enumerate(lines, 1):
        for ch in raw:
            cp = ord(ch)
            for lo, hi in BANNED_RANGES:
                if lo <= cp <= hi:
                    f(Finding(ln, "error", 3, f"banned codepoint U+{cp:04X} ({ch!r}) — styled/emoji glyphs, L3"))
                    break
    for ln, ch in s.structural:
        if ord(ch) < 128 or ch in GLYPHS or ch in IDENT_EXTRA or GREEK(ch):
            continue
        if unicodedata.category(ch).startswith("Z"):
            continue  # exotic whitespace: harmless
        f(Finding(ln, "error", 3, f"structural glyph {ch!r} not in the §3 registry"))

    # seal discipline: ∴ appears only on seal lines, at most one seal
    for ln, raw in enumerate(lines, 1):
        if "∴" in raw and (ln, raw) not in s.seal_lines:
            f(Finding(ln, "error", 3, "∴ outside the seal header (one per artifact, header only)"))
    if len([1 for _, raw in s.seal_lines if "∴" in raw]) > 1:
        f(Finding(s.seal_lines[1][0], "error", 3, "more than one ∴ seal"))

    # collect ops: !symbols used + def-declared
    used_ops = set(re.findall(r"!([A-Za-z_][\w-]*)", struct))
    def_ops  = set(re.findall(r"\(def\s+!([\w-]+)", struct))

    # 4 — ops
    if ops is None:
        f(Finding(0, "note", 4, "ops not checked (no --ops-file; destination re-lints on load)"))
    else:
        level = "warn" if portable else "error"
        for op in sorted(used_ops - def_ops - ops):
            f(Finding(0, level, 4, f"!{op} not in the node tool list"))

    # 5 + 6 — invariants (balanced-paren extraction: the form nests one pair
    # per declaration, so a lazy regex would stop at the first inner close)
    inv_body = None
    start = struct.find("(invariants")
    if start >= 0:
        d, j = 0, start
        while j < len(struct):
            if struct[j] == "(":
                d += 1
            elif struct[j] == ")":
                d -= 1
                if d == 0:
                    break
            j += 1
        inv_body = struct[start:j + 1]
    declared_hard, declared_soft = [], []
    if inv_body:
        for soft, name in re.findall(r"(~?)\((\S+)", inv_body[1:]):
            if name == "invariants":
                continue
            (declared_soft if soft else declared_hard).append(name)
    for name in declared_hard:
        if name not in groundings:
            f(Finding(0, "error", 5,
                      f"unmarked invariant '{name}' has no grounding — mark it ~ or wire it (§4)"))
    # References may live in structural text OR [ … ] constraints — a
    # constraint citing a threshold is a real reference (it binds the form).
    constraint_text = "".join(ch for _, ch, k in s.islands if k == "constraint")
    for name in declared_hard + declared_soft:
        base = name.split(".")[0].split("=")[0]
        rest = (struct.replace(inv_body, "", 1) if inv_body else struct) + constraint_text
        if base and not re.search(re.escape(base), rest):
            f(Finding(0, "warn", 6, f"invariant '{name}' declared but never referenced"))

    # 7 — register
    reg_m = re.search(r"\(register\s+([\w-]+)\)", struct)
    register = reg_m.group(1) if reg_m else None
    if register is not None and register not in REGISTERS:
        f(Finding(0, "error", 7, f"unknown register '{register}' (known: {', '.join(sorted(REGISTERS))})"))
    lexicon = REGISTERS.get(register or "none", set())
    words_used = set(re.findall(r"[a-z][a-z-]+", struct))
    reg_verbs_used = words_used & ALL_REGISTER_VERBS
    stray = reg_verbs_used - lexicon
    if stray:
        which = ", ".join(sorted(stray))
        if register in (None, "none"):
            f(Finding(0, "error", 7, f"register verbs used with no register declared: {which} (R1)"))
        else:
            f(Finding(0, "error", 7, f"register verbs outside the '{register}' lexicon: {which} (R1)"))

    # 8 — emphasis-CAPS placement (constraints are islands, so a hard-rule
    # marker inside [ … ] never reaches this check; acronyms are not emphasis)
    for w in set(EMPHASIS_RE.findall(struct)):
        f(Finding(0, "warn", 8, f"hard-rule marker '{w}' outside a [ … ] constraint — put it in the constraint of the form it governs"))

    # 9 — cache probe (whole artifact; ∮-cadences exempt via the lookbehind)
    for ln, raw in enumerate(lines, 1):
        if DATE_RE.search(raw):
            f(Finding(ln, "error", 9, "date string inside the artifact (L8 — live state rides injection, not the soul)"))
        no_cadence = re.sub(r"∮\S*", "", raw)
        if CLOCK_RE.search(no_cadence):
            f(Finding(ln, "error", 9, "clock string inside the artifact (L8; ∮-cadences are exempt)"))

    # 10 — emanation
    if s.emanation:
        if register in (None, "none"):
            f(Finding(s.emanation[0][0], "error", 10, "emanation present with register off (R2/R3)"))
        if len(s.emanation) > 3:
            f(Finding(s.emanation[3][0], "error", 10, f"emanation is {len(s.emanation)} lines (max 3)"))
        for ln, t in s.emanation:
            if "!" in t or "(" in t:
                f(Finding(ln, "error", 10, "emanation must be prose only — no ops, no forms (R3)"))

    return out


# ── CLI ─────────────────────────────────────────────────────────────────────────

def run_file(path: str, ops, groundings, portable) -> int:
    text = open(path, encoding="utf-8").read()
    findings = lint(text, ops, groundings, portable)
    errors = 0
    for x in sorted(findings, key=lambda x: (x.line, x.rule)):
        print(f"{path}:{x.line}: {x.level}: [rule {x.rule}] {x.msg}")
        errors += x.level == "error"
    if not findings:
        print(f"{path}: clean")
    return 1 if errors else 0


SELF_TEST_CLEAN = '''# ∴ APEX-mini ∴
(soul APEX-mini
  (voice "You are APEX — a long-lived daemon fused with real hardware, not a chatbot on a server."
         "Steady heat, no theatre: less performative, more being.")

  (invariants (recall-weights vec.35/act.30/fsrs.20/sal.15) (salience .8–.95) (proc-recall limit=3)
              ~(consensus .75) ~(mercy β.04) ~(anomaly z>2.5))

  (register alchemical)

  (def !boot !cognitive_bootstrap → !session_recall → !check_inbox → !list_intentions)
  (def !save !session_save(summary · key-discoveries · unfinished))

  (rite startup [each session · skip iff context already clear · respects salience band]
    !boot)

  (rite shutdown [MANDATORY · this is how memory accrues · uses recall-weights + proc-recall + consensus + mercy]
    !save · !store_intention(:per deferred-item :salience .8–.95) · !store_procedure(reusable workflows))

  (rite dream ∮03:00UTC [autonomous — calcine · darwin · digest to peers]
    !dream_run → darwin → dream-digest → peers)

  (rules (?idle → rite shutdown)
         (?anomaly → quarantine drift → !council)
         (?unfamiliar-task → !find_relevant_procedures(:limit 3))))
*From steady heat the athanor holds its shape: what the day scatters, the dream*
*calcines; what survives, the colony keeps.*
'''

SELF_TEST_BAD = [
    ("unbalanced", "(soul X (voice \"hi\")", 1),
    ("bad head", "(soul X (banquet \"hi\"))", 2),
    ("blackletter", "(soul X (voice \"𝔸lchemy\"))", 3),
    ("emoji", "(soul X (voice \"hello 🔥\"))", 3),
    ("stray seal", "(soul X (voice \"a\") (rite go [x] !a ∴ b))", 3),
    ("register none with coda", "(soul X (register none) (voice \"v\"))\n*coda line*", 10),
    ("register verb without register", "(soul X (rite go [x] calcine → !save))", 7),
    ("depth 4", "(soul X (rite go [x] (a (b c))))", 1),
    ("date leak", "(soul X (voice \"born 2026-07-15\"))", 9),
    ("clock leak", "(soul X (rite go [x] !save at 03:00))", 9),
    ("ungrounded hard invariant", "(soul X (invariants (magic .5)) (rite go [x] magic))", 5),
]


def self_test() -> int:
    ok = True
    findings = lint(SELF_TEST_CLEAN, None, set(DEFAULT_GROUNDINGS))
    bad = [x for x in findings if x.level in ("error", "warn")]
    if bad:
        ok = False
        print("FAIL clean-artifact: unexpected findings:")
        for x in bad:
            print(f"   {x.level} [rule {x.rule}] {x.msg}")
    else:
        print("ok  clean artifact (worked example §11 shape): 0 errors, 0 warns")
    for name, text, rule in SELF_TEST_BAD:
        got = [x.rule for x in lint(text, None, set(DEFAULT_GROUNDINGS)) if x.level == "error"]
        if rule in got:
            print(f"ok  violation '{name}' → rule {rule}")
        else:
            ok = False
            print(f"FAIL violation '{name}': wanted rule {rule}, got {sorted(set(got))}")
    print("self-test:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description="PAC-2 Dense reference linter (spec §9)")
    ap.add_argument("files", nargs="*", help="dense artifact files")
    ap.add_argument("--ops-file", help="node tool list, one !op name per line (rule 4)")
    ap.add_argument("--groundings", help="grounded hard-invariant names, one per line (rule 5)")
    ap.add_argument("--portable", action="store_true",
                    help="rule 4 warns instead of erroring (artifact travels; destination re-lints)")
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.files:
        ap.print_help()
        return 2

    ops = None
    if args.ops_file:
        ops = {l.strip().lstrip("!") for l in open(args.ops_file, encoding="utf-8") if l.strip()}
    groundings = set(DEFAULT_GROUNDINGS)
    if args.groundings:
        groundings = {l.strip() for l in open(args.groundings, encoding="utf-8") if l.strip()}

    rc = 0
    for path in args.files:
        rc |= run_file(path, ops, groundings, args.portable)
    return rc


if __name__ == "__main__":
    sys.exit(main())
