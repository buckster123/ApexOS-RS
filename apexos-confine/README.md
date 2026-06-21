# apexos-confine

> Path-confinement primitives — the FS-sandbox algorithm, std only, liftable on its own.

The mechanism behind ApexOS's filesystem sandbox, factored out so it can be tested in
isolation and reused anywhere: reject `..` (component-based), lenient-canonicalize (resolve
symlinks, tolerate non-existent write targets), and root containment with a read/secret split.
The caller supplies the *policy* (which roots, which secrets); this crate supplies the *mechanism*.

- **Key files:** `src/lib.rs` (`has_traversal`, `canonicalize_lenient`, `confine_fs`, `confine_to_roots`, `Denied`)
- **Depends on:** `std` only — zero ApexOS deps.
- **Lift via:** `cargo add apexos-confine`. Supply your own workspace/read-roots/secret predicate and render your own error strings from the structured `Denied` reasons. The unit tests include the symlink-escape (TOCTOU) case.

Part of [ApexOS-RS](https://github.com/buckster123/ApexOS-RS) — see [`PATTERNS.md`](../PATTERNS.md) (lift-me index) and [`docs/repo-map.md`](../docs/repo-map.md) (full map).
