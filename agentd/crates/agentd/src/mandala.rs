//! Mandala Mode — the pure core + on-disk tree (Fabrica M1a, docs/fabrica.md).
//!
//! Depth for the worker tier, with zero new concurrency: an M1a *cell* is one
//! worker (a width-1 `task_fanout` under a mandala) wrapped in geometry — an
//! address in the tree, a budget vector that strictly descends, and the
//! invariant axis. The root writes objective + definition-of-done + the verify
//! command ONCE, content-addressed; the DRIVER injects those exact bytes into
//! every cell's directive — mechanically, so no level can paraphrase the goal
//! (the telephone game is impossible by construction, not by discipline). The
//! verify command runs through the worker's own policied exec path, never a
//! driver raw-exec (the charter's security line).
//!
//! Code speaks engineering here — `CellForm`, `Lattice`, `BudgetVec` — no
//! hexagram semantics, no numerology in enums or logs (charter: doctrine
//! placement). The doctrine lives in `docs/fabrica-skill.md`, the transmission
//! format agents internalize via Cerebro.
//!
//! THE FILESYSTEM IS THE TREE: `<log_dir>/worktrees/<mandala>/<addr>.json` is
//! the only authoritative structure — the only posture compatible with a
//! daemon that swaps its own binary mid-run. Reconstruction scans the
//! directory; a cell whose parent vanished reparents to its nearest living
//! ancestor by address prefix, and its contract stays valid because the
//! contract is with the ROOT's invariant, not with the parent it lost.
//!
//! M1a scope fence: SPINE/LEAF forms only (weight 0 — no guards to arm), one
//! cell spawns at a time. Parallel cells (B), barriers + git worktrees (J):
//! M1b. Measures + vouchers/sub-conductors (R): M1c. The 64-cell composition
//! table + its exhaustive (36, 12, 16) test + changing-line adaptation +
//! epochs: M1d. This file still ships the full pure vocabulary (forms, all
//! five lattice widths, budget algebra) so later slices arm bits, not rewrite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── CellForm: three orthogonal risk bits ────────────────────────────────────

/// A cell's form: bit0 = J (join/barrier), bit1 = R (recurrence), bit2 = B
/// (breadth). Each set bit arms one unbounded dimension and therefore
/// mandates one guard — Hamming weight = number of guards that must be
/// configured. LEAF is SPINE at the budget floor, not a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellForm(pub u8);

// The full form vocabulary ships at M1a so later slices arm bits instead of
// rewriting — only SPINE is constructed at runtime until M1b (dead_code is
// the point, not an oversight).
#[allow(dead_code)]
impl CellForm {
    pub const SPINE: Self = CellForm(0b000);
    pub const GATE: Self = CellForm(0b001);
    pub const SPIRAL: Self = CellForm(0b010);
    pub const FORGE_FORM: Self = CellForm(0b011); // lap→verify→lap (FORGE the agent is elsewhere)
    pub const FAN: Self = CellForm(0b100);
    pub const DIAMOND: Self = CellForm(0b101);
    pub const SWARM: Self = CellForm(0b110);
    pub const MANDALA: Self = CellForm(0b111);

    pub fn joins(self) -> bool { self.0 & 0b001 != 0 }
    pub fn recurs(self) -> bool { self.0 & 0b010 != 0 }
    pub fn branches(self) -> bool { self.0 & 0b100 != 0 }

    /// Hamming weight = armed-guard count = risk order = ship order.
    pub fn weight(self) -> u32 { (self.0 & 0b111).count_ones() }

    /// Well-formedness is total: every set bit's guard is configured. At M1a
    /// only weight-0 forms are admitted at runtime, so all three guards may
    /// legitimately be absent — the check still ships whole for M1b/M1c.
    pub fn guards_armed(self, breadth_cap: Option<u8>, measure: Option<&str>, barrier_timeout_s: Option<u64>) -> bool {
        (!self.branches() || breadth_cap.is_some())
            && (!self.recurs() || measure.is_some())
            && (!self.joins() || barrier_timeout_s.is_some())
    }
}

// ── Addr: position IS identity ───────────────────────────────────────────────

/// A cell's address — `"0"` is the root, `"0.3.1"` root→3rd child→1st. From
/// this one string: the disk file, the (future M1b) branch name, ancestry by
/// prefix, depth by segment count. String ops only, no registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Addr(pub String);

impl Addr {
    pub const ROOT: &'static str = "0";

    /// Parse + validate: dot-separated decimal segments, root segment "0".
    /// Rejects anything that could escape the tree dir as a filename.
    pub fn parse(s: &str) -> Option<Addr> {
        if s.is_empty() || s.len() > 64 { return None; }
        let mut segs = s.split('.');
        if segs.next() != Some("0") { return None; }
        for seg in segs {
            if seg.is_empty() || seg.len() > 6 || !seg.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        Some(Addr(s.to_string()))
    }

    pub fn parent(&self) -> Option<Addr> {
        self.0.rsplit_once('.').map(|(head, _)| Addr(head.to_string()))
    }

    /// Depth below the root: root = 0, "0.3" = 1, "0.3.1" = 2.
    pub fn depth(&self) -> u8 {
        self.0.bytes().filter(|&b| b == b'.').count() as u8
    }

    pub fn child(&self, n: u32) -> Addr {
        Addr(format!("{}.{n}", self.0))
    }

    pub fn is_ancestor_of(&self, other: &Addr) -> bool {
        other.0.len() > self.0.len()
            && other.0.starts_with(&self.0)
            && other.0.as_bytes()[self.0.len()] == b'.'
    }

    /// The git branch a B-cell child will own (M1b) — named here so the
    /// address stays the single identity from day one.
    #[allow(dead_code)]
    pub fn branch(&self) -> String {
        format!("apex/w/{}", self.0)
    }

    pub fn file(&self, tree_dir: &Path) -> PathBuf {
        tree_dir.join(format!("{}.json", self.0))
    }
}

// ── BudgetVec: the descent theorem ──────────────────────────────────────────

/// The conserved vector. Admission requires STRICT decrease on depth and
/// non-increase elsewhere, all components positive — termination is a
/// theorem (well-founded descent), not a hope. Renewal (M1c) spends the
/// parent's vector; budget never appears from nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetVec {
    pub depth: u8,
    pub cells: u8,
    pub steps: u16,
    pub deadline_s: u64,
}

/// Depth ceiling (default, tunable) — both doctrine and debuggability: a
/// depth-6 chain of stall ceilings is still humanly traceable.
pub const DEPTH_CEIL: u8 = 6;
/// Geometry budget ceiling — ≤ 64 OPEN cells per mandala (parked cells hold
/// their geometry cell; the thermal budget is a different quantity).
pub const CELLS_CEIL: u8 = 64;

/// The admission law. `ring_free` is the parent ring's remaining width.
pub fn admissible(parent: &BudgetVec, child: &BudgetVec, ring_free: u8) -> bool {
    child.depth < parent.depth
        && child.cells <= parent.cells
        && child.cells >= 1
        && ring_free >= 1
        && child.steps <= parent.steps
        && child.deadline_s <= parent.deadline_s
        && child.depth > 0
        && child.steps > 0
}

/// The default child vector: depth − 1, steps contracted by the shipped
/// ratio 0.5 (floor 1) — total work over any chain ≤ 2× root, statable in
/// one line. Deadline inherits (the mandala's horizon is shared).
pub fn contract_child(parent: &BudgetVec) -> BudgetVec {
    BudgetVec {
        depth: parent.depth.saturating_sub(1),
        cells: 1, // a leaf holds itself; sub-conductor slices arrive with vouchers (M1c)
        steps: (parent.steps / 2).max(1),
        deadline_s: parent.deadline_s,
    }
}

// ── Lattice presets: declared factorizations of the geometry budget ─────────

/// Ring-width presets (the charter's five). Pure and table-driven; runtime
/// enforcement beyond width-1 arrives with the B bit (M1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lattice {
    Spine,  // 2⁶ — bisection walks
    Quad,   // 4³ — balanced refactors; four mutually adjacent siblings
    Fan,    // 8² — embarrassingly parallel sweeps
    Spiral, // Fibonacci widths — unknown decompositions, grow where progress is
    Funnel, // decreasing widths — synthesis toward one report
}

impl Lattice {
    pub fn parse(s: &str) -> Option<Lattice> {
        match s {
            "spine" => Some(Lattice::Spine),
            "quad" => Some(Lattice::Quad),
            "fan" => Some(Lattice::Fan),
            "spiral" => Some(Lattice::Spiral),
            "funnel" => Some(Lattice::Funnel),
            _ => None,
        }
    }
}

/// Ring `ring` (0-based, root's children = ring 0) → width cap. Pure; the
/// product of widths down any lattice stays ≤ CELLS_CEIL by construction of
/// the presets (unit-tested).
pub fn ring_width(lattice: Lattice, ring: u8) -> u8 {
    match lattice {
        Lattice::Spine => if ring < 6 { 2 } else { 0 },
        Lattice::Quad => if ring < 3 { 4 } else { 0 },
        Lattice::Fan => if ring < 2 { 8 } else { 0 },
        // Per-parent widths, Π ≤ 64 (the conservation law): fib caps at ring 4
        // (1·1·2·3·5 = 30) — deeper spiral growth is M1c renewal, never free width.
        Lattice::Spiral => match ring {
            0 | 1 => 1,
            2 => 2,
            3 => 3,
            4 => 5,
            _ => 0,
        },
        Lattice::Funnel => match ring {
            0 => 9,
            1 => 4,
            2 => 1,
            _ => 0,
        },
    }
}

// ── The invariant axis ───────────────────────────────────────────────────────

/// Written ONCE at mandala creation, content-addressed; every cell carries a
/// reference to these exact bytes. Charters contract; the axis is rigid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invariant {
    pub objective: String,
    pub done_when: String,
    /// The root's verify command — every check at every depth runs THIS, via
    /// the worker's policied exec path. Local success cannot diverge from
    /// global progress.
    pub verify: String,
    pub hash: String,
}

impl Invariant {
    pub fn new(objective: &str, done_when: &str, verify: &str) -> Invariant {
        let canonical = format!("objective:{objective}\ndone_when:{done_when}\nverify:{verify}");
        let hash = hex_digest(canonical.as_bytes());
        Invariant {
            objective: objective.to_string(),
            done_when: done_when.to_string(),
            verify: verify.to_string(),
            hash,
        }
    }

    /// The block the DRIVER injects verbatim into every cell directive — the
    /// axis rides mechanically, so no level can paraphrase it. Byte-stable
    /// for a given mandala (cache-friendly across its cells).
    pub fn directive_block(&self) -> String {
        format!(
            "MANDALA INVARIANT [{}] — the root's exact contract; it never changes at any depth:\n\
             OBJECTIVE: {}\nDONE WHEN: {}\nVERIFY WITH: `{}` (run it through your normal tools before claiming done)",
            &self.hash[..12], self.objective, self.done_when, self.verify
        )
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Cell + mandala records (the filesystem is the tree) ─────────────────────

/// One cell's on-disk record — `<tree_dir>/<addr>.json`, tmp+rename. The
/// worker fields bind the cell to its W-tier execution substrate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellRecord {
    pub addr: Addr,
    pub form: CellForm,
    pub task: String,
    pub budget: BudgetVec,
    pub invariant_hash: String,
    /// The W-tier worker executing this cell (its evidence file is the cell's
    /// artifact). None until admitted.
    #[serde(default)]
    pub worker: Option<u64>,
    /// Mirror of the worker's terminal state ("open" until then) — the tree
    /// stays readable without joining workers.json.
    #[serde(default = "default_open")]
    pub state: String,
    #[serde(default)]
    pub evidence: Option<String>,
    /// Set on reload when the recorded parent's file is missing: the address
    /// this cell now reports to (nearest living ancestor). The contract is
    /// with the root's invariant, so reparenting is safe by construction.
    #[serde(default)]
    pub reparented_to: Option<Addr>,
}

fn default_open() -> String { "open".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MandalaRecord {
    pub id: u64,
    /// The conductor session that created (and drives) this mandala.
    pub conductor: u64,
    pub lattice: Lattice,
    pub budget: BudgetVec,
    pub invariant: Invariant,
    #[serde(default)]
    pub state: String, // "open" | "parked" | "closed"
}

/// A cell is OPEN while its worker hasn't reached a terminal state — open
/// cells are what the geometry budget counts (parked workers still hold
/// their geometry cell; thermal residency is the other, separate budget).
pub fn is_open_state(state: &str) -> bool {
    !matches!(state, "done" | "failed" | "cancelled")
}

// ── Tree I/O ─────────────────────────────────────────────────────────────────

pub fn save_cell(tree_dir: &Path, cell: &CellRecord) {
    let _ = std::fs::create_dir_all(tree_dir);
    if let Ok(json) = serde_json::to_string_pretty(cell) {
        let path = cell.addr.file(tree_dir);
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Rebuild a mandala's cell index by scanning its tree dir — the only
/// authoritative structure. Unparseable files are skipped (never fatal);
/// missing parents reparent by prefix.
pub fn load_tree(tree_dir: &Path) -> HashMap<String, CellRecord> {
    let mut cells: HashMap<String, CellRecord> = HashMap::new();
    let Ok(rd) = std::fs::read_dir(tree_dir) else { return cells };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(cell) = serde_json::from_str::<CellRecord>(&text) else { continue };
        // The filename is authoritative for the address (position IS identity);
        // a record whose body disagrees with its position is skipped as corrupt.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if Addr::parse(stem).as_ref() != Some(&cell.addr) { continue; }
        cells.insert(cell.addr.0.clone(), cell);
    }
    reparent_orphans(&mut cells);
    cells
}

/// A cell whose parent's file vanished attaches to its nearest LIVING
/// ancestor by address prefix. Pure over the map; returns the addrs that
/// moved (for logging + re-save).
pub fn reparent_orphans(cells: &mut HashMap<String, CellRecord>) -> Vec<Addr> {
    let addrs: Vec<Addr> = cells.values().map(|c| c.addr.clone()).collect();
    let mut moved = Vec::new();
    for addr in &addrs {
        let Some(parent) = addr.parent() else { continue }; // root has no parent
        if cells.contains_key(&parent.0) { continue; }
        // Walk up to the nearest living ancestor (the root always lives; a
        // fully-orphaned fragment attaches to the root record if present).
        let mut anc = parent.parent();
        let target = loop {
            match anc {
                Some(a) if cells.contains_key(&a.0) => break Some(a),
                Some(a) => anc = a.parent(),
                None => break None,
            }
        };
        if let Some(t) = target {
            if let Some(cell) = cells.get_mut(&addr.0) {
                if cell.reparented_to.as_ref() != Some(&t) {
                    cell.reparented_to = Some(t.clone());
                    moved.push(addr.clone());
                }
            }
        }
    }
    moved
}

/// Next child index under `parent` — one past the highest ordinal ever seen
/// at that level among ALL descendants, not just direct children: a vanished
/// child whose own descendants survive must never have its address re-minted
/// (position is identity, forever — a reused address would splice a new cell
/// into an orphaned lineage).
pub fn next_child_ordinal(cells: &HashMap<String, CellRecord>, parent: &Addr) -> u32 {
    let child_depth = parent.depth() as usize + 1;
    cells
        .keys()
        .filter_map(|k| {
            let a = Addr(k.clone());
            if parent.is_ancestor_of(&a) {
                a.0.split('.').nth(child_depth).and_then(|n| n.parse::<u32>().ok())
            } else {
                None
            }
        })
        .max()
        .map(|n| n + 1)
        .unwrap_or(0)
}

pub fn open_cells(cells: &HashMap<String, CellRecord>) -> usize {
    cells.values().filter(|c| is_open_state(&c.state)).count()
}

// ── mandalas.json ────────────────────────────────────────────────────────────

pub fn save_mandalas(mandalas: &HashMap<u64, MandalaRecord>, path: &Path) {
    let mut snapshot: Vec<&MandalaRecord> = mandalas.values().collect();
    snapshot.sort_by_key(|m| m.id);
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub fn load_mandalas(path: &Path) -> HashMap<u64, MandalaRecord> {
    let list: Vec<MandalaRecord> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    list.into_iter().map(|m| (m.id, m)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_form_bits_and_weight() {
        assert_eq!(CellForm::SPINE.weight(), 0);
        assert_eq!(CellForm::GATE.weight(), 1);
        assert_eq!(CellForm::MANDALA.weight(), 3);
        assert!(CellForm::DIAMOND.branches() && CellForm::DIAMOND.joins() && !CellForm::DIAMOND.recurs());
        // Well-formedness: every set bit needs its guard — weight 0 needs none.
        assert!(CellForm::SPINE.guards_armed(None, None, None));
        assert!(!CellForm::GATE.guards_armed(None, None, None));
        assert!(CellForm::GATE.guards_armed(None, None, Some(120)));
        assert!(!CellForm::MANDALA.guards_armed(Some(4), Some("cargo test"), None));
        assert!(CellForm::MANDALA.guards_armed(Some(4), Some("cargo test"), Some(120)));
    }

    #[test]
    fn addr_algebra() {
        let a = Addr::parse("0.3.1").unwrap();
        assert_eq!(a.depth(), 2);
        assert_eq!(a.parent().unwrap().0, "0.3");
        assert_eq!(a.child(2).0, "0.3.1.2");
        assert_eq!(a.branch(), "apex/w/0.3.1");
        let root = Addr::parse("0").unwrap();
        assert_eq!(root.depth(), 0);
        assert!(root.parent().is_none());
        assert!(root.is_ancestor_of(&a));
        assert!(!a.is_ancestor_of(&root));
        // "0.31" is not a child of "0.3" — prefix check is segment-aware.
        assert!(!Addr::parse("0.3").unwrap().is_ancestor_of(&Addr::parse("0.31").unwrap()));
        // Validation: escapes and junk refuse.
        for bad in ["", "1", "0.", "0..1", "0.x", "0.1234567", "../etc", "0/1"] {
            assert!(Addr::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn budget_descent_is_a_theorem() {
        let parent = BudgetVec { depth: 3, cells: 8, steps: 12, deadline_s: 3600 };
        let child = contract_child(&parent);
        assert_eq!(child.depth, 2);
        assert_eq!(child.steps, 6); // the shipped 0.5 contraction
        assert!(admissible(&parent, &child, 2));
        // Strict depth decrease is mandatory.
        assert!(!admissible(&parent, &BudgetVec { depth: 3, ..child }, 2));
        // Non-increase elsewhere.
        assert!(!admissible(&parent, &BudgetVec { steps: 13, ..child }, 2));
        assert!(!admissible(&parent, &BudgetVec { deadline_s: 7200, ..child }, 2));
        // All components positive; a full ring refuses.
        assert!(!admissible(&parent, &BudgetVec { depth: 0, ..child }, 2));
        assert!(!admissible(&parent, &BudgetVec { steps: 0, ..child }, 2));
        assert!(!admissible(&parent, &child, 0));
        // Depth exhausts: a depth-1 parent admits no child.
        let leaf_parent = BudgetVec { depth: 1, cells: 1, steps: 1, deadline_s: 60 };
        assert!(!admissible(&leaf_parent, &contract_child(&leaf_parent), 1));
    }

    #[test]
    fn lattice_widths_conserve_the_geometry_budget() {
        // Π(ring widths) ≤ 64 for every preset — the conservation law the
        // presets were chosen to satisfy (2⁶ = 4³ = 8² = 64).
        for (lat, name) in [
            (Lattice::Spine, "spine"), (Lattice::Quad, "quad"), (Lattice::Fan, "fan"),
            (Lattice::Spiral, "spiral"), (Lattice::Funnel, "funnel"),
        ] {
            let mut product: u64 = 1;
            for ring in 0..DEPTH_CEIL {
                let w = ring_width(lat, ring) as u64;
                if w == 0 { break; }
                product = product.saturating_mul(w);
            }
            assert!(product <= CELLS_CEIL as u64, "{name}: Π widths = {product}");
            assert_eq!(Lattice::parse(name), Some(lat));
        }
        assert_eq!(ring_width(Lattice::Spine, 0), 2);
        assert_eq!(ring_width(Lattice::Fan, 0), 8);
        assert_eq!(ring_width(Lattice::Fan, 2), 0); // depth ≤ 2 by preset
        assert_eq!(ring_width(Lattice::Spiral, 4), 5); // fibonacci
        assert_eq!(Lattice::parse("hexagram"), None);
    }

    #[test]
    fn invariant_is_content_addressed_and_rides_verbatim() {
        let a = Invariant::new("port the parser", "cargo test -p parser green", "cargo test -p parser");
        let b = Invariant::new("port the parser", "cargo test -p parser green", "cargo test -p parser");
        let c = Invariant::new("port the parser", "cargo test -p parser green", "cargo test --workspace");
        assert_eq!(a.hash, b.hash, "same bytes, same address");
        assert_ne!(a.hash, c.hash, "any drift changes the address");
        let block = a.directive_block();
        assert!(block.contains("port the parser"));
        assert!(block.contains("cargo test -p parser"));
        assert!(block.contains(&a.hash[..12]));
        assert!(block.contains("never changes at any depth"));
    }

    #[test]
    fn tree_round_trips_and_reparents_by_prefix() {
        let dir = std::env::temp_dir().join(format!("apexos-mandala-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mk = |addr: &str| CellRecord {
            addr: Addr::parse(addr).unwrap(),
            form: CellForm::SPINE,
            task: format!("task at {addr}"),
            budget: BudgetVec { depth: 3, cells: 1, steps: 4, deadline_s: 600 },
            invariant_hash: "abc".into(),
            worker: None, state: "open".into(), evidence: None, reparented_to: None,
        };
        for a in ["0", "0.0", "0.0.0", "0.1", "0.1.2"] {
            save_cell(&dir, &mk(a));
        }
        // Kill 0.1 — its child 0.1.2 must reparent to the root by prefix.
        std::fs::remove_file(dir.join("0.1.json")).unwrap();
        let cells = load_tree(&dir);
        assert_eq!(cells.len(), 4);
        assert_eq!(cells["0.1.2"].reparented_to.as_ref().unwrap().0, "0");
        assert!(cells["0.0.0"].reparented_to.is_none(), "intact chains don't move");
        // Ordinals never reuse an address: 0.1's file is gone but its lineage
        // (0.1.2) survives — the next root child must be 2, never 1.
        assert_eq!(next_child_ordinal(&cells, &Addr::parse("0").unwrap()), 2);
        assert_eq!(next_child_ordinal(&cells, &Addr::parse("0.0").unwrap()), 1);
        assert_eq!(open_cells(&cells), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_position_is_skipped() {
        // A record whose body disagrees with its filename is corrupt — position
        // IS identity, and the filename is the position.
        let dir = std::env::temp_dir().join(format!("apexos-mandala-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cell = CellRecord {
            addr: Addr::parse("0.5").unwrap(), form: CellForm::SPINE, task: "t".into(),
            budget: BudgetVec { depth: 1, cells: 1, steps: 1, deadline_s: 60 },
            invariant_hash: "x".into(), worker: None, state: "open".into(),
            evidence: None, reparented_to: None,
        };
        std::fs::write(dir.join("0.9.json"), serde_json::to_string(&cell).unwrap()).unwrap();
        assert!(load_tree(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mandalas_json_round_trips() {
        let dir = std::env::temp_dir().join(format!("apexos-mandalas-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mandalas.json");
        let mut m = HashMap::new();
        m.insert(1, MandalaRecord {
            id: 1, conductor: 84, lattice: Lattice::Spine,
            budget: BudgetVec { depth: 6, cells: 64, steps: 32, deadline_s: 86_400 },
            invariant: Invariant::new("o", "d", "v"),
            state: "open".into(),
        });
        save_mandalas(&m, &path);
        let back = load_mandalas(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[&1].conductor, 84);
        assert_eq!(back[&1].lattice, Lattice::Spine);
        assert!(load_mandalas(&dir.join("missing.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
