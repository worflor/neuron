// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../LICENSE.md.

//! Compile the Slint UI into generated Rust at build time. `slint::include_modules!()` in
//! `src/ui.rs` pulls in the generated component types (`AppWindow`, all the shared structs and
//! callbacks). The software renderer keeps idle cost near zero — true to the anti-bloat motto.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The entry .slint file `include`s every panel; recompile when any of them changes.
    slint_build::compile("ui/app.slint")?;
    Ok(())
}
