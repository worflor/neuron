// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The NODE MODEL — the two-way contract between a macro's Python source and a typed tree.
//!
//! A macro is the ordered statements inside `def macro(ctx):`; the wrapper `def` is implicit, so a
//! macro is modelled as a `Vec<MacroNode>` (its body). The mapping is BIDIRECTIONAL:
//!
//!   * Python -> nodes ("parse") happens in the warm CPython sidecar via `ast` (it owns the real
//!     grammar — we never re-implement Python parsing in Rust). See [`crate::macros::macro_host`]'s
//!     `parse_macro`, which sends a `parse` frame and deserializes the JSON the host emits.
//!   * nodes -> Python ("codegen") is [`nodes_to_source`], here, in pure Rust: instant, no process,
//!     no Python required. It emits a full `def macro(ctx):` module that re-parses to the same tree.
//!
//! The JSON shape is the wire format both sides agree on: `#[serde(tag = "kind")]` snake_case, so a
//! node is e.g. `{"kind":"type","text":{...},"ghost":false}`. The Python `_parse_nodes` emits exactly
//! this and serde here deserializes it; [`nodes_to_source`] is its exact inverse for the modelled
//! statements.
//!
//! THE VALUE SYSTEM is the keystone of "not limiting": every parameter that can hold data (the text
//! to type, the question to ask, the count to repeat, the condition to branch on) is a [`Value`] — a
//! small expression tree that codegens to a Python expr and parses back from an `ast` expr. A literal
//! (`"hi"`, `5`, `True`), a captured-world read (`ctx.selection`), a bare name (`x`), a transform
//! chain (`ctx.selection.upper()`), a binary expression (`"got " + ctx.app`), or — the escape hatch —
//! ANY other Python expression verbatim ([`Value::Raw`]). So a parameter is never "just a string".
//!
//! Anything the model doesn't understand at the STATEMENT level round-trips losslessly as
//! [`MacroNode::Raw`] — the verbatim source of one top-level statement. That is the safety valve that
//! lets a half-written or arbitrarily-complex macro still map to a tree (every statement becomes
//! *some* node) without ever losing or mangling code we don't model.

use serde::{Deserialize, Serialize};

/// A VALUE — a small expression tree behind every data parameter. Codegens to one Python expression
/// ([`value_to_source`]); the sidecar's `_parse_value` is its inverse over the `ast`. The variants
/// climb from concrete literals up to the [`Value::Raw`] escape hatch (any expression verbatim), so a
/// parameter can be a literal, a captured-world read, a name, a transform chain, a binary expression,
/// or — when nothing else fits — arbitrary Python. KEEP the `v` tag + snake_case stable: it's the
/// cross-language wire contract, exactly like [`MacroNode`]'s `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "v", rename_all = "snake_case")]
pub enum Value {
    /// A string literal — `"text"`.
    Str { s: String },
    /// An integer literal — `5`.
    Int { n: i64 },
    /// A boolean literal — `True` / `False`.
    Bool { b: bool },
    /// A captured-world read — `ctx.<field>` (selection|clipboard|app|title|cwd|…). Read-only.
    Ctx { field: String },
    /// A bare name — `x` (a variable set earlier, or a builtin).
    Var { name: String },
    /// A method/transform call on a receiver — `recv.method(args)`. The transforms a user reaches
    /// for: `.upper()`, `.lower()`, `.strip()`, `.replace(a, b)`, `.splitlines()`, `.split(s)`, …
    Call {
        recv: Box<Value>,
        method: String,
        args: Vec<Value>,
    },
    /// A binary expression — `left op right`. `op` is the Python operator verbatim: arithmetic
    /// (`+ - * / // % **`), comparison (`== != < > <= >= in not in is`), or boolean (`and or`).
    Bin {
        op: String,
        left: Box<Value>,
        right: Box<Value>,
    },
    /// ANY other expression, verbatim — the value escape hatch. `expr` is the `ast.unparse` of the
    /// node the value parser couldn't map to a typed variant (a list comp, an f-string, a ternary,
    /// a subscript, a function call on a non-Value receiver, …). Re-emitted byte-for-byte.
    Raw { expr: String },
}

impl Value {
    /// A convenience constructor for an empty string literal — the default for a text-ish param.
    pub fn empty_str() -> Value {
        Value::Str { s: String::new() }
    }

    /// A convenience constructor for a string literal.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str { s: s.into() }
    }

    /// A convenience constructor for the raw escape hatch (a verbatim expression).
    pub fn raw(expr: impl Into<String>) -> Value {
        Value::Raw { expr: expr.into() }
    }
}

/// Codegen ONE value to a Python expression. The exact inverse of the sidecar's `_parse_value`, so a
/// parsed value re-emits to source that re-parses to the same value. [`Value::Bin`] is always
/// parenthesised so precedence is unambiguous and the round-trip is stable (the parser strips the
/// redundant parens back into the same tree). [`Value::Raw`] is emitted verbatim — the user's exact
/// expression.
pub fn value_to_source(value: &Value) -> String {
    match value {
        Value::Str { s } => py_str_literal(s),
        Value::Int { n } => n.to_string(),
        Value::Bool { b } => if *b { "True" } else { "False" }.to_string(),
        Value::Ctx { field } => format!("ctx.{field}"),
        Value::Var { name } => name.clone(),
        Value::Call { recv, method, args } => {
            let args = args
                .iter()
                .map(value_to_source)
                .collect::<Vec<_>>()
                .join(", ");
            // A bare integer literal cannot take an attribute in Python source — `5.x()` lexes
            // as the float literal `5.` followed by `x` ("invalid decimal literal"). Parenthesize
            // an Int receiver so every representable tree emits parseable source.
            let recv_src = value_to_source(recv);
            let recv_src = match recv.as_ref() {
                Value::Int { .. } => format!("({recv_src})"),
                _ => recv_src,
            };
            format!("{recv_src}.{method}({args})")
        }
        Value::Bin { op, left, right } => {
            format!(
                "({} {op} {})",
                value_to_source(left),
                value_to_source(right)
            )
        }
        Value::Raw { expr } => expr.clone(),
    }
}

/// One node in a macro's body. ACTIONS map to a single Neuron helper call; FLOW nodes map to a Python
/// control statement (`if`/`for`/`while`/`try`/assignment/`return`); [`MacroNode::Raw`] captures
/// any unmodelled statement verbatim. Every data parameter is a [`Value`] (an expression tree), so
/// `neuron.type_text(ctx.selection.upper())` is fully modelled — the text is a `Value`, not a string.
///
/// Serde uses an internally-tagged `kind` discriminator in snake_case, so the JSON the Python host
/// emits (`_parse_nodes`) and the JSON serde produces/consumes here are identical. KEEP this tag +
/// casing stable — it is the cross-language contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MacroNode {
    // ── actions ─────────────────────────────────────────────────────────────────────────────────
    /// `neuron.type_text(t)` (or `neuron.type_ghost(t, speed)` when `ghost`). Type a value as keys.
    Type {
        text: Value,
        ghost: bool,
        speed: Option<String>,
    },
    /// `neuron.hotkey(*keys)` — a key chord (key NAMES, in order — not values).
    Press { keys: Vec<String> },
    /// `neuron.key(name)` — a single key by name.
    KeyPress { name: String },
    /// `neuron.click(button)` — click a mouse button ("left"|"right"|"middle").
    Click { button: String },
    /// `neuron.scroll(n)` — scroll the wheel by `n` notches.
    Scroll { amount: Value },
    /// `neuron.mouse_to(x, y)` — move the cursor to an absolute pixel.
    MoveTo { x: Value, y: Value },
    /// `neuron.clipboard_set(t)` — put a value on the clipboard.
    Copy { text: Value },
    /// `neuron.hotkey("ctrl", "v")` — special-cased both ways (codegen + parse).
    Paste,
    /// `neuron.run(cmd)` (fire-and-forget) or `<capture> = neuron.run(cmd, wait=True)` when capture
    /// is `Some` — run a command, optionally binding its `(code, stdout)` to a variable.
    Open {
        command: Value,
        capture: Option<String>,
    },
    /// `neuron.focus(w)` — bring a window to the foreground by title.
    Focus { window: Value },
    /// `neuron.sleep(ms)` — pause.
    Wait { ms: Value },
    /// `neuron.notify(t)` — a fire-and-forget status line.
    Notify { text: Value },

    // ── flow ────────────────────────────────────────────────────────────────────────────────────
    /// `if neuron.ask(q, description=d): <yes> [else: <no>]` — a yes/no human gate. `description=`
    /// is omitted when it's the empty literal.
    Ask {
        question: Value,
        description: Value,
        yes: Vec<MacroNode>,
        no: Vec<MacroNode>,
    },
    /// `if cond: <then_> [else: <else_>]` — a condition branch.
    If {
        cond: Value,
        then_: Vec<MacroNode>,
        else_: Vec<MacroNode>,
    },
    /// `for _ in range(count): <body>` — repeat N times.
    RepeatN { count: Value, body: Vec<MacroNode> },
    /// `while cond: <body>` — repeat while a condition holds.
    RepeatWhile { cond: Value, body: Vec<MacroNode> },
    /// `for var in source: <body>` — iterate a collection, binding each element to `var`.
    ForEach {
        var: String,
        source: Value,
        body: Vec<MacroNode>,
    },
    /// `name = value` — set a variable.
    SetVar { name: String, value: Value },
    /// `return` — stop the macro early.
    Stop,
    /// `try: <body> except Exception: <except_>` — guard a body.
    Try {
        body: Vec<MacroNode>,
        except_: Vec<MacroNode>,
    },

    // ── escape hatch ────────────────────────────────────────────────────────────────────────────
    /// ANY statement the model doesn't recognise — its verbatim source (one top-level statement,
    /// possibly multi-line). This is the lossless safety valve.
    Raw { code: String },
}

/// One indent level of the generated module.
const INDENT: &str = "    ";

/// Emit a valid Python double-quoted string literal for `s` (escaping `\`, `"`, newlines, tabs,
/// carriage returns, and other control chars) so the generated source always re-parses to the same
/// string value. Mirrors what Python's own `repr`/`ast.unparse` would accept on the way back in.
pub fn py_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // other ASCII control chars -> \xNN so the literal stays printable + valid.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Codegen: a slice of nodes -> a complete, runnable `def macro(ctx):` Python module.
///
/// The wrapper `def macro(ctx):` is implicit in the model, so it is added here. An empty body emits
/// `pass`. The output is the exact inverse of the sidecar's parse for every modelled statement, so
/// `parse(nodes_to_source(parse(src))) == parse(src)` (round-trip stable). [`MacroNode::Raw`] is
/// emitted verbatim, re-indented to its nesting level.
pub fn nodes_to_source(nodes: &[MacroNode]) -> String {
    let mut out = String::from("def macro(ctx):\n");
    if nodes.is_empty() {
        out.push_str(INDENT);
        out.push_str("pass\n");
    } else {
        emit_block(nodes, 1, &mut out);
    }
    out
}

/// Emit a block of nodes at the given indent `level` (1 = directly inside `def macro`). An empty
/// block emits a `pass` at this level (so an empty `if`/`else`/loop/`try` body is still valid Python).
fn emit_block(nodes: &[MacroNode], level: usize, out: &mut String) {
    if nodes.is_empty() {
        indent(level, out);
        out.push_str("pass\n");
        return;
    }
    for node in nodes {
        emit_node(node, level, out);
    }
}

/// Emit one node at `level`.
fn emit_node(node: &MacroNode, level: usize, out: &mut String) {
    match node {
        // ── actions ──────────────────────────────────────────────────────────────────────────
        MacroNode::Type { text, ghost, speed } => {
            indent(level, out);
            if *ghost {
                let speed = speed.as_deref().unwrap_or("borderline");
                out.push_str(&format!(
                    "neuron.type_ghost({}, {})\n",
                    value_to_source(text),
                    py_str_literal(speed)
                ));
            } else {
                out.push_str(&format!("neuron.type_text({})\n", value_to_source(text)));
            }
        }
        MacroNode::Press { keys } => {
            let args = keys
                .iter()
                .map(|k| py_str_literal(k))
                .collect::<Vec<_>>()
                .join(", ");
            indent(level, out);
            out.push_str(&format!("neuron.hotkey({args})\n"));
        }
        MacroNode::KeyPress { name } => {
            indent(level, out);
            out.push_str(&format!("neuron.key({})\n", py_str_literal(name)));
        }
        MacroNode::Click { button } => {
            indent(level, out);
            out.push_str(&format!("neuron.click({})\n", py_str_literal(button)));
        }
        MacroNode::Scroll { amount } => {
            indent(level, out);
            out.push_str(&format!("neuron.scroll({})\n", value_to_source(amount)));
        }
        MacroNode::MoveTo { x, y } => {
            indent(level, out);
            out.push_str(&format!(
                "neuron.mouse_to({}, {})\n",
                value_to_source(x),
                value_to_source(y)
            ));
        }
        MacroNode::Copy { text } => {
            indent(level, out);
            out.push_str(&format!("neuron.clipboard_set({})\n", value_to_source(text)));
        }
        MacroNode::Paste => {
            indent(level, out);
            out.push_str("neuron.hotkey(\"ctrl\", \"v\")\n");
        }
        MacroNode::Open { command, capture } => {
            indent(level, out);
            match capture {
                Some(name) => out.push_str(&format!(
                    "{name} = neuron.run({}, wait=True)\n",
                    value_to_source(command)
                )),
                None => out.push_str(&format!("neuron.run({})\n", value_to_source(command))),
            }
        }
        MacroNode::Focus { window } => {
            indent(level, out);
            out.push_str(&format!("neuron.focus({})\n", value_to_source(window)));
        }
        MacroNode::Wait { ms } => {
            indent(level, out);
            out.push_str(&format!("neuron.sleep({})\n", value_to_source(ms)));
        }
        MacroNode::Notify { text } => {
            indent(level, out);
            out.push_str(&format!("neuron.notify({})\n", value_to_source(text)));
        }

        // ── flow ─────────────────────────────────────────────────────────────────────────────
        MacroNode::Ask {
            question,
            description,
            yes,
            no,
        } => {
            indent(level, out);
            // omit description= when it is the empty string literal (the unset default).
            let describe = !matches!(description, Value::Str { s } if s.is_empty());
            if describe {
                out.push_str(&format!(
                    "if neuron.ask({}, description={}):\n",
                    value_to_source(question),
                    value_to_source(description)
                ));
            } else {
                out.push_str(&format!("if neuron.ask({}):\n", value_to_source(question)));
            }
            emit_block(yes, level + 1, out);
            if !no.is_empty() {
                indent(level, out);
                out.push_str("else:\n");
                emit_block(no, level + 1, out);
            }
        }
        MacroNode::If { cond, then_, else_ } => {
            indent(level, out);
            out.push_str(&format!("if {}:\n", value_to_source(cond)));
            emit_block(then_, level + 1, out);
            if !else_.is_empty() {
                indent(level, out);
                out.push_str("else:\n");
                emit_block(else_, level + 1, out);
            }
        }
        MacroNode::RepeatN { count, body } => {
            indent(level, out);
            out.push_str(&format!("for _ in range({}):\n", value_to_source(count)));
            emit_block(body, level + 1, out);
        }
        MacroNode::RepeatWhile { cond, body } => {
            indent(level, out);
            out.push_str(&format!("while {}:\n", value_to_source(cond)));
            emit_block(body, level + 1, out);
        }
        MacroNode::ForEach { var, source, body } => {
            indent(level, out);
            out.push_str(&format!("for {var} in {}:\n", value_to_source(source)));
            emit_block(body, level + 1, out);
        }
        MacroNode::SetVar { name, value } => {
            indent(level, out);
            out.push_str(&format!("{name} = {}\n", value_to_source(value)));
        }
        MacroNode::Stop => {
            indent(level, out);
            out.push_str("return\n");
        }
        MacroNode::Try { body, except_ } => {
            indent(level, out);
            out.push_str("try:\n");
            emit_block(body, level + 1, out);
            // the except keyword always appears when the arm has nodes (a try with no except is
            // not valid Python; an empty modelled except is not emitted — that node would not have
            // been parsed from a no-except try in the first place).
            if !except_.is_empty() {
                indent(level, out);
                out.push_str("except Exception:\n");
                emit_block(except_, level + 1, out);
            } else {
                // a Try node with an empty except still needs a valid handler to be runnable Python.
                indent(level, out);
                out.push_str("except Exception:\n");
                indent(level + 1, out);
                out.push_str("pass\n");
            }
        }

        // ── escape hatch ─────────────────────────────────────────────────────────────────────
        MacroNode::Raw { code } => {
            // A freshly-added "code" block the user hasn't filled yet (empty / whitespace-only) would
            // otherwise emit a blank body line — invalid Python ("expected an indented block") and a
            // spurious parse warning the user never earned. Emit `pass` so a half-built macro is ALWAYS
            // valid; the user's real code replaces it. Otherwise: verbatim, re-indented per line
            // (relative indentation inside the statement preserved; a trailing blank line kept bare).
            if code.trim().is_empty() {
                indent(level, out);
                out.push_str("pass\n");
            } else {
                for line in code.split('\n') {
                    if line.is_empty() {
                        out.push('\n');
                    } else {
                        indent(level, out);
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
    }
}

/// Push `level` indents.
fn indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str(INDENT);
    }
}

// ── plain-English summary ───────────────────────────────────────────────────────────────────────

/// How many top-level steps [`summarize`] lists before it elides the rest with a trailing "…".
const SUMMARY_STEPS: usize = 5;

/// EMERGENT plain-English summary — walk a macro's TOP-LEVEL nodes into short step labels and join
/// them, so a catalog can say what a macro DOES without anyone opening its code. Every label is
/// derived from the node itself (its verb + its own data) — there is NO per-macro hardcoding. Flow
/// nodes summarize their own shape ("if …", "repeats 3×…") rather than spilling their body, so the
/// line stays a one-glance overview; past [`SUMMARY_STEPS`] the tail collapses to "…".
pub fn summarize(nodes: &[MacroNode]) -> String {
    if nodes.is_empty() {
        return "does nothing yet".to_string();
    }
    let mut parts: Vec<String> = nodes.iter().take(SUMMARY_STEPS).map(step_label).collect();
    if nodes.len() > SUMMARY_STEPS {
        parts.push("\u{2026}".to_string());
    }
    parts.join(", ")
}

/// One node -> a short plain-English label (the unit [`summarize`] joins). Derived entirely from the
/// node's own verb and data — no lookup table of macro names.
fn step_label(node: &MacroNode) -> String {
    match node {
        MacroNode::Type { ghost, .. } => {
            if *ghost {
                "ghost-types text".into()
            } else {
                "types text".into()
            }
        }
        MacroNode::Press { keys } => {
            if keys.is_empty() {
                "presses a chord".into()
            } else {
                format!("presses {}", keys.join("+"))
            }
        }
        MacroNode::KeyPress { name } => format!("presses {name}"),
        MacroNode::Click { button } => format!("clicks {button}"),
        MacroNode::Scroll { .. } => "scrolls".into(),
        MacroNode::MoveTo { .. } => "moves mouse".into(),
        MacroNode::Copy { .. } => "copies".into(),
        MacroNode::Paste => "pastes".into(),
        MacroNode::Open { command, .. } => format!("runs {}", short_value(command)),
        MacroNode::Focus { window } => format!("focuses {}", short_value(window)),
        MacroNode::Wait { ms } => format!("waits {}ms", short_value(ms)),
        MacroNode::Notify { .. } => "notifies".into(),
        MacroNode::Ask { question, .. } => format!("asks: \"{}\"", short_value(question)),
        MacroNode::If { cond, .. } => format!("if {}\u{2026}", short_value(cond)),
        MacroNode::RepeatN { count, .. } => format!("repeats {}\u{00d7}\u{2026}", short_value(count)),
        MacroNode::RepeatWhile { .. } => "repeats while\u{2026}".into(),
        MacroNode::ForEach { .. } => "for each\u{2026}".into(),
        MacroNode::SetVar { name, .. } => format!("sets {name}"),
        MacroNode::Stop => "stops".into(),
        MacroNode::Try { .. } => "tries\u{2026}".into(),
        MacroNode::Raw { .. } => "custom code".into(),
    }
}

/// A short, human rendering of a [`Value`] for a summary label: a string literal shows its text (no
/// quotes), everything else its source expression. Truncated so one long argument can't blow out the
/// whole line.
fn short_value(v: &Value) -> String {
    let raw = match v {
        Value::Str { s } => s.clone(),
        other => value_to_source(other),
    };
    let s = raw.trim();
    const MAX: usize = 24;
    if s.chars().count() > MAX {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}\u{2026}")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Brings `Strategy::prop_map`/`.boxed()` etc. into scope for the injection-hardness properties
    // further down (everything else in this module is called through fully-qualified `proptest::`
    // paths to avoid any risk of colliding with this file's own `Value` type).
    use proptest::strategy::Strategy as _;

    // ── Value codegen ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn value_literals_codegen() {
        assert_eq!(value_to_source(&Value::str("hi")), "\"hi\"");
        assert_eq!(value_to_source(&Value::Int { n: 5 }), "5");
        assert_eq!(value_to_source(&Value::Int { n: -3 }), "-3");
        assert_eq!(value_to_source(&Value::Bool { b: true }), "True");
        assert_eq!(value_to_source(&Value::Bool { b: false }), "False");
    }

    #[test]
    fn value_ctx_and_var_codegen() {
        assert_eq!(
            value_to_source(&Value::Ctx {
                field: "selection".into()
            }),
            "ctx.selection"
        );
        assert_eq!(value_to_source(&Value::Var { name: "x".into() }), "x");
    }

    #[test]
    fn value_call_transforms_codegen() {
        // ctx.selection.upper()
        let v = Value::Call {
            recv: Box::new(Value::Ctx {
                field: "selection".into(),
            }),
            method: "upper".into(),
            args: vec![],
        };
        assert_eq!(value_to_source(&v), "ctx.selection.upper()");
        // "a,b".replace(",", " ")
        let v = Value::Call {
            recv: Box::new(Value::str("a,b")),
            method: "replace".into(),
            args: vec![Value::str(","), Value::str(" ")],
        };
        assert_eq!(value_to_source(&v), "\"a,b\".replace(\",\", \" \")");
    }

    #[test]
    fn value_bin_codegen_is_parenthesised() {
        // "got " + ctx.app
        let v = Value::Bin {
            op: "+".into(),
            left: Box::new(Value::str("got ")),
            right: Box::new(Value::Ctx { field: "app".into() }),
        };
        assert_eq!(value_to_source(&v), "(\"got \" + ctx.app)");
        // nested: (a == b) and (c)
        let v = Value::Bin {
            op: "and".into(),
            left: Box::new(Value::Bin {
                op: "==".into(),
                left: Box::new(Value::Var { name: "a".into() }),
                right: Box::new(Value::Var { name: "b".into() }),
            }),
            right: Box::new(Value::Var { name: "c".into() }),
        };
        assert_eq!(value_to_source(&v), "((a == b) and c)");
    }

    #[test]
    fn value_raw_codegen_is_verbatim() {
        let v = Value::raw("[w for w in ctx.selection.split()]");
        assert_eq!(value_to_source(&v), "[w for w in ctx.selection.split()]");
    }

    // ── node codegen ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn empty_macro_is_pass() {
        assert_eq!(nodes_to_source(&[]), "def macro(ctx):\n    pass\n");
    }

    #[test]
    fn type_text_codegen() {
        let nodes = vec![MacroNode::Type {
            text: Value::str("hello world"),
            ghost: false,
            speed: None,
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.type_text(\"hello world\")\n"
        );
    }

    #[test]
    fn type_ghost_codegen() {
        let nodes = vec![MacroNode::Type {
            text: Value::str("typed"),
            ghost: true,
            speed: Some("fast".into()),
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.type_ghost(\"typed\", \"fast\")\n"
        );
    }

    #[test]
    fn type_value_codegen() {
        // type_text(ctx.selection.upper())
        let nodes = vec![MacroNode::Type {
            text: Value::Call {
                recv: Box::new(Value::Ctx {
                    field: "selection".into(),
                }),
                method: "upper".into(),
                args: vec![],
            },
            ghost: false,
            speed: None,
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.type_text(ctx.selection.upper())\n"
        );
    }

    #[test]
    fn press_and_key_and_click_codegen() {
        let nodes = vec![
            MacroNode::Press {
                keys: vec!["ctrl".into(), "c".into()],
            },
            MacroNode::KeyPress { name: "enter".into() },
            MacroNode::Click {
                button: "right".into(),
            },
        ];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             neuron.hotkey(\"ctrl\", \"c\")\n    \
             neuron.key(\"enter\")\n    \
             neuron.click(\"right\")\n"
        );
    }

    #[test]
    fn scroll_moveto_codegen() {
        let nodes = vec![
            MacroNode::Scroll {
                amount: Value::Int { n: -3 },
            },
            MacroNode::MoveTo {
                x: Value::Int { n: 100 },
                y: Value::Int { n: 200 },
            },
        ];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.scroll(-3)\n    neuron.mouse_to(100, 200)\n"
        );
    }

    #[test]
    fn copy_paste_codegen() {
        let nodes = vec![
            MacroNode::Copy {
                text: Value::Ctx {
                    field: "selection".into(),
                },
            },
            MacroNode::Paste,
        ];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.clipboard_set(ctx.selection)\n    neuron.hotkey(\"ctrl\", \"v\")\n"
        );
    }

    #[test]
    fn open_codegen_with_and_without_capture() {
        let nodes = vec![MacroNode::Open {
            command: Value::str("notepad"),
            capture: None,
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.run(\"notepad\")\n"
        );
        let nodes = vec![MacroNode::Open {
            command: Value::str("git status"),
            capture: Some("out".into()),
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    out = neuron.run(\"git status\", wait=True)\n"
        );
    }

    #[test]
    fn focus_wait_notify_codegen() {
        let nodes = vec![
            MacroNode::Focus {
                window: Value::str("Untitled - Notepad"),
            },
            MacroNode::Wait {
                ms: Value::Int { n: 500 },
            },
            MacroNode::Notify {
                text: Value::Bin {
                    op: "+".into(),
                    left: Box::new(Value::str("got ")),
                    right: Box::new(Value::Ctx { field: "app".into() }),
                },
            },
        ];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             neuron.focus(\"Untitled - Notepad\")\n    \
             neuron.sleep(500)\n    \
             neuron.notify((\"got \" + ctx.app))\n"
        );
    }

    #[test]
    fn ask_with_description_codegen() {
        let nodes = vec![MacroNode::Ask {
            question: Value::str("overwrite?"),
            description: Value::str("3 files lost"),
            yes: vec![MacroNode::Notify {
                text: Value::str("ok"),
            }],
            no: vec![],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             if neuron.ask(\"overwrite?\", description=\"3 files lost\"):\n        \
             neuron.notify(\"ok\")\n"
        );
    }

    #[test]
    fn ask_without_description_omits_kw() {
        let nodes = vec![MacroNode::Ask {
            question: Value::str("go?"),
            description: Value::empty_str(),
            yes: vec![MacroNode::Notify {
                text: Value::str("y"),
            }],
            no: vec![MacroNode::Notify {
                text: Value::str("n"),
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             if neuron.ask(\"go?\"):\n        \
             neuron.notify(\"y\")\n    \
             else:\n        \
             neuron.notify(\"n\")\n"
        );
    }

    #[test]
    fn if_then_else_codegen() {
        let nodes = vec![MacroNode::If {
            cond: Value::Bin {
                op: "==".into(),
                left: Box::new(Value::Ctx { field: "app".into() }),
                right: Box::new(Value::str("code.exe")),
            },
            then_: vec![MacroNode::Notify {
                text: Value::str("vscode"),
            }],
            else_: vec![MacroNode::Notify {
                text: Value::str("other"),
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             if (ctx.app == \"code.exe\"):\n        \
             neuron.notify(\"vscode\")\n    \
             else:\n        \
             neuron.notify(\"other\")\n"
        );
    }

    #[test]
    fn repeat_n_codegen() {
        let nodes = vec![MacroNode::RepeatN {
            count: Value::Int { n: 3 },
            body: vec![MacroNode::KeyPress {
                name: "down".into(),
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    for _ in range(3):\n        neuron.key(\"down\")\n"
        );
    }

    #[test]
    fn repeat_while_codegen() {
        let nodes = vec![MacroNode::RepeatWhile {
            cond: Value::Bool { b: true },
            body: vec![MacroNode::Wait {
                ms: Value::Int { n: 100 },
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    while True:\n        neuron.sleep(100)\n"
        );
    }

    #[test]
    fn for_each_codegen() {
        let nodes = vec![MacroNode::ForEach {
            var: "line".into(),
            source: Value::Call {
                recv: Box::new(Value::Ctx {
                    field: "selection".into(),
                }),
                method: "splitlines".into(),
                args: vec![],
            },
            body: vec![MacroNode::Type {
                text: Value::Var { name: "line".into() },
                ghost: false,
                speed: None,
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    for line in ctx.selection.splitlines():\n        neuron.type_text(line)\n"
        );
    }

    #[test]
    fn set_var_and_stop_codegen() {
        let nodes = vec![
            MacroNode::SetVar {
                name: "n".into(),
                value: Value::Int { n: 0 },
            },
            MacroNode::Stop,
        ];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    n = 0\n    return\n"
        );
    }

    #[test]
    fn try_except_codegen() {
        let nodes = vec![MacroNode::Try {
            body: vec![MacroNode::Open {
                command: Value::str("risky"),
                capture: None,
            }],
            except_: vec![MacroNode::Notify {
                text: Value::str("failed"),
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             try:\n        \
             neuron.run(\"risky\")\n    \
             except Exception:\n        \
             neuron.notify(\"failed\")\n"
        );
    }

    #[test]
    fn empty_flow_bodies_emit_pass() {
        let nodes = vec![MacroNode::RepeatN {
            count: Value::Int { n: 2 },
            body: vec![],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    for _ in range(2):\n        pass\n"
        );
    }

    #[test]
    fn raw_line_codegen() {
        let nodes = vec![MacroNode::Raw {
            code: "x = ctx.selection".into(),
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    x = ctx.selection\n"
        );
    }

    #[test]
    fn multiline_raw_is_reindented_to_level() {
        let nodes = vec![MacroNode::Raw {
            code: "with open('f') as fh:\n    print(fh.read())".into(),
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    with open('f') as fh:\n        print(fh.read())\n"
        );
    }

    #[test]
    fn string_escaping_round_trips() {
        let nodes = vec![MacroNode::Type {
            text: Value::str("say \"hi\" \\ end"),
            ghost: false,
            speed: None,
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    neuron.type_text(\"say \\\"hi\\\" \\\\ end\")\n"
        );
    }

    #[test]
    fn py_str_literal_escapes_controls() {
        assert_eq!(py_str_literal("a\nb\tc\rd"), "\"a\\nb\\tc\\rd\"");
        assert_eq!(py_str_literal("tab\u{0007}bell"), "\"tab\\x07bell\"");
    }

    // ── py_str_literal injection-hardness (decoder + properties) ────────────────────────────────
    //
    // `py_str_literal` is a code-injection boundary: it splices arbitrary macro text into GENERATED
    // PYTHON SOURCE. The escape forms it can EMIT (read off the match arms above) are exactly:
    //   `\\`  (backslash)     `\"`  (the delimiter quote)     `\n` `\r` `\t`     `\xNN` (control < 0x20)
    // Everything else is passed through verbatim (no `\uNNNN`, no `\'` — the literal is always
    // double-quoted, so a literal `'` never needs escaping). The decoder below understands exactly
    // this set — nothing more — so it doubles as a spec-check: if `py_str_literal` ever starts
    // emitting a form the decoder doesn't know, `decode_py_str_literal` panics loudly instead of
    // silently accepting a new escape shape.
    /// Decode ONE double-quoted Python string literal (as `py_str_literal` emits it) back to its
    /// value. Returns `(decoded_value, chars_consumed)`; `chars_consumed` lets callers assert the
    /// decoder walked the ENTIRE literal and landed exactly on the closing quote — the mechanical
    /// check that nothing inside the body terminated the literal early (an unescaped quote) or threw
    /// the decoder off track (an ambiguous backslash).
    fn decode_py_str_literal(lit: &str) -> (String, usize) {
        let chars: Vec<char> = lit.chars().collect();
        assert_eq!(
            chars.first(),
            Some(&'"'),
            "literal must open with a double quote: {lit:?}"
        );
        let mut out = String::new();
        let mut i = 1;
        while i < chars.len() {
            match chars[i] {
                '"' => {
                    i += 1;
                    return (out, i);
                }
                '\\' => {
                    let esc = *chars
                        .get(i + 1)
                        .unwrap_or_else(|| panic!("dangling backslash in {lit:?}"));
                    match esc {
                        '\\' => {
                            out.push('\\');
                            i += 2;
                        }
                        '"' => {
                            out.push('"');
                            i += 2;
                        }
                        'n' => {
                            out.push('\n');
                            i += 2;
                        }
                        'r' => {
                            out.push('\r');
                            i += 2;
                        }
                        't' => {
                            out.push('\t');
                            i += 2;
                        }
                        'x' => {
                            let hi =
                                *chars.get(i + 2).unwrap_or_else(|| panic!("truncated \\x escape in {lit:?}"));
                            let lo =
                                *chars.get(i + 3).unwrap_or_else(|| panic!("truncated \\x escape in {lit:?}"));
                            let hex: String = [hi, lo].iter().collect();
                            let val = u32::from_str_radix(&hex, 16)
                                .unwrap_or_else(|_| panic!("invalid \\x escape {hex:?} in {lit:?}"));
                            out.push(
                                char::from_u32(val)
                                    .unwrap_or_else(|| panic!("invalid \\x codepoint {val:x} in {lit:?}")),
                            );
                            i += 4;
                        }
                        other => panic!(
                            "py_str_literal never emits \\{other} — decoder doesn't recognise it in {lit:?}"
                        ),
                    }
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        panic!("literal never closed with a quote: {lit:?}");
    }

    /// A string with the exact texture the injection properties need to stress: control chars
    /// (including NUL), quotes, backslashes, raw newlines/CRs, non-ASCII, and astral-plane chars.
    /// `any::<char>()` already covers this whole space (every Unicode scalar value Rust can hold).
    fn arb_injection_string() -> impl proptest::strategy::Strategy<Value = String> {
        proptest::collection::vec(proptest::prelude::any::<char>(), 0..64)
            .prop_map(|cs| cs.into_iter().collect())
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// (a) decode(py_str_literal(s)) == s for arbitrary strings — the literal always re-decodes
        /// to the exact original value, and the decoder consumes the WHOLE literal doing it.
        #[test]
        fn py_literal_round_trips_arbitrary_strings(s in arb_injection_string()) {
            let lit = py_str_literal(&s);
            let (decoded, consumed) = decode_py_str_literal(&lit);
            proptest::prop_assert_eq!(&decoded, &s, "round-trip mismatch for {:?}", s);
            proptest::prop_assert_eq!(
                consumed,
                lit.chars().count(),
                "decoder didn't consume the whole literal for {:?} (lit={:?})",
                s,
                lit
            );
        }

        /// (b) the literal body can never smuggle an early, unescaped closing quote or a raw
        /// line-break out into the surrounding generated source. Mechanically: the decoder must
        /// consume the entire literal (never stop short at a spurious unescaped quote), and no raw
        /// `\n`/`\r` byte may appear anywhere in the emitted literal (a line break there is a
        /// statement break in Python — the injection this whole function exists to prevent).
        #[test]
        fn literal_never_escapes_its_quotes(s in arb_injection_string()) {
            let lit = py_str_literal(&s);
            let (_decoded, consumed) = decode_py_str_literal(&lit);
            proptest::prop_assert_eq!(
                consumed,
                lit.chars().count(),
                "an unescaped quote (or an ambiguous backslash) terminated the literal early: {:?}",
                lit
            );
            proptest::prop_assert!(
                !lit.contains('\n') && !lit.contains('\r'),
                "a raw line break survived into the literal (statement break = injection): {lit:?}"
            );
        }
    }

    /// (c) a targeted corpus of adversarial payloads — quote-then-inject attempts, escape-sequence
    /// confusion, comment breakouts, mixed-quote floods, and a lone trailing backslash — all stay
    /// completely inert: each round-trips exactly and satisfies the same "no early termination, no
    /// raw line break" contract as the property above.
    #[test]
    fn adversarial_payloads_stay_inert() {
        let payloads: &[&str] = &[
            "'; import os #",
            "\\'; os.system(\"rm -rf /\")",
            "\"\"\"",
            "\"; import os; os.system(\"whoami\"); \"",
            "trailing backslash\\",
            "\\",
            "'",
            "\"",
            "\"'\"'\"'\"'",
            "\"\"\"\"\"\"\"\"",
            "\0",
            "line1\nline2\r\nline3",
            "\\n\\r\\t literal backslash-letter text, not real escapes",
            "\\x41\\x42 literal backslash-x-digit text, not a real escape",
            "\u{202e}reversed-by-bidi-override\u{202c}",
        ];
        for p in payloads {
            let lit = py_str_literal(p);
            let (decoded, consumed) = decode_py_str_literal(&lit);
            assert_eq!(decoded.as_str(), *p, "payload {p:?} did not round-trip (lit={lit:?})");
            assert_eq!(
                consumed,
                lit.chars().count(),
                "payload {p:?} let the decoder stop before the literal's true end (lit={lit:?})"
            );
            assert!(
                !lit.contains('\n') && !lit.contains('\r'),
                "payload {p:?} left a raw line break in the literal: {lit:?}"
            );
        }
    }

    #[test]
    fn nested_flow_codegen() {
        // for line in ctx.selection.splitlines(): if line: notify(line)
        let nodes = vec![MacroNode::ForEach {
            var: "line".into(),
            source: Value::Call {
                recv: Box::new(Value::Ctx {
                    field: "selection".into(),
                }),
                method: "splitlines".into(),
                args: vec![],
            },
            body: vec![MacroNode::If {
                cond: Value::Var { name: "line".into() },
                then_: vec![MacroNode::Notify {
                    text: Value::Var { name: "line".into() },
                }],
                else_: vec![],
            }],
        }];
        assert_eq!(
            nodes_to_source(&nodes),
            "def macro(ctx):\n    \
             for line in ctx.selection.splitlines():\n        \
             if line:\n            \
             neuron.notify(line)\n"
        );
    }

    // ── wire shape ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn serde_value_tag_shape() {
        let v = Value::Ctx {
            field: "selection".into(),
        };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j, serde_json::json!({"v": "ctx", "field": "selection"}));
        let back: Value = serde_json::from_value(j).unwrap();
        assert_eq!(back, v);

        let call = Value::Call {
            recv: Box::new(Value::Ctx {
                field: "app".into(),
            }),
            method: "upper".into(),
            args: vec![],
        };
        let j = serde_json::to_value(&call).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "v": "call",
                "recv": {"v": "ctx", "field": "app"},
                "method": "upper",
                "args": []
            })
        );
    }

    #[test]
    fn serde_node_tag_shape() {
        let n = MacroNode::Type {
            text: Value::str("hi"),
            ghost: false,
            speed: None,
        };
        let j = serde_json::to_value(&n).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "kind": "type",
                "text": {"v": "str", "s": "hi"},
                "ghost": false,
                "speed": null
            })
        );
        let back: MacroNode = serde_json::from_value(j).unwrap();
        assert_eq!(back, n);

        let ask = MacroNode::Ask {
            question: Value::str("q"),
            description: Value::empty_str(),
            yes: vec![MacroNode::Notify {
                text: Value::str("y"),
            }],
            no: vec![],
        };
        let j = serde_json::to_value(&ask).unwrap();
        assert_eq!(
            j,
            serde_json::json!({
                "kind": "ask",
                "question": {"v": "str", "s": "q"},
                "description": {"v": "str", "s": ""},
                "yes": [{"kind": "notify", "text": {"v": "str", "s": "y"}}],
                "no": []
            })
        );
        let back: MacroNode = serde_json::from_value(j).unwrap();
        assert_eq!(back, ask);
    }
}

#[cfg(test)]
mod summary_tests {
    //! The EMERGENT plain-English summarizer — proves each node arm maps to its label and that the
    //! line caps + elides. Derived from the nodes alone (no macro-name lookup), so these fixtures
    //! double as the spec for what the Workshop catalog reads off any macro on disk.
    use super::*;

    #[test]
    fn empty_macro_summary() {
        assert_eq!(summarize(&[]), "does nothing yet");
    }

    #[test]
    fn action_arms_map_to_labels() {
        let nodes = vec![
            MacroNode::Type {
                text: Value::str("hi"),
                ghost: false,
                speed: None,
            },
            MacroNode::Press {
                keys: vec!["ctrl".into(), "c".into()],
            },
            MacroNode::Copy {
                text: Value::Ctx {
                    field: "selection".into(),
                },
            },
        ];
        assert_eq!(summarize(&nodes), "types text, presses ctrl+c, copies");
    }

    #[test]
    fn ghost_type_and_key_and_click() {
        let nodes = vec![
            MacroNode::Type {
                text: Value::str("x"),
                ghost: true,
                speed: Some("fast".into()),
            },
            MacroNode::KeyPress {
                name: "enter".into(),
            },
            MacroNode::Click {
                button: "right".into(),
            },
        ];
        assert_eq!(
            summarize(&nodes),
            "ghost-types text, presses enter, clicks right"
        );
    }

    #[test]
    fn open_focus_wait_use_their_own_values() {
        let nodes = vec![
            MacroNode::Open {
                command: Value::str("notepad"),
                capture: None,
            },
            MacroNode::Focus {
                window: Value::str("Code"),
            },
            MacroNode::Wait {
                ms: Value::Int { n: 500 },
            },
        ];
        assert_eq!(summarize(&nodes), "runs notepad, focuses Code, waits 500ms");
    }

    #[test]
    fn ask_quotes_its_question() {
        let nodes = vec![MacroNode::Ask {
            question: Value::str("overwrite?"),
            description: Value::empty_str(),
            yes: vec![],
            no: vec![],
        }];
        assert_eq!(summarize(&nodes), "asks: \"overwrite?\"");
    }

    #[test]
    fn flow_nodes_summarize_their_shape_not_their_body() {
        let nodes = vec![
            MacroNode::If {
                cond: Value::Ctx { field: "app".into() },
                then_: vec![MacroNode::Notify {
                    text: Value::str("buried"),
                }],
                else_: vec![],
            },
            MacroNode::RepeatN {
                count: Value::Int { n: 3 },
                body: vec![],
            },
            MacroNode::RepeatWhile {
                cond: Value::Bool { b: true },
                body: vec![],
            },
            MacroNode::ForEach {
                var: "x".into(),
                source: Value::Var { name: "xs".into() },
                body: vec![],
            },
            MacroNode::Try {
                body: vec![],
                except_: vec![],
            },
        ];
        // body labels ("notify") never leak — only the top-level shapes show.
        assert_eq!(
            summarize(&nodes),
            "if ctx.app\u{2026}, repeats 3\u{00d7}\u{2026}, repeats while\u{2026}, for each\u{2026}, tries\u{2026}"
        );
        assert!(!summarize(&nodes).contains("notifies"));
    }

    #[test]
    fn setvar_stop_raw_paste() {
        let nodes = vec![
            MacroNode::SetVar {
                name: "n".into(),
                value: Value::Int { n: 0 },
            },
            MacroNode::Paste,
            MacroNode::Stop,
            MacroNode::Raw {
                code: "import os".into(),
            },
        ];
        assert_eq!(summarize(&nodes), "sets n, pastes, stops, custom code");
    }

    #[test]
    fn long_lists_cap_and_elide() {
        // SUMMARY_STEPS top-level steps, then a "…" tail — and nothing past the cap is rendered.
        let nodes = vec![
            MacroNode::Copy {
                text: Value::empty_str(),
            },
            MacroNode::Paste,
            MacroNode::Copy {
                text: Value::empty_str(),
            },
            MacroNode::Paste,
            MacroNode::Copy {
                text: Value::empty_str(),
            },
            MacroNode::Notify {
                text: Value::str("past the cap"),
            },
        ];
        let s = summarize(&nodes);
        assert!(s.ends_with("\u{2026}"), "overflow gets a trailing ellipsis");
        assert!(!s.contains("notifies"), "the 6th step is elided, not shown");
        assert_eq!(s.matches(',').count(), SUMMARY_STEPS, "5 steps + the … tail");
    }

    #[test]
    fn long_values_are_truncated() {
        let long = "a".repeat(80);
        let nodes = vec![MacroNode::Open {
            command: Value::str(long),
            capture: None,
        }];
        let s = summarize(&nodes);
        assert!(s.starts_with("runs aaaa"));
        assert!(s.ends_with("\u{2026}"), "a long value is cut with an ellipsis");
        assert!(s.chars().count() < 40, "the line is bounded, not 80+ chars");
    }
}

#[cfg(test)]
mod codegen_parse_proptests {
    //! parse<->codegen idempotence: an ARBITRARY (not hand-enumerated — see the corpus in
    //! `crates/neuron-core/tests/macro_stress_parser.rs`, which this deliberately does not duplicate)
    //! [`MacroNode`] tree must reach a stable fixed point through the sidecar's real `ast`-backed
    //! parser. Mirrors the EXACT equivalence the existing `parse_codegen_parse_idempotence` corpus
    //! test in that file asserts (not a stricter one): a first codegen→parse cycle may legitimately
    //! re-shape unrecognised sub-expressions (e.g. into [`Value::Raw`]), so we don't require
    //! `source == source'` after only one hop; we require the SECOND hop to be a byte-identical no-op
    //! (`p0 == p1` and `codegen(p1) == codegen(p0's reparse)`), exactly like the corpus fixture sweep.
    //!
    //! Needs the bundled CPython sidecar (the parse half is real `ast`, not reimplemented in Rust —
    //! see this module's top doc comment). Skips cleanly (asserts nothing) when it can't materialize,
    //! matching every other sidecar-dependent test in this codebase; `MacroHost::available()` is a
    //! cheap path-resolution check, not a spawn, so this costs nothing when the runtime is present but
    //! not yet warmed either.
    use super::*;
    use crate::macros::macro_host::macro_host;
    // Brings `Strategy::prop_map`/`.boxed()`, `Just`, `any`, `prop_oneof!` etc. into scope for every
    // strategy builder below (fully-qualified `proptest::` paths are used everywhere else in this
    // file to avoid any risk of colliding with this file's own `Value`/`MacroNode` types — neither
    // name is exported by proptest's prelude, so this glob import is safe).
    use proptest::prelude::*;

    /// A handful of syntactically-safe identifiers (no Python keywords) for every position that
    /// splices directly into source as a bare name (`var`/`name` bindings, `ctx.field`-less `Var`
    /// names, `Call` method names) — unlike string DATA (which goes through the hardened
    /// [`py_str_literal`] and can be anything, proven by the properties above), these positions have
    /// no escaping and MUST be valid identifiers or the generated source is simply invalid Python.
    fn arb_ident() -> impl proptest::strategy::Strategy<Value = String> {
        proptest::sample::select(&["x", "y", "z", "n", "i", "line", "val", "tmp", "acc", "out"][..])
            .prop_map(|s| s.to_string())
    }

    /// The known `ctx.<field>` names (mirrors the doc comment on [`Value::Ctx`]) — also a bare
    /// identifier splice, so also restricted rather than arbitrary.
    fn arb_ctx_field() -> impl proptest::strategy::Strategy<Value = String> {
        proptest::sample::select(&["selection", "clipboard", "app", "title", "cwd"][..])
            .prop_map(|s| s.to_string())
    }

    /// A real Python binary/boolean operator token (mirrors the sets the integration stress test
    /// exercises) — also a raw splice with no escaping, so restricted to the valid vocabulary.
    fn arb_bin_op() -> impl proptest::strategy::Strategy<Value = String> {
        proptest::sample::select(&["+", "-", "*", "==", "!=", "<", "and", "or", "in"][..])
            .prop_map(|s| s.to_string())
    }

    /// String DATA (goes through `py_str_literal`, so genuinely arbitrary — same texture as the
    /// injection-hardness properties above, just kept a little shorter for proptest tree-generation
    /// speed).
    fn arb_data_string() -> impl proptest::strategy::Strategy<Value = String> {
        proptest::collection::vec(proptest::prelude::any::<char>(), 0..24)
            .prop_map(|cs| cs.into_iter().collect())
    }

    /// An arbitrary [`Value`] expression tree, bounded to `depth` levels of `Call`/`Bin` nesting.
    fn arb_value(depth: u32) -> proptest::strategy::BoxedStrategy<Value> {
        let leaf = prop_oneof![
            arb_data_string().prop_map(Value::str),
            any::<i64>().prop_map(|n| Value::Int { n }),
            any::<bool>().prop_map(|b| Value::Bool { b }),
            arb_ctx_field().prop_map(|field| Value::Ctx { field }),
            arb_ident().prop_map(|name| Value::Var { name }),
        ];
        if depth == 0 {
            return leaf.boxed();
        }
        prop_oneof![
            3 => leaf,
            1 => (arb_value(depth - 1), arb_ident(), proptest::collection::vec(arb_value(depth - 1), 0..2))
                .prop_map(|(recv, method, args)| Value::Call { recv: Box::new(recv), method, args }),
            1 => (arb_bin_op(), arb_value(depth - 1), arb_value(depth - 1))
                .prop_map(|(op, l, r)| Value::Bin { op, left: Box::new(l), right: Box::new(r) }),
        ]
        .boxed()
    }

    /// A leaf (non-flow) [`MacroNode`] — every action arm, plus `SetVar`/`Stop`. [`MacroNode::Raw`]
    /// is deliberately EXCLUDED: it is verbatim, unvalidated source, so an arbitrary string there
    /// could easily be invalid Python — a property this generator cannot honor (it must always
    /// produce a tree whose codegen reparses successfully).
    fn arb_leaf_node() -> proptest::strategy::BoxedStrategy<MacroNode> {
        let v = || arb_value(2);
        prop_oneof![
            (v(), any::<bool>(), proptest::option::of(arb_data_string()))
                .prop_map(|(text, ghost, speed)| MacroNode::Type { text, ghost, speed }),
            proptest::collection::vec(arb_data_string(), 0..3).prop_map(|keys| MacroNode::Press { keys }),
            arb_data_string().prop_map(|name| MacroNode::KeyPress { name }),
            arb_data_string().prop_map(|button| MacroNode::Click { button }),
            v().prop_map(|amount| MacroNode::Scroll { amount }),
            (v(), v()).prop_map(|(x, y)| MacroNode::MoveTo { x, y }),
            v().prop_map(|text| MacroNode::Copy { text }),
            Just(MacroNode::Paste),
            (v(), proptest::option::of(arb_ident()))
                .prop_map(|(command, capture)| MacroNode::Open { command, capture }),
            v().prop_map(|window| MacroNode::Focus { window }),
            v().prop_map(|ms| MacroNode::Wait { ms }),
            v().prop_map(|text| MacroNode::Notify { text }),
            (arb_ident(), v()).prop_map(|(name, value)| MacroNode::SetVar { name, value }),
            Just(MacroNode::Stop),
        ]
        .boxed()
    }

    /// An arbitrary [`MacroNode`], bounded to `depth` levels of flow nesting (`if`/loops/`try`/`ask`)
    /// around leaf actions.
    fn arb_node(depth: u32) -> proptest::strategy::BoxedStrategy<MacroNode> {
        if depth == 0 {
            return arb_leaf_node();
        }
        let body = |d: u32| proptest::collection::vec(arb_node(d), 1..3);
        let v = || arb_value(1);
        prop_oneof![
            4 => arb_leaf_node(),
            1 => (v(), v(), body(depth - 1), body(depth - 1))
                .prop_map(|(question, description, yes, no)| MacroNode::Ask { question, description, yes, no }),
            1 => (v(), body(depth - 1), body(depth - 1))
                .prop_map(|(cond, then_, else_)| MacroNode::If { cond, then_, else_ }),
            1 => (v(), body(depth - 1)).prop_map(|(count, body)| MacroNode::RepeatN { count, body }),
            1 => (v(), body(depth - 1)).prop_map(|(cond, body)| MacroNode::RepeatWhile { cond, body }),
            1 => (arb_ident(), v(), body(depth - 1))
                .prop_map(|(var, source, body)| MacroNode::ForEach { var, source, body }),
            1 => (body(depth - 1), body(depth - 1))
                .prop_map(|(body, except_)| MacroNode::Try { body, except_ }),
        ]
        .boxed()
    }

    proptest::proptest! {
        // Each case round-trips through the live CPython sidecar TWICE (real subprocess IPC, not a
        // pure-Rust check) — bounded lower than the house default (128-256) so the suite stays fast;
        // the sidecar's own load-stress test (`sidecar_parse_load_stress`) already proves per-parse
        // latency is sub-millisecond once warm, so 40 cases is still a meaningful sweep, not a token one.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(40))]

        /// nodes -> source -> parse -> source' -> parse' reaches a stable fixed point: the SECOND
        /// parse agrees with the first (`p0 == p1`) and codegen from there on is byte-identical
        /// (`codegen(p1) == codegen(p0)`'s regenerated source). Every generated tree must also parse
        /// successfully both times — a parse failure on a tree this generator can only build from
        /// modelled, syntactically-valid constructs would itself be a bug.
        #[test]
        fn codegen_parse_codegen_is_stable(nodes in proptest::collection::vec(arb_node(3), 1..4)) {
            // Serialize against the sidecar-killing death-race tests: they taskkill the shared
            // singleton, which would surface here as a spurious parse failure mid-round-trip.
            let _sidecar = crate::macros::macro_host::SIDECAR_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let host = macro_host();
            if !host.available() {
                return Ok(()); // no bundled python runtime in this environment -> skip cleanly
            }
            let s0 = nodes_to_source(&nodes);
            let p0 = match host.parse_macro(&s0) {
                Ok(p) => p,
                Err(e) => {
                    return Err(proptest::test_runner::TestCaseError::fail(format!(
                        "generated tree failed to parse: {e}\nsrc:\n{s0}"
                    )));
                }
            };
            let s1 = nodes_to_source(&p0);
            let p1 = match host.parse_macro(&s1) {
                Ok(p) => p,
                Err(e) => {
                    return Err(proptest::test_runner::TestCaseError::fail(format!(
                        "re-parse of the first-pass source failed: {e}\nsrc:\n{s1}"
                    )));
                }
            };
            proptest::prop_assert_eq!(&p0, &p1, "parse must be stable at the second cycle\nsrc:\n{}", s1);
            let s2 = nodes_to_source(&p1);
            proptest::prop_assert_eq!(s1, s2, "codegen must reach a byte-identical fixed point after one normalization pass");
        }
    }
}
