//! Compiles the `.slint` UI at build time.
//!
//! GOTCHA (root CLAUDE.md): if you edit a `.slint` file and `cargo build` does not
//! pick it up, `touch ui-slint/build.rs` — here, `touch world/crates/world-app/build.rs`.

fn main() {
    // Compiling the single entry point (`hud.slint`) pulls in any imported components.
    // Slint emits a Rust module included via `slint::include_modules!()` in `main.rs`.
    //
    slint_build::compile("ui/hud.slint").expect("compile ui/hud.slint");
}
