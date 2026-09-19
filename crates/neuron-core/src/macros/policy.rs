// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Source-owned macro authority. No sidecar, UI preference, or sidecar file gets a second vote.

use serde::{Deserialize, Serialize};

pub const RAW_DIRECTIVE: &str = "# neuron: raw";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroMode {
    #[default]
    Bound,
    Raw,
}

impl MacroMode {
    pub fn label(self) -> &'static str {
        match self {
            MacroMode::Bound => "BOUND",
            MacroMode::Raw => "RAW",
        }
    }
}

/// Read the execution mode from the leading comment/header region. No directive means BOUND.
/// A misspelled Neuron directive is an error rather than a silent authority decision.
pub fn mode_from_source(source: &str) -> Result<MacroMode, String> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut raw = false;
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t == RAW_DIRECTIVE {
            if raw {
                return Err("duplicate '# neuron: raw' directive".into());
            }
            raw = true;
            continue;
        }
        if t.starts_with("# neuron:") {
            return Err(format!(
                "unknown macro directive '{t}' (expected '{RAW_DIRECTIVE}')"
            ));
        }
        if t.starts_with('#') {
            continue;
        }
        break;
    }
    Ok(if raw { MacroMode::Raw } else { MacroMode::Bound })
}

/// Make the source say exactly what the requested mode is. RAW is one canonical source line;
/// BOUND is its absence. Only the leading header directive is touched.
pub fn set_source_mode(source: &str, mode: MacroMode) -> String {
    let (bom, source) = match source.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", source),
    };
    let mut out = String::new();
    let mut header = true;
    for line in source.split_inclusive('\n') {
        let t = line.trim();
        if header && t == RAW_DIRECTIVE {
            continue;
        }
        if header && !t.is_empty() && !t.starts_with('#') {
            header = false;
        }
        out.push_str(line);
    }
    // split_inclusive misses nothing except the empty string, so a source without a final newline
    // is already preserved above.
    if mode == MacroMode::Raw {
        format!("{bom}{RAW_DIRECTIVE}\n{out}")
    } else {
        format!("{bom}{out}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_is_bound_and_one_header_line_is_raw() {
        assert_eq!(mode_from_source("def macro(ctx):\n    pass\n").unwrap(), MacroMode::Bound);
        assert_eq!(
            mode_from_source("# a note\n# neuron: raw\ndef macro(ctx):\n    pass\n").unwrap(),
            MacroMode::Raw
        );
        // A string/comment in the body is not compiler metadata.
        assert_eq!(
            mode_from_source("def macro(ctx):\n    x = '# neuron: raw'\n").unwrap(),
            MacroMode::Bound
        );
    }

    #[test]
    fn malformed_or_duplicate_directives_are_refused() {
        assert!(mode_from_source("# neuron: unbound\ndef macro(ctx):\n    pass\n").is_err());
        assert!(mode_from_source(
            "# neuron: raw\n# neuron: raw\ndef macro(ctx):\n    pass\n"
        ).is_err());
    }

    #[test]
    fn toggle_is_source_truth_and_round_trips() {
        let src = "# keep me\ndef macro(ctx):\n    pass\n";
        let raw = set_source_mode(src, MacroMode::Raw);
        assert!(raw.starts_with("# neuron: raw\n"));
        assert!(raw.contains("# keep me\n"));
        assert_eq!(mode_from_source(&raw).unwrap(), MacroMode::Raw);
        let bound = set_source_mode(&raw, MacroMode::Bound);
        assert_eq!(bound, src);
        assert_eq!(mode_from_source(&bound).unwrap(), MacroMode::Bound);
    }

    #[test]
    fn bom_survives_the_toggle() {
        let src = "\u{feff}def macro(ctx):\n    pass\n";
        let raw = set_source_mode(src, MacroMode::Raw);
        assert!(raw.starts_with("\u{feff}# neuron: raw\n"));
        assert_eq!(set_source_mode(&raw, MacroMode::Bound), src);
    }
}
