// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! End-to-end proof of the bidirectional macro model against the REAL python sidecar: the two-way
//! mapping between a macro's Python source and the typed [`MacroNode`] tree is ROUND-TRIP STABLE.
//!
//! For each representative source:  parse(src) -> nodes -> nodes_to_source(nodes) -> parse(...) ==
//! the first node tree. So the model loses nothing it understands and re-emits to the same tree;
//! anything it doesn't understand survives verbatim as a `raw` node (still re-parsed identically).
//! Also asserts a malformed source returns the `Syntax` error variant (never a panic).
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable — so CI without python is fine.
//! (The Rust codegen unit tests in `node.rs` are the python-free executable proof of `nodes_to_source`.)

use neuron::macros::{macro_host, nodes_to_source, MacroNode, ParseError, Value};

/// Shorthand for a string-literal Value.
fn s(v: &str) -> Value {
    Value::str(v)
}
/// Shorthand for a ctx-read Value.
fn ctx(field: &str) -> Value {
    Value::Ctx {
        field: field.into(),
    }
}

/// Isolate the macros dir into a private temp cwd, returning the host if the bundled runtime
/// materializes (it ships in the binary, so this should always succeed; None => skip on a rare IO
/// failure). Returns (prev_cwd, tmp_dir, run-dir pin) for cleanup alongside — the pin must outlive
/// the whole test, so it rides along in the tuple instead of dropping at the end of this fn.
fn setup() -> Option<(std::path::PathBuf, std::path::PathBuf, neuron::runroot::RunDirPin)> {
    let tmp = std::env::temp_dir().join(format!("neuron_macro_nodes_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let run_pin = neuron::runroot::RunDirPin::to(&tmp);

    if !macro_host().available() {
        eprintln!("skipping macro-nodes e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(&prev).ok();
        drop(run_pin);
        let _ = std::fs::remove_dir_all(&tmp);
        return None;
    }
    Some((prev, tmp, run_pin))
}

fn teardown(prev: std::path::PathBuf, tmp: std::path::PathBuf, run_pin: neuron::runroot::RunDirPin) {
    std::env::set_current_dir(prev).ok();
    drop(run_pin);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// parse(src) must succeed; assert the tree, then prove the FULL round-trip is stable:
/// nodes -> source -> parse == the same tree.
fn assert_round_trip(src: &str, expect: &[MacroNode]) {
    let host = macro_host();
    let nodes = host
        .parse_macro(src)
        .unwrap_or_else(|e| panic!("parse failed for {src:?}: {e}"));
    assert_eq!(nodes, expect, "parsed tree mismatch for source:\n{src}");

    let regen = nodes_to_source(&nodes);
    let reparsed = host
        .parse_macro(&regen)
        .unwrap_or_else(|e| panic!("re-parse failed for generated source:\n{regen}\nerr: {e}"));
    assert_eq!(
        reparsed, nodes,
        "round-trip not stable.\noriginal src:\n{src}\nregenerated:\n{regen}"
    );
}

#[test]
fn macro_nodes_round_trip_is_stable() {
    let Some((prev, tmp, run_pin)) = setup() else {
        return;
    };
    let host = macro_host();

    // 1) a plain sequence of modelled helpers (neuron.-prefixed).
    assert_round_trip(
        "def macro(ctx):\n    neuron.type_text(\"hello\")\n    neuron.hotkey(\"ctrl\", \"c\")\n    neuron.notify(\"done\")\n",
        &[
            MacroNode::Type { text: s("hello"), ghost: false, speed: None },
            MacroNode::Press { keys: vec!["ctrl".into(), "c".into()] },
            MacroNode::Notify { text: s("done") },
        ],
    );

    // 2) BARE calls (no `neuron.` prefix) must map to the same nodes — the helpers are injected as
    //    globals too, so both styles are valid macros.
    assert_round_trip(
        "def macro(ctx):\n    type_text(\"hi\")\n    run(\"notepad\")\n",
        &[
            MacroNode::Type { text: s("hi"), ghost: false, speed: None },
            MacroNode::Open { command: s("notepad"), capture: None },
        ],
    );

    // 3) an ask/else branch (recurses into both arms).
    assert_round_trip(
        "def macro(ctx):\n    if neuron.ask(\"go?\"):\n        neuron.type_text(\"yes\")\n    else:\n        neuron.notify(\"no\")\n",
        &[MacroNode::Ask {
            question: s("go?"),
            description: Value::empty_str(),
            yes: vec![MacroNode::Type { text: s("yes"), ghost: false, speed: None }],
            no: vec![MacroNode::Notify { text: s("no") }],
        }],
    );

    // 4) an ask with only a yes-branch (no else).
    assert_round_trip(
        "def macro(ctx):\n    if neuron.ask(\"q\"):\n        neuron.run(\"calc\")\n",
        &[MacroNode::Ask {
            question: s("q"),
            description: Value::empty_str(),
            yes: vec![MacroNode::Open { command: s("calc"), capture: None }],
            no: vec![],
        }],
    );

    // 5) a `raw` statement: an unmodelled call (clipboard_get) on the RHS of an assign whose value
    //    we can't reduce to a typed Value still survives — here it lands as SetVar{value: Raw}.
    assert_round_trip(
        "def macro(ctx):\n    x = ctx.selection\n",
        &[MacroNode::SetVar { name: "x".into(), value: ctx("selection") }],
    );

    // 6) a MIXED macro: a copy, a set-var whose RHS is a transform Call (the Value parser models
    //    `neuron.clipboard_get()` as a Call on the `neuron` Var — MORE powerful than Raw, and still
    //    round-trips), then an ask — proves typed nodes interleave and re-emit in place.
    assert_round_trip(
        "def macro(ctx):\n    neuron.clipboard_set(\"x\")\n    y = neuron.clipboard_get()\n    if neuron.ask(\"again?\"):\n        neuron.type_text(\"z\")\n",
        &[
            MacroNode::Copy { text: s("x") },
            MacroNode::SetVar {
                name: "y".into(),
                value: Value::Call {
                    recv: Box::new(Value::Var { name: "neuron".into() }),
                    method: "clipboard_get".into(),
                    args: vec![],
                },
            },
            MacroNode::Ask {
                question: s("again?"),
                description: Value::empty_str(),
                yes: vec![MacroNode::Type { text: s("z"), ghost: false, speed: None }],
                no: vec![],
            },
        ],
    );

    // 7) escaping: text with a double-quote and a backslash must round-trip exactly.
    assert_round_trip(
        "def macro(ctx):\n    neuron.type_text(\"say \\\"hi\\\" \\\\ bye\")\n",
        &[MacroNode::Type { text: Value::str("say \"hi\" \\ bye"), ghost: false, speed: None }],
    );

    // 8) no `def macro`: a half-written macro maps its WHOLE module body to nodes (still round-trips
    //    because nodes_to_source always wraps them in `def macro(ctx):`).
    assert_round_trip(
        "neuron.type_text(\"loose\")\nx = 1\n",
        &[
            MacroNode::Type { text: s("loose"), ghost: false, speed: None },
            MacroNode::SetVar { name: "x".into(), value: Value::Int { n: 1 } },
        ],
    );

    // ── VALUE-RICH macros: every parameter is an expression, not just a string ─────────────────

    // 9) type_text((ctx.selection).upper()) — a transform chain over a ctx read. The parser strips
    //    the redundant parens; the typed Call codegens to `ctx.selection.upper()`, which re-parses
    //    to the same Call. Proves a Value chain round-trips.
    assert_round_trip(
        "def macro(ctx):\n    neuron.type_text(ctx.selection.upper())\n",
        &[MacroNode::Type {
            text: Value::Call {
                recv: Box::new(ctx("selection")),
                method: "upper".into(),
                args: vec![],
            },
            ghost: false,
            speed: None,
        }],
    );

    // 10) notify("got " + ctx.app) — a binary expression Value.
    assert_round_trip(
        "def macro(ctx):\n    neuron.notify(\"got \" + ctx.app)\n",
        &[MacroNode::Notify {
            text: Value::Bin {
                op: "+".into(),
                left: Box::new(s("got ")),
                right: Box::new(ctx("app")),
            },
        }],
    );

    // ── the FULL flow vocabulary ────────────────────────────────────────────────────────────────

    // 11) a plain if/else over a comparison value.
    assert_round_trip(
        "def macro(ctx):\n    if ctx.app == \"code.exe\":\n        neuron.notify(\"vscode\")\n    else:\n        neuron.notify(\"other\")\n",
        &[MacroNode::If {
            cond: Value::Bin {
                op: "==".into(),
                left: Box::new(ctx("app")),
                right: Box::new(s("code.exe")),
            },
            then_: vec![MacroNode::Notify { text: s("vscode") }],
            else_: vec![MacroNode::Notify { text: s("other") }],
        }],
    );

    // 12) for _ in range(n) -> RepeatN.
    assert_round_trip(
        "def macro(ctx):\n    for _ in range(3):\n        neuron.key(\"down\")\n",
        &[MacroNode::RepeatN {
            count: Value::Int { n: 3 },
            body: vec![MacroNode::KeyPress { name: "down".into() }],
        }],
    );

    // 13) for line in x.splitlines() -> ForEach with a Call source.
    assert_round_trip(
        "def macro(ctx):\n    for line in ctx.selection.splitlines():\n        neuron.type_text(line)\n",
        &[MacroNode::ForEach {
            var: "line".into(),
            source: Value::Call {
                recv: Box::new(ctx("selection")),
                method: "splitlines".into(),
                args: vec![],
            },
            body: vec![MacroNode::Type {
                text: Value::Var { name: "line".into() },
                ghost: false,
                speed: None,
            }],
        }],
    );

    // 14) while True -> RepeatWhile.
    assert_round_trip(
        "def macro(ctx):\n    while True:\n        neuron.sleep(100)\n",
        &[MacroNode::RepeatWhile {
            cond: Value::Bool { b: true },
            body: vec![MacroNode::Wait { ms: Value::Int { n: 100 } }],
        }],
    );

    // 15) try/except -> Try.
    assert_round_trip(
        "def macro(ctx):\n    try:\n        neuron.run(\"risky\")\n    except Exception:\n        neuron.notify(\"failed\")\n",
        &[MacroNode::Try {
            body: vec![MacroNode::Open { command: s("risky"), capture: None }],
            except_: vec![MacroNode::Notify { text: s("failed") }],
        }],
    );

    // 16) bare return -> Stop, after a set var.
    assert_round_trip(
        "def macro(ctx):\n    n = 0\n    return\n",
        &[
            MacroNode::SetVar { name: "n".into(), value: Value::Int { n: 0 } },
            MacroNode::Stop,
        ],
    );

    // 17) run capture: `out = neuron.run(cmd, wait=True)` -> Open{capture}.
    assert_round_trip(
        "def macro(ctx):\n    out = neuron.run(\"git status\", wait=True)\n",
        &[MacroNode::Open { command: s("git status"), capture: Some("out".into()) }],
    );

    // 18) paste: hotkey("ctrl","v") -> the special Paste node (both directions).
    assert_round_trip(
        "def macro(ctx):\n    neuron.hotkey(\"ctrl\", \"v\")\n",
        &[MacroNode::Paste],
    );

    // 19) type_ghost with a speed -> Type{ghost}.
    assert_round_trip(
        "def macro(ctx):\n    neuron.type_ghost(\"typed\", \"fast\")\n",
        &[MacroNode::Type { text: s("typed"), ghost: true, speed: Some("fast".into()) }],
    );

    // 20) ask WITH a description= keyword.
    assert_round_trip(
        "def macro(ctx):\n    if neuron.ask(\"overwrite?\", description=\"3 files lost\"):\n        neuron.notify(\"ok\")\n",
        &[MacroNode::Ask {
            question: s("overwrite?"),
            description: s("3 files lost"),
            yes: vec![MacroNode::Notify { text: s("ok") }],
            no: vec![],
        }],
    );

    // 21) THE GNARLY STATEMENT: a `with` block guarding a comprehension with a walrus is nothing the
    //     model touches at the STATEMENT level (it's neither a modelled action, flow, nor assignment)
    //     — it MUST land as a single Raw node and survive BYTE-IDENTICAL through the full round-trip.
    //     (ast.unparse normalizes spacing; we assert the parsed Raw re-emits to the same Raw, which is
    //     the real guarantee — the verbatim code is preserved losslessly.)
    let gnarly = "def macro(ctx):\n    with open(\"f\") as fh:\n        rows = [r for r in fh if (n := len(r)) > 3]\n";
    let nodes = host.parse_macro(gnarly).expect("parse gnarly");
    assert_eq!(nodes.len(), 1, "the gnarly stmt is one node");
    let MacroNode::Raw { code } = &nodes[0] else {
        panic!("the gnarly with-block must fall to Raw, got {:?}", nodes[0]);
    };
    assert!(code.contains(":="), "the walrus survives in the raw code");
    assert!(code.contains("with open"), "the with-block survives in the raw code");
    assert!(code.contains("for r in"), "the comprehension survives in the raw code");
    // and it round-trips byte-identically (parse -> source -> parse == same Raw node).
    assert_round_trip(gnarly, &nodes);

    // 22) a deliberately complex helper call we DON'T model (a kwarg we don't recognise) falls to
    //     Raw and survives verbatim — proves an unmodelled CALL is preserved, not mangled.
    let raw_call = "def macro(ctx):\n    neuron.run(\"x\", wait=False, shell=True)\n";
    let rc = host.parse_macro(raw_call).expect("parse raw call");
    assert!(
        matches!(&rc[0], MacroNode::Raw { .. }),
        "an unmodelled keyword-call must be Raw, got {:?}", rc[0]
    );
    assert_round_trip(raw_call, &rc);

    // Whole-document parsing preserves the ACTUAL entry wrapper even when a hand-written entry
    // uses a one-line suite. A canvas edit may expand the body, but must not rename main->macro or
    // discard its signature/header metadata.
    let one_line = "# neuron: raw\n# keep-header\ndef main(ctx, n=3): return n\n";
    let one_doc = host.parse_document(one_line).expect("parse one-line document");
    assert_eq!(one_doc.mode, neuron::macros::MacroMode::Raw);
    let one_regen = neuron::macros::document_to_source(&one_doc);
    assert!(one_regen.starts_with("# neuron: raw\n# keep-header\n"));
    assert!(
        one_regen.contains("def main(ctx, n=3):\n    return n\n"),
        "one-line entry wrapper was canonicalized or lost: {one_regen}"
    );
    // 23) MALFORMED source -> the Syntax error variant, NOT a panic and NOT a Host error.
    match host.parse_macro("def macro(ctx):\n    if neuron.ask(\"q\"\n") {
        Err(ParseError::Syntax { line, msg }) => {
            assert!(!msg.is_empty(), "syntax error should carry a message");
            eprintln!("malformed source reported Syntax error at line {line}: {msg}");
        }
        other => panic!("malformed source must be a Syntax error, got {other:?}"),
    }

    // 24) a literal `pass` body is itself a statement, so it maps to a single raw node (NOT an
    //     empty list — that asymmetry is intended: empty nodes generate `pass`, but a hand-written
    //     `pass` is real source we preserve). It still round-trips.
    assert_round_trip(
        "def macro(ctx):\n    pass\n",
        &[MacroNode::Raw { code: "pass".into() }],
    );

    // 25) an empty node list generates a `pass`-bodied macro; parsing THAT gives the raw `pass`
    //     node, and from there the round-trip is fixed. Prove codegen of [] is valid + re-parsable.
    let regen_empty = nodes_to_source(&[]);
    assert_eq!(regen_empty, "def macro(ctx):\n    pass\n");
    let from_empty = host.parse_macro(&regen_empty).expect("parse empty-codegen");
    assert_eq!(from_empty, vec![MacroNode::Raw { code: "pass".into() }]);

    teardown(prev, tmp, run_pin);
}
