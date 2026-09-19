// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! NON-DESTRUCTIVE stress tests for the macro PARSER + codegen — the two-way bridge between a macro's
//! Python source and the typed [`MacroNode`] tree (`parse_macro` in the warm `CPython` sidecar; the
//! Rust inverse [`nodes_to_source`]). Covers the audit gaps: deep expression/flow nesting, Unicode in
//! Values, extreme-size sources, the full Python operator set, malformed-input recovery, Raw-node
//! preservation, idempotent round-trips, concurrent + high-volume parsing, and summarize coverage.
//!
//! STRICTLY NON-DESTRUCTIVE: parse-only / codegen-only. No macro is ever EXECUTED, the sidecar runs
//! with input DISARMED, and the macros dir is isolated to a private temp cwd and cleaned up. The
//! pure-Rust phases (codegen + summarize) need no sidecar and run unconditionally; the round-trip /
//! parse-load phases need the bundled `CPython` and skip cleanly if it can't materialize.

use neuron::macros::macro_host::FIRE_BUDGET;
use neuron::macros::{
    macro_host, nodes_to_source, summarize, value_to_source, MacroHost, MacroNode, ParseError, Value,
};
use std::time::{Duration, Instant};

// ── tiny Value/Node builders ─────────────────────────────────────────────────────────────────────
fn s(v: &str) -> Value {
    Value::str(v)
}
fn var(n: &str) -> Value {
    Value::Var { name: n.into() }
}
fn bin(op: &str, l: Value, r: Value) -> Value {
    Value::Bin { op: op.into(), left: Box::new(l), right: Box::new(r) }
}
fn notify(v: Value) -> MacroNode {
    MacroNode::Notify { text: v }
}
fn leaf() -> Vec<MacroNode> {
    vec![notify(s("x"))]
}

/// The full Python operator vocabulary the [`Value::Bin`] codegen claims to support.
const ARITH: &[&str] = &["+", "-", "*", "/", "//", "%", "**"];
const CMP: &[&str] = &["==", "!=", "<", ">", "<=", ">=", "in", "not in", "is", "is not"];
const BOOL: &[&str] = &["and", "or"];

// ════════════════════════════════════════════════════════════════════════════════════════════════
// PURE-RUST PHASES (no sidecar) — codegen + the plain-English summarizer.
// ════════════════════════════════════════════════════════════════════════════════════════════════

/// `all_python_operators` (codegen half): every valid Python operator codegens to its parenthesised
/// `(a op b)` form. (The parse/round-trip half is in `parser_stress`, which needs the sidecar.)
#[test]
fn all_python_operators_codegen() {
    for &op in ARITH.iter().chain(CMP).chain(BOOL) {
        let v = bin(op, var("a"), var("b"));
        assert_eq!(
            value_to_source(&v),
            format!("(a {op} b)"),
            "operator {op:?} must codegen parenthesised + unambiguous"
        );
    }
    // nested precedence stays unambiguous: ((a + b) * c) and ((a == b) and (c == d)).
    assert_eq!(
        value_to_source(&bin("*", bin("+", var("a"), var("b")), var("c"))),
        "((a + b) * c)"
    );
    assert_eq!(
        value_to_source(&bin("and", bin("==", var("a"), var("b")), bin("==", var("c"), var("d")))),
        "((a == b) and (c == d))"
    );
    eprintln!("[parser pure] all_python_operators_codegen OK ({} ops)", ARITH.len() + CMP.len() + BOOL.len());
}

/// `deep_nested_expressions` (codegen half): the RECURSIVE Rust codegen does not stack-overflow on a
/// deeply nested Value, and emits a fully-parenthesised, balanced expression.
#[test]
fn deep_nested_expression_codegen_is_safe() {
    for depth in [50usize, 100, 200, 400] {
        let v = nested_bin(depth);
        let src = value_to_source(&v);
        // every Bin adds one '(' and one ')': balanced + present at the claimed depth.
        assert_eq!(src.matches('(').count(), depth, "depth {depth}: open parens");
        assert_eq!(src.matches(')').count(), depth, "depth {depth}: close parens");
    }
    // a deeply nested call chain also codegens without overflow.
    let _ = value_to_source(&nested_call(400));
    eprintln!("[parser pure] deep_nested_expression_codegen_is_safe OK");
}

/// `summarize_step_label_coverage`: EVERY `MacroNode` arm produces a non-empty, human label (no arm
/// falls through to nothing), including empty/minimal Values.
#[test]
fn summarize_step_label_coverage() {
    let every_arm: Vec<MacroNode> = vec![
        MacroNode::Type { text: s("hi"), ghost: false, speed: None },
        MacroNode::Type { text: s(""), ghost: true, speed: Some("fast".into()) },
        MacroNode::Press { keys: vec!["ctrl".into(), "c".into()] },
        MacroNode::Press { keys: vec![] }, // empty chord still labels
        MacroNode::KeyPress { name: "enter".into() },
        MacroNode::Click { button: "left".into() },
        MacroNode::Scroll { amount: Value::Int { n: 0 } },
        MacroNode::MoveTo { x: Value::Int { n: 0 }, y: Value::Int { n: 0 } },
        MacroNode::Copy { text: Value::empty_str() },
        MacroNode::Paste,
        MacroNode::Open { command: Value::empty_str(), capture: None },
        MacroNode::Focus { window: Value::empty_str() },
        MacroNode::Wait { ms: Value::Int { n: 0 } },
        MacroNode::Notify { text: Value::empty_str() },
        MacroNode::Ask { question: Value::empty_str(), description: Value::empty_str(), yes: vec![], no: vec![] },
        MacroNode::If { cond: var("a"), then_: vec![], else_: vec![] },
        MacroNode::RepeatN { count: Value::Int { n: 1 }, body: vec![] },
        MacroNode::RepeatWhile { cond: Value::Bool { b: true }, body: vec![] },
        MacroNode::ForEach { var: "x".into(), source: var("xs"), body: vec![] },
        MacroNode::SetVar { name: "n".into(), value: Value::Int { n: 0 } },
        MacroNode::Stop,
        MacroNode::Try { body: vec![], except_: vec![] },
        MacroNode::Raw { code: "import os".into() },
    ];
    for node in &every_arm {
        let label = summarize(std::slice::from_ref(node));
        assert!(!label.is_empty(), "arm {node:?} produced an empty label");
        assert_ne!(label, "does nothing yet", "a real node must not read as empty: {node:?}");
    }
    // and the empty-program label is the dedicated one.
    assert_eq!(summarize(&[]), "does nothing yet");
    eprintln!("[parser pure] summarize_step_label_coverage OK ({} arms)", every_arm.len());
}

/// `summarize_unicode_truncation`: summarize handles emoji / RTL / combining marks in Values without
/// panicking, stays BOUNDED at the MAX=24 char cap (code-point based), always yields valid UTF-8, and
/// appends the ellipsis on overflow.
///
/// NOTE: truncation is by CODE POINT (`chars()`), not grapheme cluster — so a base+combining-mark
/// pair CAN be split at the boundary. That is a known cosmetic limitation, not a correctness bug:
/// the result is always valid UTF-8 and bounded. We assert the real guarantees, not grapheme-safety.
#[test]
fn summarize_unicode_truncation() {
    let cases = [
        "\u{1F600}".repeat(40),                  // 😀 ×40 (emoji)
        "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629} ".repeat(8), // Arabic (RTL)
        "a\u{0301}".repeat(40),                  // a + combining acute ×40
        "\u{200B}".repeat(60),                   // zero-width space ×60
    ];
    for raw in &cases {
        // Open's label shows its command Value -> exercises short_value's truncation path.
        let line = summarize(&[MacroNode::Open { command: Value::str(raw), capture: None }]);
        assert!(line.starts_with("runs "), "label keeps its verb: {line:?}");
        assert!(std::str::from_utf8(line.as_bytes()).is_ok(), "summary stays valid UTF-8");
        // long values get cut with the ellipsis and the whole line stays bounded (not 40+ chars).
        assert!(line.ends_with('\u{2026}'), "an overflowing value gets the ellipsis: {line:?}");
        assert!(line.chars().count() < 40, "the line is bounded, not the full payload: {line:?}");
    }
    // Ask quotes a unicode question without panicking.
    let q = "\u{062D}\u{0630}\u{0641}\u{061F} ".repeat(10);
    let asked = summarize(&[MacroNode::Ask {
        question: Value::str(&q),
        description: Value::empty_str(),
        yes: vec![],
        no: vec![],
    }]);
    assert!(asked.starts_with("asks:"), "{asked:?}");
    assert!(std::str::from_utf8(asked.as_bytes()).is_ok());
    eprintln!("[parser pure] summarize_unicode_truncation OK");
}

// ════════════════════════════════════════════════════════════════════════════════════════════════
// SIDECAR PHASES (need the bundled CPython) — every round-trip, parse-stress, and recovery case.
// ════════════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn parser_stress() {
    let Some((prev, tmp, run_pin)) = setup() else {
        return; // no python runtime -> skip cleanly (not a failure)
    };
    let result = std::panic::catch_unwind(|| run_sidecar_phases(macro_host()));
    std::env::set_current_dir(&prev).ok();
    drop(run_pin);
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_sidecar_phases(host: &MacroHost) {
    // ── deep_nested_expressions: 25/50/100 round-trip exactly; an extreme depth degrades gracefully.
    for depth in [25usize, 50, 100] {
        rt(host, &[notify(nested_bin(depth))]);
        rt(host, &[MacroNode::SetVar { name: "r".into(), value: nested_call(depth) }]);
    }
    // very deep: must NOT panic on the Rust side and the sidecar must SURVIVE (Ok-and-correct, or a
    // clean Syntax/Host error at the interpreter's recursion ceiling — never a hang or a Rust panic).
    {
        let nodes = vec![notify(nested_bin(300))];
        let src = nodes_to_source(&nodes);
        match host.parse_macro(&src) {
            Ok(p) => assert_eq!(p, nodes, "if a deep expr parses, it must parse CORRECTLY"),
            Err(e) => eprintln!("[parser] depth-300 expr degraded cleanly: {e}"),
        }
        let after = host.parse_macro("def macro(ctx):\n    neuron.notify(\"ok\")\n");
        assert!(after.is_ok(), "sidecar must survive a deep-expr parse (auto-respawn ok): {after:?}");
    }
    eprintln!("[parser] deep_nested_expressions OK");

    // ── unicode_stress_values: diverse Unicode survives a round-trip byte-for-byte in Str + Raw. ──
    {
        let unis = [
            "\u{1F600}\u{1F4A9}\u{1F680}",           // emoji
            "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}", // Arabic (RTL)
            "e\u{0301}a\u{0300}o\u{0308}",          // combining marks
            "zero\u{200B}width\u{FEFF}joiner\u{200D}", // zero-width / BOM / ZWJ
            "\u{4F60}\u{597D}\u{4E16}\u{754C}",     // CJK
            "tab\tnewline\nquote\"back\\slash",     // escapes alongside unicode
        ];
        for u in unis {
            rt(host, &[notify(s(u))]);
            rt(host, &[MacroNode::Copy { text: s(u) }]);
            rt(host, &[MacroNode::SetVar { name: "v".into(), value: s(u) }]);
        }
        // unicode inside a Raw node survives too.
        let raw_src = "def macro(ctx):\n    data = {\"k\\u00e9y\": \"\u{1F4BE}\"}\n";
        let nodes = host.parse_macro(raw_src).expect("parse unicode raw");
        rt(host, &nodes);
        eprintln!("[parser] unicode_stress_values OK");
    }

    // ── extreme_long_values: 10KB / 100KB / 1MB literals, a 100-arg call, a 5000-statement body. ──
    {
        for size in [10_000usize, 100_000, 1_000_000] {
            let big = "x".repeat(size);
            let nodes = vec![notify(s(&big))];
            let t = Instant::now();
            rt(host, &nodes);
            let el = t.elapsed();
            assert!(el < FIRE_BUDGET, "a {size}-byte literal round-trip blew the parse budget ({el:?})");
            eprintln!("[parser]   {size}-byte literal round-trip in {el:?}");
        }
        // a method call with 100 positional args.
        let args: Vec<Value> = (0..100).map(|i| s(&format!("a{i}"))).collect();
        let call = Value::Call { recv: Box::new(var("x")), method: "fmt".into(), args };
        rt(host, &[MacroNode::SetVar { name: "r".into(), value: call }]);
        // a macro body with 5000 top-level statements (bounded by the 2.5s parse budget).
        let many: Vec<MacroNode> = (0..5000).map(|i| notify(s(&format!("n{i}")))).collect();
        let t = Instant::now();
        rt(host, &many);
        eprintln!("[parser] extreme_long_values OK (5000-stmt body in {:?})", t.elapsed());
    }

    // ── all_python_operators: every operator round-trips (codegen -> parse -> same Bin). ──────────
    {
        for &op in ARITH.iter().chain(CMP).chain(BOOL) {
            rt(host, &[notify(bin(op, var("a"), var("b")))]);
        }
        // nested mixed-operator expressions round-trip with precedence preserved by parens.
        rt(host, &[notify(bin("*", bin("+", var("a"), var("b")), var("c")))]);
        rt(host, &[notify(bin("or", bin("and", var("a"), var("b")), bin("==", var("c"), var("d"))))]);
        eprintln!("[parser] all_python_operators OK");
    }

    // ── deeply_nested_flows: 12 levels of for/if/try/foreach/while nest correctly + round-trip. ───
    {
        let depth = 12;
        let nodes = nest_flows(depth);
        rt(host, &nodes);
        // round-trip already proves indentation is exact (else it wouldn't re-parse to the same tree);
        // additionally confirm the innermost statement reached the deep indent level.
        let src = nodes_to_source(&nodes);
        let deep_indent = " ".repeat(4 * (depth + 1));
        assert!(
            src.lines().any(|l| l.starts_with(&deep_indent)),
            "the deepest body must be indented {} spaces",
            4 * (depth + 1)
        );
        eprintln!("[parser] deeply_nested_flows OK ({depth} levels)");
    }

    // ── malformed_syntax_at_all_positions: each malformed source -> Syntax error, never a panic;
    //     the sidecar stays warm (a valid parse works right after each). ────────────────────────────
    {
        let malformed = [
            "def macro(ctx):\n    neuron.notify(\"unclosed\n",         // unterminated string/paren
            "def macro(ctx):\n    if neuron.ask(\"q\"\n",            // missing close paren
            "def macro(ctx):\n    for x in\n",                       // incomplete for
            "def macro(ctx):\n    while\n",                          // incomplete while
            "def macro(ctx):\n    try:\n        pass\n",            // try with no except (Py syntax err)
            "def macro(ctx):\n    return return\n",                  // invalid keyword position
            "def macro(ctx):\n        neuron.notify(\"x\")\n    neuron.notify(\"y\")\n", // bad indent
            "def macro(ctx):\n    x = (1 + )\n",                      // dangling operator
        ];
        for src in malformed {
            match host.parse_macro(src) {
                Err(ParseError::Syntax { msg, .. }) => {
                    assert!(!msg.is_empty(), "a syntax error must carry a message for: {src:?}");
                }
                other => panic!("malformed source must be ParseError::Syntax, got {other:?}\nsrc:{src:?}"),
            }
            // the sidecar must keep serving — a syntax error is data, not a crash.
            let ok = host.parse_macro("def macro(ctx):\n    neuron.notify(\"alive\")\n");
            assert!(ok.is_ok(), "sidecar must stay warm after a syntax error: {ok:?}");
        }
        eprintln!("[parser] malformed_syntax_at_all_positions OK ({} cases)", malformed.len());
    }

    // ── raw_node_preservation_pathological: unmodelled statements survive verbatim + idempotently. ─
    {
        let pathological = [
            // a with-block guarding a walrus comprehension
            "def macro(ctx):\n    with open(\"f\") as fh:\n        rows = [r for r in fh if (n := len(r)) > 3]\n",
            // a decorated nested function def
            "def macro(ctx):\n    import functools\n    @functools.lru_cache\n    def inner(x):\n        return x * 2\n    return inner(3)\n",
            // a context manager + f-string + ternary
            "def macro(ctx):\n    label = f\"{ctx.app or 'none'}!\" if ctx.app else \"empty\"\n    neuron.notify(label)\n",
            // an async-style construct (still just Raw)
            "def macro(ctx):\n    lst = [x async for x in agen()]\n",
            // deeply-indented hand-written code
            "def macro(ctx):\n    for a in range(2):\n        for b in range(2):\n            d = {a: [b, a + b]}\n",
        ];
        for src in pathological {
            // parse(codegen(parse(x))) == parse(x), three times => a stable fixed point.
            let p0 = host.parse_macro(src).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
            let mut prev = p0.clone();
            for cycle in 0..3 {
                let regen = nodes_to_source(&prev);
                let again = host
                    .parse_macro(&regen)
                    .unwrap_or_else(|e| panic!("re-parse cycle {cycle} of {src:?}: {e}"));
                assert_eq!(again, prev, "round-trip not idempotent (cycle {cycle}) for {src:?}");
                prev = again;
            }
        }
        eprintln!("[parser] raw_node_preservation_pathological OK ({} cases)", pathological.len());
    }

    // ── concurrent_parsing: 16 threads × 100 parses of the same macro all agree, no races/panics. ─
    {
        let src =
            "def macro(ctx):\n    neuron.type_text(ctx.selection.upper())\n    if ctx.app == \"x.exe\":\n        neuron.notify(\"hi\")\n";
        let expected = host.parse_macro(src).expect("baseline parse");
        let mut handles = Vec::new();
        for _ in 0..16 {
            let exp = expected.clone();
            let src = src.to_string();
            handles.push(std::thread::spawn(move || {
                let h = macro_host();
                for _ in 0..100 {
                    let got = h.parse_macro(&src).expect("concurrent parse must succeed");
                    assert_eq!(got, exp, "a concurrent parse produced a different tree");
                }
            }));
        }
        for h in handles {
            h.join().expect("a parser thread panicked");
        }
        eprintln!("[parser] concurrent_parsing OK (16×100)");
    }

    // ── parse_codegen_parse_idempotence: ~100 varied macros reach a byte-identical codegen fixed
    //     point and a stable parse. ──────────────────────────────────────────────────────────────
    {
        let corpus = idempotence_corpus();
        assert!(corpus.len() >= 100, "need >=100 fixtures, have {}", corpus.len());
        for (i, nodes) in corpus.iter().enumerate() {
            let s0 = nodes_to_source(nodes);
            let p0 = host
                .parse_macro(&s0)
                .unwrap_or_else(|e| panic!("fixture {i} parse: {e}\nsrc:\n{s0}"));
            let s1 = nodes_to_source(&p0);
            let p1 = host.parse_macro(&s1).unwrap_or_else(|e| panic!("fixture {i} re-parse: {e}"));
            assert_eq!(p0, p1, "fixture {i}: parse not stable");
            let s2 = nodes_to_source(&p1);
            assert_eq!(s1, s2, "fixture {i}: codegen is not a byte-identical fixed point");
            let p2 = host.parse_macro(&s2).unwrap_or_else(|e| panic!("fixture {i} 3rd parse: {e}"));
            assert_eq!(p1, p2, "fixture {i}: parse not stable at 3rd cycle");
        }
        eprintln!("[parser] parse_codegen_parse_idempotence OK ({} fixtures)", corpus.len());
    }

    // ── sidecar_parse_load_stress: 100 registered macros, then 1000 rapid parses, all under budget. ─
    {
        for i in 0..100 {
            host.register(&format!("pstress_{i}"), "def macro(ctx):\n    return 'ok'\n")
                .expect("register stress macro");
        }
        let src =
            "def macro(ctx):\n    neuron.notify(ctx.app)\n    if ctx.app == \"x.exe\":\n        neuron.type_text(\"y\")\n";
        let mut total = Duration::ZERO;
        let mut worst = Duration::ZERO;
        for n in 0..1000 {
            let t = Instant::now();
            let r = host.parse_macro(src);
            let el = t.elapsed();
            assert!(r.is_ok(), "parse #{n} failed/timed out under load: {r:?}");
            total += el;
            worst = worst.max(el);
        }
        for i in 0..100 {
            host.unregister(&format!("pstress_{i}"));
        }
        let avg = total / 1000;
        eprintln!("[parser] sidecar_parse_load_stress OK (1000 parses: avg {avg:?}, worst {worst:?})");
        assert!(avg < Duration::from_millis(200), "avg parse latency too high: {avg:?}");
        assert!(worst < FIRE_BUDGET, "a parse hit the budget ceiling: {worst:?}");
    }

    eprintln!("[parser] all sidecar phases passed");
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// codegen `nodes` -> source -> parse via the sidecar -> assert it re-parses to the SAME tree (the
/// visual constructor's load-bearing guarantee).
fn rt(host: &MacroHost, nodes: &[MacroNode]) {
    let src = nodes_to_source(nodes);
    let parsed = host
        .parse_macro(&src)
        .unwrap_or_else(|e| panic!("parse failed: {e}\nsrc:\n{src}"));
    assert_eq!(parsed.as_slice(), nodes, "round-trip mismatch.\nsrc:\n{src}");
}

/// A left-nested chain of `+` Bins, `depth` deep (the parser yields a left-nested `BinOp` tree).
fn nested_bin(depth: usize) -> Value {
    let mut v = s("x");
    for _ in 0..depth {
        v = bin("+", v, s("y"));
    }
    v
}

/// A `depth`-deep transform-call chain: `x.strip().strip()…`.
fn nested_call(depth: usize) -> Value {
    let mut v = var("x");
    for _ in 0..depth {
        v = Value::Call { recv: Box::new(v), method: "strip".into(), args: vec![] };
    }
    v
}

/// `depth` nested flow wrappers around a leaf, cycling for/if/try/foreach/while. Each wrapper has a
/// NON-empty body (and Try a non-empty except), so the tree codegens to source that re-parses to the
/// exact same tree.
fn nest_flows(depth: usize) -> Vec<MacroNode> {
    let mut inner = leaf();
    for i in 0..depth {
        inner = vec![match i % 5 {
            0 => MacroNode::RepeatN { count: Value::Int { n: 2 }, body: inner },
            1 => MacroNode::If { cond: var("a"), then_: inner, else_: vec![] },
            2 => MacroNode::Try { body: inner, except_: vec![notify(s("caught"))] },
            3 => MacroNode::ForEach { var: "x".into(), source: var("xs"), body: inner },
            _ => MacroNode::RepeatWhile { cond: Value::Bool { b: false }, body: inner },
        }];
    }
    inner
}

/// ~100 varied node trees for the idempotence sweep — every Value variant the parser models, in
/// every single-Value node template, plus fixed multi-arg / flow / Raw fixtures.
fn idempotence_corpus() -> Vec<Vec<MacroNode>> {
    // Value samples the parser reproduces EXACTLY (so codegen->parse is identity).
    let values: Vec<Value> = vec![
        s("plain"),
        s("with \"quotes\" and \\ slash"),
        Value::Int { n: 42 },
        Value::Int { n: -7 },
        Value::Bool { b: true },
        Value::Ctx { field: "selection".into() },
        var("myvar"),
        Value::Call { recv: Box::new(Value::Ctx { field: "selection".into() }), method: "upper".into(), args: vec![] },
        Value::Call { recv: Box::new(s("a,b")), method: "replace".into(), args: vec![s(","), s(" ")] },
        bin("+", s("got "), Value::Ctx { field: "app".into() }),
        bin("==", Value::Ctx { field: "app".into() }, s("code.exe")),
        Value::raw("[w for w in ctx.selection.split()]"),
    ];
    // single-Value node templates.
    let templates: Vec<fn(Value) -> MacroNode> = vec![
        |v| MacroNode::Notify { text: v },
        |v| MacroNode::Copy { text: v },
        |v| MacroNode::Type { text: v, ghost: false, speed: None },
        |v| MacroNode::Wait { ms: v },
        |v| MacroNode::Scroll { amount: v },
        |v| MacroNode::Focus { window: v },
        |v| MacroNode::SetVar { name: "r".into(), value: v },
        |v| MacroNode::RepeatN { count: v, body: vec![MacroNode::Notify { text: Value::str("x") }] },
        |v| MacroNode::RepeatWhile { cond: v, body: vec![MacroNode::Notify { text: Value::str("x") }] },
        |v| MacroNode::If { cond: v, then_: vec![MacroNode::Notify { text: Value::str("y") }], else_: vec![] },
    ];
    let mut corpus: Vec<Vec<MacroNode>> = Vec::new();
    for tmpl in &templates {
        for v in &values {
            corpus.push(vec![tmpl(v.clone())]);
        }
    }
    // fixed multi-arg / flow / escape-hatch fixtures.
    corpus.push(vec![MacroNode::Press { keys: vec!["ctrl".into(), "shift".into(), "v".into()] }]);
    corpus.push(vec![MacroNode::KeyPress { name: "enter".into() }]);
    corpus.push(vec![MacroNode::Click { button: "right".into() }]);
    corpus.push(vec![MacroNode::MoveTo { x: Value::Int { n: 100 }, y: Value::Int { n: 200 } }]);
    corpus.push(vec![MacroNode::Paste]);
    corpus.push(vec![MacroNode::Stop]);
    corpus.push(vec![MacroNode::Open { command: s("git status"), capture: Some("out".into()) }]);
    corpus.push(vec![MacroNode::Type { text: s("ghost"), ghost: true, speed: Some("fast".into()) }]);
    corpus.push(vec![MacroNode::Ask {
        question: s("go?"),
        description: s("3 files lost"),
        yes: vec![MacroNode::Notify { text: s("y") }],
        no: vec![MacroNode::Notify { text: s("n") }],
    }]);
    corpus.push(vec![MacroNode::ForEach {
        var: "line".into(),
        source: Value::Call { recv: Box::new(Value::Ctx { field: "selection".into() }), method: "splitlines".into(), args: vec![] },
        body: vec![MacroNode::Type { text: var("line"), ghost: false, speed: None }],
    }]);
    corpus.push(vec![MacroNode::Try {
        body: vec![MacroNode::Open { command: s("risky"), capture: None }],
        except_: vec![MacroNode::Notify { text: s("failed") }],
    }]);
    corpus.push(vec![MacroNode::Raw { code: "x = {k: v for k, v in items}".into() }]);
    corpus
}

// ── setup / teardown (isolate the macros dir into a private temp cwd) ──────────────────────────────
// The pin must outlive the whole test, so it rides along in the returned tuple instead of dropping
// at the end of this fn.
fn setup() -> Option<(std::path::PathBuf, std::path::PathBuf, neuron::runroot::RunDirPin)> {
    let tmp = std::env::temp_dir().join(format!("neuron_macro_stress_parser_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let run_pin = neuron::runroot::RunDirPin::to(&tmp);
    let host = macro_host();
    if !host.available() {
        eprintln!("skipping parser_stress: bundled python runtime did not materialize");
        std::env::set_current_dir(&prev).ok();
        drop(run_pin);
        let _ = std::fs::remove_dir_all(&tmp);
        return None;
    }
    host.set_armed(false); // parse-only; never execute, never synthesize input
    Some((prev, tmp, run_pin))
}
