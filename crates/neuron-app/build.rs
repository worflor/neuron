//! Compile the Slint UI into generated Rust at build time. `slint::include_modules!()` in
//! `src/ui.rs` pulls in the generated component types (`AppWindow`, all the shared structs and
//! callbacks). The software renderer keeps idle cost near zero — true to the anti-bloat motto.

fn main() {
    // The entry .slint file `include`s every panel; recompile when any of them changes.
    slint_build::compile("ui/app.slint").expect("Slint compilation failed");
}
