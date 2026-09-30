// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The GUI's authoring surface: the picker palette, rule/wedge/glyph/sniper editors and the config
//! writes behind them all live in `neuron::authoring`, shared with the CLI so a bind written in one
//! is byte-identical to a bind written in the other. This module only re-exports it under the
//! `editor::` path the panels already use.

pub use neuron::authoring::*;

#[cfg(test)]
mod tests {
    /// `neuron config app` edits the GUI's `app.toml` by key; its key/type table must track the
    /// preferences the app actually persists.
    #[test]
    fn cli_pref_table_matches_prefs_field_for_field() {
        let defaults = toml::Value::try_from(crate::prefs::Prefs::default()).unwrap();
        let table = defaults.as_table().unwrap();
        for key in table.keys() {
            assert!(
                neuron::manage::app_pref_keys().contains(&key.as_str()),
                "prefs field `{key}` is missing from neuron::manage::APP_PREF_KINDS"
            );
        }
        for (key, kind) in neuron::manage::APP_PREF_KINDS {
            let v = table
                .get(*key)
                .unwrap_or_else(|| panic!("APP_PREF_KINDS lists `{key}` but Prefs has no such field"));
            let actual = match v {
                toml::Value::Boolean(_) => "bool",
                toml::Value::String(_) => "string",
                toml::Value::Integer(_) => "int",
                toml::Value::Float(_) => "float",
                toml::Value::Array(_) => "list",
                toml::Value::Table(_) => "table",
                toml::Value::Datetime(_) => "datetime",
            };
            assert_eq!(actual, *kind, "`{key}` is a {actual} in Prefs but listed as {kind}");
        }
    }
}
