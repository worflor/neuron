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

/// Panic with every misfit in `window`, naming the state in `when`.
pub(crate) fn assert_fits(window: &slint::Window, when: &str) {
    let found = misfits(window);
    assert!(found.is_empty(), "{when}: layout that does not fit its space (see src/uifit.rs):\n  {}", found.join("\n  "));
}
