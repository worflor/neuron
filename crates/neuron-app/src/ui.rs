// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The generated Slint UI module. `slint::include_modules!()` pulls in the types produced by
//! `build.rs` (the `AppWindow` component plus every shared struct/global/callback). Re-exported
//! so the rest of the crate refers to them through `crate::ui::*`.

slint::include_modules!();
