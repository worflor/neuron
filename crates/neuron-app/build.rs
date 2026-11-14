// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../LICENSE.md.

//! Compile the Slint UI into generated Rust at build time. `slint::include_modules!()` in
//! `src/ui.rs` pulls in the generated component types (`AppWindow`, all the shared structs and
//! callbacks). The software renderer keeps idle cost near zero — true to the anti-bloat motto.
//! On Windows it also gives the executable neuron's mark as its icon.

#[allow(dead_code)]
#[path = "src/mark.rs"]
mod mark;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The entry .slint file `include`s every panel; recompile when any of them changes. Debug
    // builds carry element names and layout kinds, which the tests' geometry checks read.
    let debug = std::env::var("PROFILE").is_ok_and(|p| p != "release");
    // Every Slint warning fails the build. A binding loop is only a warning to Slint, and one
    // shipped: the page column's width fed back into the window's size and the UI flickered.
    std::env::set_var("SLINT_COMPILER_DENY_WARNINGS", "1");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config)?;
    println!("cargo:rerun-if-changed=src/mark.rs");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        let ico = std::path::Path::new(&std::env::var("OUT_DIR")?).join("neuron.ico");
        std::fs::write(&ico, mark::ico(&[16, 20, 24, 32, 40, 48, 64, 256]))?;
        let mut res = winresource::WindowsResource::new();
        res.set_icon(&ico.to_string_lossy());
        // A missing resource compiler costs the executable its icon, not the build.
        if let Err(e) = res.compile() {
            println!("cargo:warning=the executable has no icon: {e}");
        }
    }
    Ok(())
}
