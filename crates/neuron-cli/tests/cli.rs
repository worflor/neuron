// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! End-to-end tests of the CLI grammar: the real `neuron` binary, driven the way an agent would,
//! against a throwaway run root (`NEURON_RUN_DIR`). Only config verbs run here: nothing in this file
//! touches a device, arms input, or reads the maintainer's real config.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Root(PathBuf);

impl Root {
    fn new(tag: &str) -> Root {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("neuron_cli_it_{tag}_{}_{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Root(dir)
    }

    fn cmd(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_neuron"))
            .env("NEURON_RUN_DIR", &self.0)
            .arg("--no-live")
            .args(args)
            .output()
            .unwrap()
    }

    /// Run with `--json`, expect success, return the parsed document.
    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        let o = self.cmd(&a);
        assert!(o.status.success(), "neuron {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("neuron {args:?} printed invalid JSON ({e}): {}", String::from_utf8_lossy(&o.stdout)))
    }

    /// Run expecting failure; return its stderr.
    fn fails(&self, args: &[&str]) -> String {
        let o = self.cmd(args);
        assert!(!o.status.success(), "neuron {args:?} should have failed but printed: {}", String::from_utf8_lossy(&o.stdout));
        String::from_utf8_lossy(&o.stderr).into_owned()
    }

    fn text(&self, args: &[&str]) -> String {
        let o = self.cmd(args);
        assert!(o.status.success(), "neuron {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

#[test]
fn bind_lifecycle_add_replace_set_move_remove() {
    let r = Root::new("bind");
    let added = r.json(&["bind", "add", "--trigger", "mouse:4", "--action", "key:f5"]);
    assert_eq!(added["outcome"], "added");
    assert_eq!(added["index"], 0);
    assert_eq!(added["rule"]["trigger"]["usage"], 4);
    assert_eq!(added["rule"]["action"]["key"], "f5");

    r.json(&["bind", "add", "--trigger", "mouse:5", "--action", r#"{"type":"dpi-set","dpi":800}"#, "--hypershift"]);
    let again = r.json(&["bind", "add", "--trigger", "mouse:4", "--action", "key:g"]);
    assert_eq!(again["outcome"], "replaced", "the same trigger on the same layer re-binds");
    assert_eq!(again["rules"].as_array().unwrap().len(), 2);

    let set = r.json(&["bind", "set", "1", "--action", "dpi:1200", "--base"]);
    assert_eq!(set["rule"]["layer"], Value::Null);
    let listed = r.json(&["bind", "list"]);
    assert_eq!(listed["rules"][1]["action"]["dpi"], 1200);
    assert_eq!(r.json(&["bind", "list", "--layer", "hypershift"])["rules"].as_array().unwrap().len(), 0);

    r.json(&["bind", "mv", "1", "0"]);
    assert_eq!(r.json(&["bind", "list"])["rules"][0]["action"]["type"], "dpi-set");
    r.json(&["bind", "rm", "--trigger", "mouse:4"]);
    assert_eq!(r.json(&["bind", "list"])["rules"].as_array().unwrap().len(), 1);
    r.json(&["bind", "clear", "--base", "--yes"]);
    assert_eq!(r.json(&["bind", "list"])["rules"].as_array().unwrap().len(), 0);
    assert!(r.fails(&["bind", "clear", "--base"]).contains("--yes"));
}

#[test]
fn errors_are_actionable_json_on_stderr_with_a_nonzero_exit() {
    let r = Root::new("err");
    let o = r.cmd(&["bind", "add", "--trigger", "mouse:4", "--action", "key:no-such-key", "--json"]);
    assert!(!o.status.success());
    let err: Value = serde_json::from_slice(&o.stderr).expect("--json errors are JSON on stderr");
    assert!(err["error"].as_str().unwrap().contains("isn't a key"), "{err}");
    assert!(o.stdout.is_empty(), "nothing on stdout when the command failed");
    assert!(r.fails(&["bind", "add", "--trigger", "nonsense", "--action", "echo"]).contains("not a trigger"));
    assert!(r.fails(&["bind", "rm", "9"]).contains("no rule at index 9"));
    assert_eq!(r.json(&["bind", "list"])["rules"].as_array().unwrap().len(), 0, "a rejected write leaves the file untouched");
}

#[test]
fn missing_macros_are_errors_unless_explicitly_allowed() {
    let r = Root::new("refs");
    assert!(r.fails(&["bind", "add", "--trigger", "key:f13", "--action", "macro:ghost"]).contains("no macro 'ghost'"));
    let ok = r.json(&["bind", "add", "--trigger", "key:f13", "--action", "macro:ghost", "--allow-missing-refs"]);
    assert_eq!(ok["outcome"], "added");
    assert_eq!(ok["warnings"].as_array().unwrap().len(), 1);
    // once the macro exists there is no warning
    r.text(&["macro", "add", "ghost", "--source", "def macro(ctx):\n    return 1\n", "--no-check"]);
    let fixed = r.json(&["bind", "add", "--trigger", "key:f13", "--action", "macro:ghost"]);
    assert_eq!(fixed["warnings"].as_array().unwrap().len(), 0);
    // deleting a macro reports the bind that still names it
    let rm = r.json(&["macro", "rm", "ghost"]);
    assert_eq!(rm["still_referenced_by"][0], "gui[0]");
}

#[test]
fn every_action_variant_is_expressible_from_the_command_line() {
    let r = Root::new("actions");
    let list = r.json(&["action", "list"]);
    let types = list["types"].as_array().unwrap();
    assert!(types.len() >= 39, "the catalog lists every Action variant");
    for t in types {
        let spec = serde_json::to_string(&t["example"]).unwrap();
        let out = r.json(&["action", "check", &spec, "--allow-missing-refs"]);
        assert_eq!(out["action"], t["example"], "{} survives a round trip through the CLI", t["type"]);
        // ...and can be bound
        r.json(&["bind", "add", "--trigger", "key:f14", "--action", &spec, "--allow-missing-refs"]);
    }
    // the TOML-inline form the config files use
    let toml_form = r.json(&["action", "check", r#"{type="key", key="F5"}"#]);
    assert_eq!(toml_form["action"]["key"], "F5");
    assert!(toml_form["toml"].as_str().unwrap().contains("type = \"key\""));
}

#[test]
fn triggers_of_every_kind_resolve() {
    let r = Root::new("triggers");
    for (spec, kind) in [
        ("mouse:4", "input"),
        ("key:f13", "input"),
        ("macro:M3", "input"),
        ("input:0x0c/0xe9", "input"),
        ("label:Left Ctrl", "input"),
        ("gesture:circle", "gesture"),
        ("radial:2", "radial-sector"),
        ("app:valorant", "app-focus"),
        ("mic-tap", "mic-tap"),
        ("hold:hypershift", "hold"),
        ("cast:2", "cast"),
        ("game-light:overwatch/ult", "game-light"),
    ] {
        assert_eq!(r.json(&["trigger", "check", spec])["trigger"]["kind"], kind, "{spec}");
    }
    assert!(r.fails(&["trigger", "check", "cast:0"]).contains("N taps"));
}

#[test]
fn profiles_carry_settings_lighting_and_their_own_binds() {
    let r = Root::new("profile");
    r.json(&["profile", "new", "game"]);
    r.json(&["profile", "set", "game", "dpi=1600", "polling=500", "disable-win=true"]);
    r.json(&["light", "stack", "add", "--profile", "game", "--preset", "fire", "--blend", "add"]);
    r.json(&["light", "stack", "add", "--profile", "game", "--pattern", "uniform", "--color", "ff8800", "--disable"]);
    r.json(&["bind", "add", "--profile", "game", "--trigger", "mic-tap", "--action", "mute:mic"]);
    let shown = r.json(&["profile", "show", "game"]);
    assert_eq!(shown["profile"]["dpi"], 1600);
    assert_eq!(shown["profile"]["lighting"].as_array().unwrap().len(), 2);
    assert_eq!(shown["binds"].as_array().unwrap().len(), 1);

    let stack = r.json(&["light", "stack", "list", "--profile", "game"]);
    assert_eq!(stack["layers"][0]["pattern"], "heat");
    assert_eq!(stack["layers"][1]["enabled"], false);
    r.json(&["light", "stack", "mv", "0", "1", "--profile", "game"]);
    assert_eq!(r.json(&["light", "stack", "list", "--profile", "game"])["layers"][1]["pattern"], "heat");
    assert!(r.fails(&["light", "stack", "add", "--profile", "game", "--pattern", "uniform", "--param", "nope=1"]).contains("no knob"));

    // routes, rename retargeting, export/import
    r.json(&["profile", "route", "add", "valorant", "game"]);
    let renamed = r.json(&["profile", "rename", "game", "fps"]);
    assert_eq!(renamed["routes_followed"], 1);
    assert_eq!(r.json(&["profile", "route", "list"])["routes"][0]["profile"], "fps");
    let exported = r.text(&["profile", "export", "fps"]);
    let file = write(&r.0, "fps.toml", &exported);
    assert!(r.fails(&["profile", "import", &file]).contains("already exists"));
    r.json(&["profile", "import", &file, "--replace"]);
    let del = r.json(&["profile", "delete", "fps", "--yes"]);
    assert_eq!(del["report"]["binds_removed"], 1);
    assert_eq!(del["report"]["dangling_routes"][0], "valorant");
    assert!(r.fails(&["profile", "delete", "fps"]).contains("no profile"));
}

#[test]
fn cast_wheel_rhythms_and_glyph_binds() {
    let r = Root::new("cast");
    r.json(&["cast", "set", "--sectors", "6", "--assist", "0.3", "--activation", "hold"]);
    r.json(&["cast", "wedge", "set", "0", "--action", "key:1"]);
    r.json(&["cast", "wedge", "set", "1", "--action", "curtain", "--hyper"]);
    r.json(&["cast", "rhythm", "set", "2", "--action", "whiteboard"]);
    let show = r.json(&["cast", "show"]);
    assert_eq!(show["sectors"], 6);
    assert_eq!(show["wedges"][0]["action"]["key"], "1");
    assert_eq!(show["hyper_wedges"][1]["action"]["type"], "curtain");
    assert!(show["rhythms"].as_array().unwrap().iter().any(|x| x["taps"] == 2));
    assert!(r.fails(&["cast", "wedge", "set", "9", "--action", "echo"]).contains("beyond the wheel"));
    assert!(r.fails(&["cast", "set", "--sectors", "2"]).contains("sectors must be"));
    assert!(r.fails(&["cast", "glyph", "bind", "swirl", "--action", "echo"]).contains("no recorded glyph"));
    r.json(&["cast", "rhythm", "rm", "2"]);
    assert!(r.fails(&["cast", "rhythm", "rm", "2"]).contains("is bound"));
}

#[test]
fn feel_config_and_preferences_validate() {
    let r = Root::new("feel");
    let f = r.json(&["feel", "timing", "--hold-ms", "240", "--hypershift", "smart"]);
    assert_eq!(f["hold_ms"], 240);
    assert_eq!(f["hypershift"], "smart");
    assert!(r.fails(&["feel", "timing", "--hold-ms", "0"]).contains("1-5000"));
    assert!(r.fails(&["feel", "timing", "--hypershift", "toggle"]).contains("stance"));
    r.json(&["sniper", "--trigger", "mouse:5", "--dpi", "400"]);
    assert_eq!(r.json(&["feel", "show"])["sniper"]["dpi"], 400);
    r.json(&["sniper", "--unbind"]);

    let set = r.json(&["config", "app", "set", "notif_volume", "0.4"]);
    assert_eq!(set["value"], 0.4);
    assert!(r.fails(&["config", "app", "set", "notif_volume", "loud"]).contains("number"));
    assert!(r.fails(&["config", "app", "set", "host_obs_password", "x"]).contains("secret"));
    assert!(r.fails(&["config", "app", "get", "host_obs_password"]).contains("secret"));
    assert_eq!(r.json(&["config", "app", "get", "notif_volume"])["value"], 0.4);
}

#[test]
fn macros_are_stored_shown_and_deleted_without_a_runtime() {
    let r = Root::new("macro");
    let src = "# neuron: raw\ndef macro(ctx):\n    return 1\n";
    let file = write(&r.0, "m.py", src);
    let added = r.json(&["macro", "add", "m", &file, "--no-check"]);
    assert_eq!(added["mode"], "raw");
    assert_eq!(r.json(&["macro", "show", "m"])["source"], src);
    assert_eq!(r.json(&["macro", "list"])["macros"][0]["name"], "m");
    assert!(r.fails(&["macro", "add", "bad name", &file, "--no-check"]).contains("ASCII"));
    r.json(&["macro", "rm", "m"]);
    assert!(r.fails(&["macro", "show", "m"]).contains("no macro"));
}

#[test]
fn dump_then_apply_reproduces_the_whole_setup_and_is_idempotent() {
    let a = Root::new("dump_a");
    a.json(&["bind", "add", "--trigger", "mouse:4", "--action", "key:f5"]);
    a.json(&["bind", "add", "--trigger", "mouse:5", "--action", "keys:w:180 ~90 a", "--hypershift"]);
    a.json(&["bind", "hold", "set", "--trigger", "mouse:5"]);
    a.json(&["cast", "wedge", "set", "0", "--action", "key:1"]);
    a.json(&["feel", "timing", "--gap-ms", "300"]);
    a.json(&["profile", "new", "game"]);
    a.json(&["profile", "set", "game", "dpi=800"]);
    a.json(&["light", "stack", "add", "--profile", "game", "--preset", "aurora"]);
    a.json(&["bind", "add", "--profile", "game", "--trigger", "mic-tap", "--action", "echo"]);
    a.json(&["profile", "route", "add", "valorant", "game"]);
    a.json(&["config", "app", "set", "phoenix", "false"]);
    a.text(&["macro", "add", "hello", "--source", "def macro(ctx):\n    return 1\n", "--no-check"]);
    a.json(&["macro", "options", "hello", "--set", r#"{"volume": 3}"#]);

    let doc = a.text(&["dump"]);
    assert!(doc.contains("[macro_options.hello]"), "chosen macro options are part of the setup:\n{doc}");
    let file = write(&a.0, "setup.toml", &doc);

    let b = Root::new("dump_b");
    let dry = b.json(&["apply", &file, "--dry-run"]);
    assert_eq!(dry["applied"], false);
    assert!(b.json(&["profile", "list"])["profiles"].as_array().unwrap().is_empty(), "a dry run writes nothing");

    let done = b.json(&["apply", &file]);
    assert_eq!(done["applied"], true);
    assert_eq!(b.text(&["dump"]), doc, "the applied setup dumps back byte for byte");
    let again = b.json(&["apply", &file]);
    assert!(again["sections"].as_array().unwrap().iter().all(|s| s["status"] == "unchanged"), "a second apply changes nothing: {again}");

    // the JSON form of the same document applies too
    let json_doc = a.json(&["dump"]);
    let jfile = write(&a.0, "setup.json", &serde_json::to_string(&json_doc).unwrap());
    let c = Root::new("dump_c");
    c.json(&["apply", &jfile]);
    assert_eq!(c.text(&["dump"]), doc);

    // a broken document is refused whole
    let bad = write(&a.0, "bad.toml", "format = 1\n\n[feel]\nhold_ms = 0\ngap_ms = 280\ncoyote_ms = 120\nhypershift = \"hold\"\n");
    let d = Root::new("dump_d");
    assert!(d.fails(&["apply", &bad]).contains("nothing was written"));
}

#[test]
fn the_catalog_lets_an_agent_discover_the_surface() {
    let r = Root::new("catalog");
    let c = r.json(&["catalog"]);
    assert!(c["actions"]["types"].as_array().unwrap().len() >= 39);
    assert!(c["lighting"]["presets"].as_array().unwrap().iter().any(|p| p["slug"] == "fire"));
    assert!(c["emblems"].as_array().unwrap().iter().any(|e| e == "mouse"));
    assert!(c["app_preferences"].as_array().unwrap().iter().any(|p| p["key"] == "notif_volume"));
    let controls = r.json(&["control", "catalog", "--page", "keyboard"]);
    assert!(controls["controls"].as_array().unwrap().iter().any(|c| c["name"] == "F13"));
    assert_eq!(r.json(&["control", "name", "9", "4"])["name"], "Mouse 4 (thumb 1)");
    let status = r.json(&["status"]);
    assert_eq!(status["counts"]["binds"], 0);
    assert!(r.json(&["config", "path"])["files"].as_array().unwrap().len() >= 8);
}

#[test]
fn the_retired_remap_verb_no_longer_pretends_to_write_hardware() {
    let r = Root::new("remap");
    assert!(r.fails(&["remap", "--key", "=", "--to", "g", "--reset"]).contains("button restore"));
    assert!(r.fails(&["remap", "--key", "="]).contains("--to"));
    // the honest path: a bind plus the (hardware-free) plan
    let plan = r.json(&["button", "plan"]);
    assert!(plan["devices"].is_array());
}

#[test]
fn live_sync_hands_a_reload_to_the_app_and_never_waits_when_there_is_none() {
    // Without --no-live, an edit signals the app; here no app watches this run root, so the write
    // still succeeds and reports `no-app`, or `other-root` when an app runs on a different root
    // (and leaves no queue file behind).
    let r = Root::new("live");
    let o = Command::new(env!("CARGO_BIN_EXE_neuron"))
        .env("NEURON_RUN_DIR", &r.0)
        .args(["bind", "add", "--trigger", "mouse:4", "--action", "echo", "--json"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["live"] == "no-app" || v["live"] == "other-root" || v["live"] == "reloaded", "{v}");
    assert!(!r.0.join("live.queue").exists(), "an unclaimed queue is withdrawn");
}

#[test]
fn a_missing_macro_is_one_clean_error_and_a_scratch_root_never_signals_the_app() {
    let r = Root::new("macro_missing");
    let o = r.cmd(&["macro", "run", "nope", "--json"]);
    assert!(!o.status.success());
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("no macro named 'nope'"), "{err}");
    assert!(!String::from_utf8_lossy(&o.stdout).contains("macro host"));
    // a pinned run root never signals an app it cannot show is its own
    let st = r.json(&["status"]);
    assert_ne!(st["app_on_this_root"], true);
}

#[test]
fn usage_errors_exit_2_in_plain_words_and_json() {
    let r = Root::new("usage");
    let o = r.cmd(&["dpi", "abc", "--json"]);
    assert_eq!(o.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert!(err["error"].as_str().unwrap().contains("expected a whole number"), "{err}");
}
