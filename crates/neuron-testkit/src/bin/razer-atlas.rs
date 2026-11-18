// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../../LICENSE.md.

//! Regenerate the Razer keyboard and mouse atlases from pinned upstream checkouts:
//!
//! ```text
//! cargo run -p neuron-testkit --bin razer-atlas -- OPENRAZER_DIR OPENRGB_DIR SIGNALRGB_DIR \
//!     > crates/neuron-core/protocol/razer-keyboards.toml
//! cargo run -p neuron-testkit --bin razer-atlas -- --mice OPENRAZER_DIR OPENRGB_DIR SIGNALRGB_DIR \
//!     > crates/neuron-core/protocol/razer-mice.toml
//! ```
//!
//! Each directory is a git checkout; its HEAD commit is pinned in the file's `[sources]`.

use neuron_testkit::atlas_extract::{build, build_mice, Checkouts};
use std::path::PathBuf;
use std::process::ExitCode;

fn head(dir: &PathBuf) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() -> ExitCode {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mice = args.first().is_some_and(|a| a == "--mice");
    if mice {
        args.remove(0);
    }
    let dirs: Vec<PathBuf> = args.into_iter().map(PathBuf::from).collect();
    let [openrazer, openrgb, signalrgb] = dirs.as_slice() else {
        eprintln!("usage: razer-atlas [--mice] OPENRAZER_DIR OPENRGB_DIR SIGNALRGB_DIR");
        return ExitCode::from(2);
    };
    let pin = |d: &PathBuf| head(d).unwrap_or_else(|| "unknown".into());
    let checkouts = Checkouts {
        openrazer: (openrazer.clone(), pin(openrazer)),
        openrgb: (openrgb.clone(), pin(openrgb)),
        signalrgb: (signalrgb.clone(), pin(signalrgb)),
    };
    match if mice { build_mice(&checkouts) } else { build(&checkouts) } {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("razer-atlas: {e}");
            ExitCode::FAILURE
        }
    }
}
