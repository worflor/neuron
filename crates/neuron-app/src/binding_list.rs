// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The direct binding view keeps source indices so filtered edits still address the same rule.

use crate::ui::{AppWindow, RuleRow, State};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

const PAGE_SIZE: usize = 8;

struct Page {
    indices: Vec<i32>,
    number: usize,
    matches: usize,
}

fn select(rows: &[String], query: &str, page: usize, editing: Option<usize>) -> Page {
    let query = query.to_lowercase();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let matches: Vec<usize> = rows.iter().enumerate()
        .filter_map(|(i, text)| {
            let text = text.to_lowercase();
            tokens.iter().all(|word| text.contains(word)).then_some(i)
        })
        .collect();
    let last = matches.len().saturating_sub(1) / PAGE_SIZE;
    let number = editing.and_then(|row| matches.iter().position(|i| *i == row))
        .map_or(page.min(last), |position| position / PAGE_SIZE);
    Page {
        indices: matches.iter().skip(number * PAGE_SIZE).take(PAGE_SIZE).map(|i| *i as i32).collect(),
        number,
        matches: matches.len(),
    }
}

fn search_text(row: &RuleRow) -> String {
    format!("{} {} {} {} {} {} {} {}", row.trigger, row.control, row.device, row.pid,
        row.action, row.layer, row.kind, if row.removable { "editable" } else { "source" })
}

fn update_indices(existing: ModelRc<i32>, indices: Vec<i32>) -> ModelRc<i32> {
    if let Some(model) = existing.as_any().downcast_ref::<VecModel<i32>>() {
        while model.row_count() > indices.len() { model.remove(model.row_count() - 1); }
        for (i, row) in indices.into_iter().enumerate() {
            if i == model.row_count() { model.push(row); }
            else if model.row_data(i) != Some(row) { model.set_row_data(i, row); }
        }
        return existing;
    }
    ModelRc::new(VecModel::from(indices))
}

pub fn refresh(app: &AppWindow, hyper: bool) {
    let st = app.global::<State>();
    let rows = if hyper { st.get_hypershift_rules() } else { st.get_rules() };
    let text: Vec<String> = rows.iter().map(|row| search_text(&row)).collect();
    let (query, page, indices) = if hyper {
        (st.get_hyper_rule_query(), st.get_hyper_rule_page(), st.get_hyper_rule_indices())
    } else {
        (st.get_rule_query(), st.get_rule_page(), st.get_rule_indices())
    };
    let editing = (st.get_editing_rule() >= 0 && st.get_editing_rule_hyper() == hyper)
        .then_some(st.get_editing_rule() as usize);
    let result = select(&text, query.as_str(), page.max(0) as usize, editing);
    let indices = update_indices(indices, result.indices);
    if hyper {
        st.set_hyper_rule_indices(indices);
        st.set_hyper_rule_page(result.number as i32);
        st.set_hyper_rule_matches(result.matches as i32);
    } else {
        st.set_rule_indices(indices);
        st.set_rule_page(result.number as i32);
        st.set_rule_matches(result.matches as i32);
    }
}

pub fn install(app: &AppWindow) {
    let weak = app.as_weak();
    app.global::<State>().on_filter_rules(move |query, hyper| {
        let Some(app) = weak.upgrade() else { return };
        let st = app.global::<State>();
        if st.get_editing_rule() >= 0 { return; }
        if hyper { st.set_hyper_rule_query(query); st.set_hyper_rule_page(0); }
        else { st.set_rule_query(query); st.set_rule_page(0); }
        refresh(&app, hyper);
    });
    let weak = app.as_weak();
    app.global::<State>().on_page_rules(move |direction, hyper| {
        let Some(app) = weak.upgrade() else { return };
        let st = app.global::<State>();
        if st.get_editing_rule() >= 0 { return; }
        let page = if hyper { st.get_hyper_rule_page() } else { st.get_rule_page() };
        let next = page.saturating_add(direction.clamp(-1, 1)).max(0);
        if hyper { st.set_hyper_rule_page(next); } else { st.set_rule_page(next); }
        refresh(&app, hyper);
    });
    let weak = app.as_weak();
    app.global::<State>().on_reset_rule_view(move |hyper| {
        let Some(app) = weak.upgrade() else { return };
        let st = app.global::<State>();
        if hyper { st.set_hyper_rule_query("".into()); st.set_hyper_rule_page(0); }
        else { st.set_rule_query("".into()); st.set_rule_page(0); }
        refresh(&app, hyper);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtering_and_deletion_preserve_source_indices_and_clamp_page() {
        let mut rows: Vec<String> = (0..25).map(|i| format!("mouse {} {}", i, if i % 2 == 0 { "copy" } else { "paste" })).collect();
        let filtered = select(&rows, "MOUSE copy", 1, None);
        assert_eq!(filtered.indices, vec![16, 18, 20, 22, 24]);
        assert_eq!(filtered.matches, 13);
        rows.truncate(8);
        let shrunk = select(&rows, "", 99, None);
        assert_eq!(shrunk.number, 0);
        assert_eq!(shrunk.indices, (0..8).collect::<Vec<i32>>());
        assert!(select(&rows, "missing", 1, None).indices.is_empty());
    }

    #[test]
    fn an_open_editor_stays_on_its_source_page() {
        let rows = vec!["keyboard bind".to_string(); 30];
        let page = select(&rows, "bind", 0, Some(19));
        assert_eq!(page.number, 2);
        assert_eq!(page.indices, (16..24).collect::<Vec<i32>>());
    }

    #[test]
    fn identical_indices_keep_the_live_model() {
        let model = ModelRc::new(VecModel::from(vec![8, 11]));
        let same = update_indices(model.clone(), vec![8, 11]);
        assert_eq!(model, same);
        assert_eq!(update_indices(same, vec![8, 12]).row_data(1), Some(12));
    }
}
