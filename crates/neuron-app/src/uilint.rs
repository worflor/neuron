// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Layout rules the Slint compiler does not enforce, checked over the `.slint` sources.
//!
//! Slint draws an element wherever its geometry says, including past its parent, and draws text at
//! its full width unless told otherwise. Both compile, and both show up only when the right data
//! reaches the screen. These rules reject the two shapes that produced such bugs:
//!
//! - **bleed**: an element placed outside its parent (`x`/`y` that starts negative, or a size of
//!   `parent.width + …`). Whatever clips the parent cuts it. `x: -parent.x` is exempt: it cancels
//!   the parent's own offset, which is how a moving clip window shows a fixed child.
//! - **loose-text**: text that is positioned by hand (`x:`), shows data instead of a fixed string,
//!   and has no `width`, `overflow` or `wrap`. Nothing bounds it, so a long value runs on.
//!
//! A deliberate exception carries `// ui-lint: allow(<rule>) <reason>` on the offending line or
//! within the six lines above it.

use std::path::{Path, PathBuf};

const TEXT_ELEMENTS: &[&str] = &["Text", "Mono", "Label", "SubLabel", "SectionTitle"];
const ALLOW_REACH: usize = 6;

#[derive(Debug, PartialEq)]
struct Finding {
    line: usize,
    rule: &'static str,
    what: String,
}

/// Every `name: value;` on a line, ignoring a trailing comment. Several may share a line.
fn properties(line: &str) -> impl Iterator<Item = (&str, &str)> {
    let code = line.split("//").next().unwrap_or("");
    code.split(';').filter_map(|statement| {
        let (name, value) = statement.split_once(':')?;
        let name = name.trim();
        let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        plain.then(|| (name, value.trim()))
    })
}

/// The element type a line opens, for `Type {`, `id := Type {` and `if cond : [id :=] Type {`.
fn opened_element(line: &str) -> Option<&str> {
    let code = line.split("//").next().unwrap_or("").trim();
    let head = code.strip_suffix('{')?.trim();
    let head = head.rsplit(" : ").next().unwrap_or(head);
    let head = head.rsplit(":=").next().unwrap_or(head).trim();
    let simple = !head.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric());
    (simple && head.starts_with(|c: char| c.is_ascii_uppercase())).then_some(head)
}

fn allowed(lines: &[&str], at: usize, rule: &str) -> bool {
    let marker = format!("ui-lint: allow({rule})");
    lines[at.saturating_sub(ALLOW_REACH)..=at].iter().any(|l| l.contains(&marker))
}

fn lint(source: &str) -> Vec<Finding> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for (name, value) in properties(line) {
            let outside = match name {
                "x" | "y" => value.starts_with('-') && value != "-parent.x" && value != "-parent.y",
                "width" => value.starts_with("parent.width +"),
                "height" => value.starts_with("parent.height +"),
                _ => false,
            };
            if outside && !allowed(&lines, i, "bleed") {
                found.push(Finding { line: i + 1, rule: "bleed", what: format!("{name}: {value}") });
            }
        }
        if !opened_element(line).is_some_and(|ty| TEXT_ELEMENTS.contains(&ty)) {
            continue;
        }
        // the element's own properties: lines at depth 1 until its closing brace
        let (mut depth, mut placed, mut bounded, mut data) = (1i32, false, false, None);
        for own in &lines[i + 1..] {
            for (name, value) in properties(own).filter(|_| depth == 1) {
                match name {
                    "x" => placed = true,
                    "width" | "max-width" | "overflow" | "wrap" => bounded = true,
                    "text" => {
                        let fixed = value.len() >= 2 && value.starts_with('"') && value[1..].find('"') == Some(value.len() - 2);
                        if !fixed {
                            data = Some(value.to_string());
                        }
                    }
                    _ => {}
                }
            }
            depth += own.matches('{').count() as i32 - own.matches('}').count() as i32;
            if depth <= 0 {
                break;
            }
        }
        if let (true, false, Some(text)) = (placed, bounded, data) {
            if !allowed(&lines, i, "loose-text") {
                found.push(Finding { line: i + 1, rule: "loose-text", what: format!("text: {text}") });
            }
        }
    }
    found
}

fn slint_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the ui directory is readable").flatten() {
        let path = entry.path();
        if path.is_dir() {
            slint_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "slint") {
            out.push(path);
        }
    }
}

#[test]
fn ui_sources_keep_elements_and_text_inside_their_bounds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut files = Vec::new();
    slint_sources(&root, &mut files);
    assert!(files.len() > 5, "found the ui sources");
    let mut report = String::new();
    for file in files {
        let source = std::fs::read_to_string(&file).expect("a ui source is UTF-8");
        for f in lint(&source) {
            let shown = file.strip_prefix(&root).unwrap_or(&file).display();
            report.push_str(&format!("\n  ui/{shown}:{}  {}  {}", f.line, f.rule, f.what));
        }
    }
    assert!(
        report.is_empty(),
        "layout that can draw outside its bounds (see src/uilint.rs for the rules and the allow marker):{report}"
    );
}

#[test]
fn the_rules_catch_the_shapes_they_name() {
    let rules = |src: &str| lint(src).into_iter().map(|f| (f.line, f.rule)).collect::<Vec<_>>();
    assert_eq!(rules("Rectangle {\n    x: -4px; width: parent.width + 8px;\n}"), [(2, "bleed"), (2, "bleed")]);
    assert_eq!(rules("Rectangle {\n    width: parent.width + 8px;\n    y: -self.grow;\n}"), [(2, "bleed"), (3, "bleed")]);
    assert_eq!(rules("Rectangle {\n    x: parent.width - 4px;\n    width: parent.width - 8px;\n}"), []);
    assert_eq!(rules("// ui-lint: allow(bleed) a halo\nRectangle {\n    x: -4px;\n}"), []);
    assert_eq!(rules("Rectangle {\n    clip: true;\n    Text {\n        x: -parent.x;\n        width: root.width;\n    }\n}"), []);
    assert_eq!(rules("Mono {\n    x: 4px; width: 80px;\n    text: root.name;\n}"), [], "properties sharing a line");

    let loose = "lbl := Mono {\n    x: 4px;\n    text: \"with \" + root.name;\n}";
    assert_eq!(rules(loose), [(1, "loose-text")]);
    assert_eq!(rules("if root.on : Text {\n    x: 4px;\n    text: root.name;\n}"), [(1, "loose-text")]);
    assert_eq!(rules("Mono {\n    x: 4px;\n    text: root.name;\n    overflow: elide;\n}"), []);
    assert_eq!(rules("Mono {\n    x: 4px;\n    text: root.name;\n    width: 80px;\n}"), []);
    assert_eq!(rules("Mono {\n    x: 4px;\n    text: \"fixed\";\n}"), []);
    assert_eq!(rules("Mono {\n    text: root.name;\n}"), [], "a layout sizes text it places");
    assert_eq!(rules("Rectangle {\n    x: 4px;\n    Mono {\n        text: root.name;\n    }\n}"), []);
    assert_eq!(rules("// ui-lint: allow(loose-text) sized by its own width\nMono {\n    x: 4px;\n    text: root.name;\n}"), []);
}
