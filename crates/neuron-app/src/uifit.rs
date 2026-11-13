// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Layout rules checked on the live element tree of a drawn window, in whatever state the test
//! put it in. `uilint` reads the sources and catches shapes that are wrong in every state; this
//! catches the ones that only go wrong with particular data, such as a name long enough to push
//! a row past its column.
//!
//! - **overflow**: a child of a layout lies outside the layout's box. A layout only places a child
//!   there when its contents need more room than it was given, so whatever comes after it is pushed
//!   off the edge or cut by a clip.
//!
//! - **clipped text**: a label that needs more width than it has and neither elides nor wraps, or
//!   wrapped text with more lines than its height holds. The cut lands mid-glyph.
//! - **dead touch**: a touch area with no size, or one a later sibling's touch area covers
//!   completely, so it never gets a press.
//! - **ghost touch**: an enabled touch area under an element faded to nothing; it catches presses
//!   meant for what shows beneath it.
//!
//! Warnings, written to `target/uifit-warnings.txt` (never a failure):
//!
//! - **loose**: a button whose label has far more room either side than buttons of its height
//!   usually give.
//! - **soft hairline**: a 1-2 px rule on a fractional position, drawn blurred across two pixels.
//! - **faint text**: text under 1.6:1 against the most common colour behind it in the frame.
//! - **cut off**: a touch area, text or capsule partly past the edge of an ancestor that clips it
//!   (scrolling aside), so part of it cannot be seen or pressed.
//! - **roomy field**: a `Field` drawn far wider than the longest value it declares it can hold
//!   (`fits`), so every legal state sits in a box mostly empty.
//!
//! The walk needs the element debug info that the build emits for debug builds (see `build.rs`);
//! without it every element reads as an unnamed non-layout and nothing is checked.

use slint::private_unstable_api::re_exports::WindowInner;

/// Rounding in layout maths, in logical pixels.
const SLACK: f32 = 0.5;

/// What breaks the rules in the window as last laid out, one line each.
pub(crate) fn misfits(window: &slint::Window) -> Vec<String> {
    let Some(root) = WindowInner::from_pub(window).window_item_rc() else { return Vec::new() };
    let mut found = Vec::new();
    let mut stack = vec![(root, String::new())];
    while let Some((item, path)) = stack.pop() {
        let infos: Vec<(String, String, Option<String>)> = (0..item.element_count().unwrap_or(0))
            .map(|i| {
                let (ty, id) = item.element_type_names_and_ids(i).and_then(|v| v.into_iter().next()).unwrap_or_default();
                (ty.to_string(), id.to_string(), item.element_layout_kind(i).map(|k| k.to_string()))
            })
            .collect();
        let name = infos.iter().find(|(_, id, _)| !id.is_empty()).map(|(_, id, _)| id.clone())
            .or_else(|| infos.first().map(|(ty, _, _)| ty.clone()))
            .unwrap_or_default();
        let path = match (path.is_empty(), name.is_empty()) {
            (_, true) => path,
            (true, false) => name,
            (false, false) => format!("{path} > {name}"),
        };
        let layout = infos.iter().find_map(|(_, _, kind)| kind.clone());
        let bounds = item.geometry();
        let checked = layout.is_some() && item.is_visible() && bounds.size.width > 0.0 && bounds.size.height > 0.0;
        let mut child = item.first_child();
        while let Some(c) = child {
            let g = c.geometry();
            let outside = g.origin.x < -SLACK
                || g.origin.y < -SLACK
                || g.max_x() > bounds.size.width + SLACK
                || g.max_y() > bounds.size.height + SLACK;
            if checked && outside && g.size.width > 0.0 && g.size.height > 0.0 {
                found.push(format!(
                    "{path}: a {} child at ({:.1}, {:.1}) size {:.1}×{:.1} is outside its {:.1}×{:.1} box",
                    layout.as_deref().unwrap_or("layout"),
                    g.origin.x, g.origin.y, g.size.width, g.size.height, bounds.size.width, bounds.size.height,
                ));
            }
            child = c.next_sibling();
            stack.push((c, path.clone()));
        }
    }
    found
}

/// A button's label sits in room it does not use: more than twice the side padding buttons of its
/// height usually give, and at least this much more. A warning, never a failure: padding is taste,
/// but a key far roomier than its neighbours usually stretched by accident.
const LOOSE_OVER: f32 = 12.0;

/// Buttons in `window` padded far past the norm for their height: an element with a touch area
/// of its own and one label, shorter than 40 px. One line each.
pub(crate) fn loose(window: &slint::Window) -> Vec<String> {
    use slint::private_unstable_api::re_exports::Orientation;
    let inner = WindowInner::from_pub(window);
    let Some(root) = inner.window_item_rc() else { return Vec::new() };
    let adapter = inner.window_adapter();
    let types = |count: Option<usize>, at: &dyn Fn(usize) -> Option<String>| (0..count.unwrap_or(0)).filter_map(at).collect::<Vec<String>>();
    // (path, height, side padding)
    let mut keys: Vec<(String, f32, f32)> = Vec::new();
    let mut stack = vec![(root, String::new())];
    while let Some((item, path)) = stack.pop() {
        let mine = types(item.element_count(), &|i| item.element_type_names_and_ids(i).and_then(|v| v.into_iter().next()).map(|(ty, id)| if id.is_empty() { ty.to_string() } else { id.to_string() }));
        let path = match mine.first() {
            Some(name) if path.is_empty() => name.clone(),
            Some(name) => format!("{path} > {name}"),
            None => path,
        };
        let bounds = item.geometry();
        let mut touch = false;
        let mut labels = Vec::new();
        let mut child = item.first_child();
        while let Some(c) = child {
            let kinds = types(c.element_count(), &|i| c.element_type_names_and_ids(i).map(|v| v.iter().map(|(ty, _)| ty.to_string()).collect::<Vec<_>>().join(",")));
            // a key's touch area is the key: one covering only part of the element (a row of
            // capsules beside a caption) does not make the element a key
            let covers = c.geometry().size.width >= bounds.size.width * 0.9;
            touch |= covers && kinds.iter().any(|k| k.split(',').any(|t| t == "TouchArea"));
            // labels within two levels: a key's text usually sits in its own row layout
            let mut near = vec![(c.clone(), 0)];
            while let Some((n, depth)) = near.pop() {
                let kinds = types(n.element_count(), &|i| n.element_type_names_and_ids(i).map(|v| v.iter().map(|(ty, _)| ty.to_string()).collect::<Vec<_>>().join(",")));
                if kinds.iter().any(|k| k.split(',').any(|t| t == "Text")) {
                    labels.push(n.clone());
                } else if depth < 2 {
                    let mut g = n.first_child();
                    while let Some(x) = g {
                        g = x.next_sibling();
                        near.push((x, depth + 1));
                    }
                }
            }
            child = c.next_sibling();
            stack.push((c, path.clone()));
        }
        // a key, not a row: a full-width row that is clickable anywhere (a toggle with its caption
        // at the left) is not padded, it is laid out
        let small = bounds.size.height > 0.0 && bounds.size.height < 40.0 && bounds.size.width > 0.0 && bounds.size.width <= 220.0;
        if touch && small && labels.len() == 1 && item.is_visible() {
            let label = &labels[0];
            let natural = label.borrow().as_ref().layout_info(Orientation::Horizontal, label.geometry().size.height, &adapter, label).preferred;
            if natural > 0.0 {
                keys.push((path, bounds.size.height.round(), ((bounds.size.width - natural) / 2.0).max(0.0)));
            }
        }
    }
    let mut found = Vec::new();
    for (path, height, pad) in &keys {
        let mut same: Vec<f32> = keys.iter().filter(|k| k.1 == *height).map(|k| k.2).collect();
        if same.len() < 3 {
            continue;
        }
        same.sort_by(f32::total_cmp);
        let norm = same[same.len() / 2];
        if *pad > 2.0 * norm && *pad - norm >= LOOSE_OVER {
            found.push(format!("{path}: {pad:.0} px each side of its label; keys {height:.0} px tall usually give {norm:.0}"));
        }
    }
    found
}

/// Add `found` to `target/uifit-warnings.txt`, once each across the test processes, and say how
/// many the file holds.
fn warn(found: Vec<String>) {
    if found.is_empty() {
        return;
    }
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/uifit-warnings.txt");
    let known: std::collections::BTreeSet<String> = std::fs::read_to_string(&file).unwrap_or_default().lines().map(str::to_string).collect();
    let fresh: Vec<&String> = found.iter().filter(|line| !known.contains(*line)).collect();
    if fresh.is_empty() {
        return;
    }
    use std::io::Write;
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        for line in &fresh {
            let _ = writeln!(out, "{line}");
        }
    }
    eprintln!("uifit: {} warning(s) noted in {}", known.len() + fresh.len(), file.display());
}

/// One drawn item, in window coordinates.
struct Node {
    path: String,
    /// Its own rectangle.
    rect: Rect,
    /// The part of the window its clipping ancestors let it draw in.
    room: Rect,
    /// Its parent's index in the walk.
    parent: Option<usize>,
    /// Faded out: it or an ancestor is at (nearly) zero opacity.
    faded: bool,
    /// The colour of its text, when it shows some.
    ink: Option<slint::Color>,
    /// It clips what it draws (a rounded card or capsule).
    clips: bool,
    /// A scrolling view is among the ancestors that clip it: being cut there is scrolling.
    scrolled: bool,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn cut(self, other: Rect) -> Rect {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        Rect { x, y, w: ((self.x + self.w).min(other.x + other.w) - x).max(0.0), h: ((self.y + self.h).min(other.y + other.h) - y).max(0.0) }
    }

    fn area(self) -> f32 {
        self.w * self.h
    }

    fn holds(self, other: Rect) -> bool {
        other.x >= self.x - SLACK && other.y >= self.y - SLACK && other.x + other.w <= self.x + self.w + SLACK && other.y + other.h <= self.y + self.h + SLACK
    }
}

enum Kind {
    Other,
    /// An enabled touch area.
    Touch,
    /// Text that cannot shorten itself (no elide) and does not wrap, with the width it needs.
    Rigid { text: String, needs: f32 },
    /// Wrapped text, with the height its lines need at its width.
    Wrapped { text: String, needs: f32 },
    /// Text that shortens itself or is empty: nothing to measure.
    Text,
    /// A filled rectangle at most this thin (a rule, a tick, a stem).
    Hairline,
}

/// Every item the window draws, in drawing order, with what the checks below need.
fn scan(window: &slint::Window) -> Vec<Node> {
    use slint::private_unstable_api::re_exports::{
        Clip, ComplexText, Flickable, Opacity, Orientation, Rectangle, SimpleText, TextOverflow, TextWrap, TouchArea,
    };
    let inner = WindowInner::from_pub(window);
    let Some(root) = inner.window_item_rc() else { return Vec::new() };
    let adapter = inner.window_adapter();
    let size = window.size().to_logical(window.scale_factor());
    let screen = Rect { x: 0.0, y: 0.0, w: size.width, h: size.height };
    let mut nodes: Vec<Node> = Vec::new();
    // (item, parent index, parent's window origin, the room the parent leaves its children, faded,
    // whether that room was cut by a scrolling view)
    let mut stack = vec![(root, None::<usize>, (0.0f32, 0.0f32), screen, false, false)];
    while let Some((item, parent, origin, room, faded, scrolled)) = stack.pop() {
        let g = item.geometry();
        let rect = Rect { x: origin.0 + g.origin.x, y: origin.1 + g.origin.y, w: g.size.width, h: g.size.height };
        let name = (0..item.element_count().unwrap_or(0))
            .filter_map(|i| item.element_type_names_and_ids(i)?.into_iter().next())
            .map(|(ty, id)| if id.is_empty() { ty.to_string() } else { id.to_string() })
            .next()
            .unwrap_or_default();
        let path = match parent.map(|p| nodes[p].path.as_str()) {
            Some(above) if !name.is_empty() => format!("{above} > {name}"),
            Some(above) => above.to_string(),
            None => name,
        };
        let faded = faded || item.downcast::<Opacity>().is_some_and(|o| o.as_pin_ref().opacity() < 0.02);
        let needs = |orientation, cross: f32| item.borrow().as_ref().layout_info(orientation, cross, &adapter, &item).preferred;
        let mut ink = None;
        let kind = if let Some(t) = item.downcast::<ComplexText>() {
            let t = t.as_pin_ref();
            let text = t.text().to_string();
            ink = (!text.trim().is_empty()).then(|| t.color().color());
            if text.trim().is_empty() || t.overflow() == TextOverflow::Elide {
                Kind::Text
            } else if t.wrap() == TextWrap::NoWrap {
                Kind::Rigid { needs: needs(Orientation::Horizontal, rect.h), text }
            } else {
                Kind::Wrapped { needs: needs(Orientation::Vertical, rect.w), text }
            }
        } else if let Some(t) = item.downcast::<SimpleText>() {
            let t = t.as_pin_ref();
            let text = t.text().to_string();
            ink = (!text.trim().is_empty()).then(|| t.color().color());
            if text.trim().is_empty() { Kind::Text } else { Kind::Rigid { needs: needs(Orientation::Horizontal, rect.h), text } }
        } else if item.downcast::<TouchArea>().is_some_and(|t| t.as_pin_ref().enabled()) {
            Kind::Touch
        } else if item.downcast::<Rectangle>().is_some() && (rect.w <= 2.0 || rect.h <= 2.0) && rect.w > 0.0 && rect.h > 0.0 {
            Kind::Hairline
        } else {
            Kind::Other
        };
        // what this item lets its children draw in
        let scrolls = item.downcast::<Flickable>().is_some();
        let clips = item.downcast::<Clip>().is_some_and(|c| c.as_pin_ref().clip()) || scrolls;
        let inside = if clips { room.cut(rect) } else { room };
        let index = nodes.len();
        nodes.push(Node { path, rect, room, parent, faded, ink, clips: clips && !scrolls, scrolled, kind });
        // children are pushed in reverse so they pop in drawing order
        let mut children = Vec::new();
        let mut child = item.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            children.push(c);
        }
        for c in children.into_iter().rev() {
            stack.push((c, Some(index), (rect.x, rect.y), inside, faded, scrolled || scrolls));
        }
    }
    nodes
}

/// Whether `node` can be seen at all: not faded, not clipped away, not empty.
fn shown(node: &Node) -> bool {
    !node.faded && node.rect.cut(node.room).area() > 1.0
}

/// Text that is cut instead of shortened: a label that needs more width than it has and neither
/// elides nor wraps, or wrapped text with more lines than its height holds. The cut falls
/// mid-glyph wherever the data happens to end.
fn clipped_text(nodes: &[Node]) -> Vec<String> {
    let mut found = Vec::new();
    for node in nodes.iter().filter(|n| shown(n)) {
        match &node.kind {
            Kind::Rigid { text, needs } if *needs > node.rect.w + 1.0 => {
                found.push(format!("{}: \"{}\" needs {needs:.0} px, has {:.0}, and neither elides nor wraps", node.path, short(text), node.rect.w));
            }
            Kind::Wrapped { text, needs } if *needs > node.rect.h + 1.0 => {
                found.push(format!("{}: \"{}\" wraps to {needs:.0} px tall in {:.0}", node.path, short(text), node.rect.h));
            }
            _ => {}
        }
    }
    found
}

/// Touch areas that can never be reached: one covered completely by a later sibling's touch area
/// (the later one takes every press first), or one with no size.
fn dead_touch(nodes: &[Node]) -> Vec<String> {
    let touches: Vec<usize> = (0..nodes.len()).filter(|&i| matches!(nodes[i].kind, Kind::Touch) && shown(&nodes[i])).collect();
    let mut found = Vec::new();
    for &i in &touches {
        let a = &nodes[i];
        if a.rect.w < 1.0 || a.rect.h < 1.0 {
            found.push(format!("{}: a touch area {:.0}×{:.0} px takes no press", a.path, a.rect.w, a.rect.h));
            continue;
        }
        let covered = touches.iter().find(|&&j| j > i && nodes[j].parent == a.parent && nodes[j].rect.holds(a.rect));
        if let Some(&j) = covered {
            found.push(format!("{}: covered by its later sibling {}, which takes every press first", a.path, nodes[j].path));
        }
    }
    found
}

/// Touch areas that take presses while invisible: under an element faded to nothing, they catch
/// clicks meant for what shows beneath.
fn ghost_touch(nodes: &[Node]) -> Vec<String> {
    nodes.iter()
        .filter(|n| matches!(n.kind, Kind::Touch) && n.faded && n.rect.cut(n.room).area() > 1.0)
        .map(|n| format!("{}: takes presses while faded to nothing", n.path))
        .collect()
}

/// Thin rules placed between pixels: a 1-2 px line on a fractional position is drawn soft across
/// two rows of pixels.
fn soft_hairlines(nodes: &[Node], scale: f32) -> Vec<String> {
    let off = |v: f32| ((v * scale).fract() - 0.5).abs() < 0.4;
    // the element the fraction starts at: the highest ancestor still off the pixel grid on that axis
    let source = |mut i: usize, vertical: bool| {
        let at = |n: &Node| if vertical { n.rect.y } else { n.rect.x };
        while let Some(p) = nodes[i].parent.filter(|&p| off(at(&nodes[p]))) {
            i = p;
        }
        i
    };
    let mut causes: std::collections::BTreeMap<(usize, bool), usize> = std::collections::BTreeMap::new();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| matches!(n.kind, Kind::Hairline) && shown(n)) {
        if n.rect.w <= 2.0 && off(n.rect.x) {
            *causes.entry((source(i, false), false)).or_default() += 1;
        }
        if n.rect.h <= 2.0 && off(n.rect.y) {
            *causes.entry((source(i, true), true)).or_default() += 1;
        }
    }
    causes.into_iter().map(|((i, vertical), count)| {
        let n = &nodes[i];
        let (axis, at) = if vertical { ("y", n.rect.y) } else { ("x", n.rect.x) };
        format!("{}: starts at {axis} = {at:.2}, between pixels, so {count} thin rule(s) inside it draw blurred", n.path)
    }).collect()
}

/// Text too close in lightness to what is drawn behind it, read off the frame: the most common
/// colour in the text's box is its ground. Below 1.6:1 it is hard to read at all.
fn faint_text(nodes: &[Node], frame: &slint::SharedPixelBuffer<slint::Rgba8Pixel>) -> Vec<String> {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let px = frame.as_slice();
    let luminance = |r: u8, g: u8, b: u8| {
        let lin = |c: u8| { let c = f32::from(c) / 255.0; if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) } };
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    };
    let mut found = Vec::new();
    for node in nodes.iter().filter(|n| shown(n)) {
        let Some(colour) = node.ink else { continue };
        if colour.alpha() < 200 {
            continue;
        }
        let r = node.rect.cut(node.room);
        let (x0, y0) = (r.x.max(0.0) as usize, r.y.max(0.0) as usize);
        let (x1, y1) = (((r.x + r.w) as usize).min(fw), ((r.y + r.h) as usize).min(fh));
        if x1 <= x0 + 2 || y1 <= y0 + 2 {
            continue;
        }
        let mut counts: std::collections::HashMap<(u8, u8, u8), u32> = std::collections::HashMap::new();
        for y in (y0..y1).step_by(2) {
            for x in (x0..x1).step_by(2) {
                let p = px[y * fw + x];
                *counts.entry((p.r & 0xF8, p.g & 0xF8, p.b & 0xF8)).or_default() += 1;
            }
        }
        let Some((&(br, bg, bb), _)) = counts.iter().max_by_key(|(_, n)| **n) else { continue };
        let (a, b) = (luminance(colour.red(), colour.green(), colour.blue()), luminance(br, bg, bb));
        let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
        if ratio < 1.6 {
            found.push(format!("{}: text at {ratio:.2}:1 against its ground", node.path));
        }
    }
    found
}

/// Fields drawn far wider than anything they can hold: a `Field` declares its longest legal value
/// in `fits` (measured by its hidden `fit` text), and every legal state is then known to sit
/// inside this much less room than it was given.
fn roomy_fields(nodes: &[Node]) -> Vec<String> {
    const PADDING: f32 = 24.0;
    const SPARE: f32 = 24.0;
    nodes.iter()
        .filter(|n| n.path.ends_with("Field::fit") && n.rect.w > 0.0)
        .filter_map(|fit| {
            let field = &nodes[fit.parent?];
            let spare = field.rect.w - (fit.rect.w + PADDING);
            (spare > SPARE && field.rect.cut(field.room).area() > 1.0).then(|| {
                format!("{}: {:.0} px wide, but its longest legal value needs {:.0}", field.path, field.rect.w, fit.rect.w + PADDING)
            })
        })
        .collect()
}

/// Things cut off by an ancestor that clips: a touch area, a line of text or a capsule partly
/// outside the room it is given (scrolling aside). The part past the edge is not there to see or
/// press: a row that grew past its column, a label pushed out of its box.
fn cut_off(nodes: &[Node]) -> Vec<String> {
    nodes.iter()
        .filter(|n| !n.faded && !n.scrolled && (matches!(n.kind, Kind::Touch) || n.ink.is_some() || n.clips))
        .filter_map(|n| {
            let seen = n.rect.cut(n.room).area();
            let whole = n.rect.area();
            (seen > 1.0 && seen < whole * 0.97 && whole - seen > 16.0).then(|| {
                format!("{}: {:.0}% of it is cut off by an ancestor's edge", n.path, 100.0 * (1.0 - seen / whole))
            })
        })
        .collect()
}

/// Where the element whose path ends with `name` is drawn, in window coordinates: (x, y, w, h).
pub(crate) fn rect_of(window: &slint::Window, name: &str) -> Option<(f32, f32, f32, f32)> {
    scan(window).into_iter().find(|n| n.path.ends_with(name) && shown(n)).map(|n| (n.rect.x, n.rect.y, n.rect.w, n.rect.h))
}

/// How much of the window blinks across three frames taken a moment apart with nothing done in
/// between: pixels that change and then change back (a and c alike, b different). A hover fade
/// or a running animation moves one way and does not count; a flicker does. 0..1 of the window.
pub(crate) fn blink(a: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, b: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, c: &slint::SharedPixelBuffer<slint::Rgba8Pixel>) -> f32 {
    if a.width() != b.width() || b.width() != c.width() || a.height() != c.height() || a.height() != b.height() {
        return 1.0;
    }
    let far = |p: &slint::Rgba8Pixel, q: &slint::Rgba8Pixel| {
        (i32::from(p.r) - i32::from(q.r)).abs() + (i32::from(p.g) - i32::from(q.g)).abs() + (i32::from(p.b) - i32::from(q.b)).abs()
    };
    let (a, b, c) = (a.as_slice(), b.as_slice(), c.as_slice());
    let blinking = (0..a.len()).filter(|&i| far(&a[i], &c[i]) <= 12 && far(&a[i], &b[i]) >= 60).count();
    blinking as f32 / a.len().max(1) as f32
}

fn short(text: &str) -> String {
    let mut out: String = text.chars().take(32).collect();
    if text.chars().count() > 32 {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// Panic with every misfit in `window`, naming the state in `when`: layout that does not fit,
/// text that is cut, and touch areas that can never be reached or take presses unseen.
pub(crate) fn assert_fits(window: &slint::Window, when: &str) {
    let nodes = scan(window);
    let mut found = misfits(window);
    found.extend(clipped_text(&nodes));
    found.extend(dead_touch(&nodes));
    found.extend(ghost_touch(&nodes));
    assert!(found.is_empty(), "{when}: UI that cannot be right in this state (see src/uifit.rs):\n  {}", found.join("\n  "));
}

/// Note what is worth a look in `window` as drawn in `frame`: loosely padded keys, rules between
/// pixels, and text hard to read against its ground.
pub(crate) fn note(window: &slint::Window, frame: &slint::SharedPixelBuffer<slint::Rgba8Pixel>) {
    let nodes = scan(window);
    let mut found = loose(window);
    found.extend(soft_hairlines(&nodes, window.scale_factor()));
    found.extend(faint_text(&nodes, frame));
    found.extend(roomy_fields(&nodes));
    found.extend(cut_off(&nodes));
    warn(found);
}
