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
            format!("{}.{method}({args})", value_to_source(recv))
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
