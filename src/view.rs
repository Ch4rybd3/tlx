//! Per-tab view state: column layout, filters, sort, grouping, tags and selection.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use rayon::prelude::*;

use crate::data::{ColType, Table, cmp_ci};
use crate::filter::{
    ColumnFilter, CompiledSearch, FilterNode, Pred, SearchBehavior, SearchOptions, compile_column_filter, compile_node,
};
use crate::format::{CompiledRule, CondRule, compile_rules};
use crate::grid::GridState;

#[derive(Clone, Debug)]
pub struct ColumnState {
    pub col: usize,
    pub visible: bool,
    pub width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disp {
    Row(u32),
    Group(u32),
}

pub struct GroupInfo {
    pub level: usize,
    pub col: usize,
    pub value: String,
    pub count: usize,
    pub key: String,
    pub collapsed: bool,
}

pub struct TabView {
    pub table: Arc<Table>,
    pub columns: Vec<ColumnState>,
    /// Number of visible data columns pinned on the left ("first scrollable column").
    pub frozen: usize,
    /// (data column, descending)
    pub sort: Vec<(usize, bool)>,
    pub group_by: Vec<usize>,
    pub collapsed: HashSet<String>,
    pub col_filters: BTreeMap<usize, ColumnFilter>,
    pub filter_tree: FilterNode,
    pub filters_enabled: bool,
    pub search: String,
    pub search_opts: SearchOptions,
    pub tagged_only: bool,
    pub tags: Vec<bool>,
    pub tag_count: usize,
    pub rules: Vec<CondRule>,

    // Derived
    pub filtered: Vec<u32>,
    pub display: Vec<Disp>,
    pub groups: Vec<GroupInfo>,
    pub compiled_search: Option<Arc<CompiledSearch>>,
    pub compiled_rules: Vec<CompiledRule>,
    pub errors: Vec<String>,
    pub rule_errors: Vec<String>,
    pub dirty: bool,
    pub rules_dirty: bool,
    pub last_refresh_ms: u128,

    // Selection / scrolling
    pub selected: Vec<bool>,
    pub sel_count: usize,
    pub cursor: usize,
    pub cursor_col: usize,
    pub anchor: usize,
    pub top: usize,
    pub h_offset: f32,
    pub scroll_to_cursor: bool,
    pub ui: GridState,
}

impl TabView {
    pub fn new(table: Arc<Table>) -> Self {
        let n = table.nrows;
        let columns = (0..table.ncols()).map(|c| ColumnState { col: c, visible: true, width: 120.0 }).collect();
        let mut v = Self {
            table,
            columns,
            frozen: 0,
            sort: vec![],
            group_by: vec![],
            collapsed: HashSet::new(),
            col_filters: BTreeMap::new(),
            filter_tree: FilterNode::default(),
            filters_enabled: true,
            search: String::new(),
            search_opts: SearchOptions::default(),
            tagged_only: false,
            tags: vec![false; n],
            tag_count: 0,
            rules: vec![],
            filtered: vec![],
            display: vec![],
            groups: vec![],
            compiled_search: None,
            compiled_rules: vec![],
            errors: vec![],
            rule_errors: vec![],
            dirty: true,
            rules_dirty: true,
            last_refresh_ms: 0,
            selected: vec![false; n],
            sel_count: 0,
            cursor: 0,
            cursor_col: 0,
            anchor: 0,
            top: 0,
            h_offset: 0.0,
            scroll_to_cursor: false,
            ui: GridState::default(),
        };
        v.ui.needs_autofit = true;
        v
    }

    pub fn visible_cols(&self) -> Vec<usize> {
        self.columns.iter().filter(|c| c.visible).map(|c| c.col).collect()
    }

    pub fn column_state(&mut self, col: usize) -> Option<&mut ColumnState> {
        self.columns.iter_mut().find(|c| c.col == col)
    }

    pub fn cursor_row(&self) -> Option<usize> {
        match self.display.get(self.cursor) {
            Some(Disp::Row(r)) => Some(*r as usize),
            _ => None,
        }
    }

    pub fn has_filters(&self) -> bool {
        self.col_filters.values().any(ColumnFilter::is_active) || !self.filter_tree.is_empty()
    }

    pub fn filter_description(&self) -> String {
        let mut parts = vec![];
        for (c, f) in &self.col_filters {
            parts.extend(f.describe(&self.table.headers[*c]));
        }
        if !self.filter_tree.is_empty() {
            parts.push(self.filter_tree.describe());
        }
        parts.join(" AND ")
    }

    pub fn clear_filters(&mut self) {
        self.col_filters.clear();
        self.filter_tree = FilterNode::default();
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Recomputes filtered/sorted/grouped rows if anything changed.
    /// The row under the cursor is kept at the same screen position when
    /// still visible ("context-aware filtering").
    pub fn refresh(&mut self) {
        if self.rules_dirty {
            let (rules, errs) = compile_rules(&self.table, &self.rules);
            self.compiled_rules = rules;
            self.rule_errors = errs;
            self.rules_dirty = false;
        }
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let started = std::time::Instant::now();
        let t = self.table.clone();
        let keep = self.cursor_row();
        let screen_off = self.cursor.saturating_sub(self.top);
        self.errors.clear();

        let mut preds: Vec<Pred> = vec![];
        if self.filters_enabled {
            for (c, f) in &self.col_filters {
                match compile_column_filter(&t, *c, f) {
                    Ok(p) => preds.extend(p),
                    Err(e) => self.errors.push(e),
                }
            }
            match compile_node(&t, &self.filter_tree) {
                Ok(p) => preds.push(p),
                Err(e) => self.errors.push(e),
            }
        }
        self.compiled_search = match CompiledSearch::compile(&t, &self.search, &self.search_opts, self.visible_cols()) {
            Ok(s) => s.map(Arc::new),
            Err(e) => {
                self.errors.push(e);
                None
            }
        };
        let search = match self.search_opts.behavior {
            SearchBehavior::Filter => self.compiled_search.clone(),
            SearchBehavior::Highlight => None,
        };
        for p in &preds {
            p.warm(&t);
        }
        let tags = &self.tags;
        let tagged_only = self.tagged_only;
        let mut rows: Vec<u32> = (0..t.nrows)
            .into_par_iter()
            .filter(|&r| {
                (!tagged_only || tags[r])
                    && preds.iter().all(|p| p.eval(&t, r))
                    && search.as_ref().is_none_or(|s| s.row_matches(&t, r))
            })
            .map(|r| r as u32)
            .collect();
        self.sort_rows(&mut rows);
        self.filtered = rows;
        self.rebuild_display();

        // Restore the cursor on the same data row.
        let new_cursor = keep.and_then(|k| self.display.par_iter().position_first(|d| *d == Disp::Row(k as u32)));
        match new_cursor {
            Some(i) => {
                self.cursor = i;
                self.top = i.saturating_sub(screen_off);
            }
            None => {
                self.cursor = self.cursor.min(self.display.len().saturating_sub(1));
                self.clear_selection();
                self.set_selected(self.cursor, true);
            }
        }
        self.anchor = self.cursor;
        self.last_refresh_ms = started.elapsed().as_millis();
    }

    fn sort_rows(&self, rows: &mut [u32]) {
        let t = &*self.table;
        let mut keys: Vec<(usize, bool)> =
            self.group_by.iter().map(|&c| (c, self.sort.iter().find(|s| s.0 == c).is_some_and(|s| s.1))).collect();
        for s in &self.sort {
            if !self.group_by.contains(&s.0) {
                keys.push(*s);
            }
        }
        if keys.is_empty() {
            return;
        }
        enum K<'a> {
            Num(&'a [f64]),
            Time(&'a [i64]),
            Text(usize),
        }
        let ks: Vec<(K, bool)> = keys
            .iter()
            .map(|&(c, desc)| {
                let k = match t.col_types[c] {
                    ColType::Number => K::Num(t.num_keys(c)),
                    ColType::DateTime => K::Time(t.time_keys(c)),
                    ColType::Text => K::Text(c),
                };
                (k, desc)
            })
            .collect();
        rows.par_sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            for (k, desc) in &ks {
                let o = match k {
                    K::Num(v) => v[a].total_cmp(&v[b]),
                    K::Time(v) => v[a].cmp(&v[b]),
                    K::Text(c) => cmp_ci(t.cell(a, *c), t.cell(b, *c)),
                };
                let o = if *desc { o.reverse() } else { o };
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            a.cmp(&b)
        });
    }

    /// Rebuilds the display list (group headers + rows) without re-filtering.
    pub fn rebuild_display(&mut self) {
        self.display.clear();
        self.groups.clear();
        if self.group_by.is_empty() {
            self.display = self.filtered.iter().map(|&r| Disp::Row(r)).collect();
            return;
        }
        let mut b = GroupBuilder {
            t: &self.table,
            rows: &self.filtered,
            by: &self.group_by,
            collapsed: &self.collapsed,
            display: Vec::new(),
            groups: Vec::new(),
        };
        b.level(0, 0, self.filtered.len(), "");
        self.display = b.display;
        self.groups = b.groups;
        self.cursor = self.cursor.min(self.display.len().saturating_sub(1));
    }

    pub fn toggle_group(&mut self, g: usize) {
        let key = self.groups[g].key.clone();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_display();
    }

    pub fn set_all_groups(&mut self, collapsed: bool) {
        if collapsed {
            // Collapse only top-level groups; nested ones follow.
            let keys: Vec<String> = self.groups.iter().map(|g| g.key.clone()).collect();
            self.collapsed.extend(keys);
        } else {
            self.collapsed.clear();
        }
        self.rebuild_display();
    }

    // -- selection -------------------------------------------------------

    pub fn clear_selection(&mut self) {
        if self.sel_count > 0 {
            self.selected.iter_mut().for_each(|s| *s = false);
            self.sel_count = 0;
        }
    }

    fn set_selected(&mut self, idx: usize, on: bool) {
        if let Some(Disp::Row(r)) = self.display.get(idx) {
            let r = *r as usize;
            if self.selected[r] != on {
                self.selected[r] = on;
                if on {
                    self.sel_count += 1;
                } else {
                    self.sel_count -= 1;
                }
            }
        }
    }

    fn select_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        for i in lo..=hi.min(self.display.len().saturating_sub(1)) {
            self.set_selected(i, true);
        }
    }

    /// Mouse click on a display row.
    pub fn click_row(&mut self, idx: usize, ctrl: bool, shift: bool) {
        if idx >= self.display.len() {
            return;
        }
        if shift {
            if !ctrl {
                self.clear_selection();
            }
            self.select_range(self.anchor, idx);
        } else if ctrl {
            let on = match self.display[idx] {
                Disp::Row(r) => !self.selected[r as usize],
                Disp::Group(_) => false,
            };
            self.set_selected(idx, on);
            self.anchor = idx;
        } else {
            self.clear_selection();
            self.set_selected(idx, true);
            self.anchor = idx;
        }
        self.cursor = idx;
    }

    pub fn move_cursor(&mut self, delta: isize, shift: bool) {
        if self.display.is_empty() {
            return;
        }
        let max = self.display.len() as isize - 1;
        let new = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.cursor = new;
        if shift {
            self.clear_selection();
            self.select_range(self.anchor, new);
        } else {
            self.clear_selection();
            self.set_selected(new, true);
            self.anchor = new;
        }
        self.scroll_to_cursor = true;
    }

    pub fn jump_to(&mut self, idx: usize) {
        if idx < self.display.len() {
            self.cursor = idx;
            self.clear_selection();
            self.set_selected(idx, true);
            self.anchor = idx;
            self.scroll_to_cursor = true;
        }
    }

    pub fn select_all(&mut self) {
        for i in 0..self.display.len() {
            self.set_selected(i, true);
        }
    }

    /// Rows to act on: the selection (in display order) or the cursor row.
    pub fn target_rows(&self) -> Vec<usize> {
        let mut v: Vec<usize> = if self.sel_count > 0 {
            self.display
                .iter()
                .filter_map(|d| match d {
                    Disp::Row(r) if self.selected[*r as usize] => Some(*r as usize),
                    _ => None,
                })
                .collect()
        } else {
            vec![]
        };
        if v.is_empty() {
            v.extend(self.cursor_row());
        }
        v
    }

    // -- tags ------------------------------------------------------------

    pub fn toggle_tags(&mut self) {
        let rows = self.target_rows();
        if rows.is_empty() {
            return;
        }
        let all_tagged = rows.iter().all(|&r| self.tags[r]);
        for r in rows {
            self.set_tag(r, !all_tagged);
        }
        if self.tagged_only {
            self.dirty = true;
        }
    }

    pub fn set_tag(&mut self, r: usize, on: bool) {
        if self.tags[r] != on {
            self.tags[r] = on;
            if on {
                self.tag_count += 1;
            } else {
                self.tag_count -= 1;
            }
        }
    }

    pub fn clear_tags(&mut self) {
        self.tags.iter_mut().for_each(|t| *t = false);
        self.tag_count = 0;
        if self.tagged_only {
            self.dirty = true;
        }
    }

    fn scan(&self, forward: bool, pred: impl Fn(usize) -> bool) -> Option<usize> {
        let n = self.display.len();
        if n == 0 {
            return None;
        }
        let test = |i: usize| matches!(self.display[i], Disp::Row(r) if pred(r as usize));
        if forward {
            (self.cursor + 1..n).chain(0..=self.cursor.min(n - 1)).find(|&i| test(i))
        } else {
            (0..self.cursor).rev().chain((self.cursor..n).rev()).find(|&i| test(i))
        }
    }

    pub fn next_tagged(&mut self, forward: bool) -> bool {
        match self.scan(forward, |r| self.tags[r]) {
            Some(i) => {
                self.jump_to(i);
                true
            }
            None => false,
        }
    }

    pub fn next_match(&mut self, forward: bool) -> bool {
        let Some(s) = self.compiled_search.clone() else { return false };
        let t = self.table.clone();
        match self.scan(forward, |r| s.row_matches(&t, r)) {
            Some(i) => {
                self.jump_to(i);
                true
            }
            None => false,
        }
    }

    pub fn go_to_line(&mut self, line: usize) -> Result<(), String> {
        if line == 0 || line > self.table.nrows {
            return Err(format!("Line {line} does not exist (1..{})", self.table.nrows));
        }
        let target = Disp::Row((line - 1) as u32);
        match self.display.iter().position(|d| *d == target) {
            Some(i) => {
                self.jump_to(i);
                Ok(())
            }
            None => Err(format!("Line {line} is hidden by the current filter or a collapsed group")),
        }
    }

    // -- clipboard / export ---------------------------------------------

    pub fn copy_rows(&self, with_header: bool) -> String {
        let cols = self.visible_cols();
        let mut out = String::new();
        if with_header {
            out.push_str(&cols.iter().map(|&c| self.table.headers[c].as_str()).collect::<Vec<_>>().join("\t"));
            out.push('\n');
        }
        for r in self.target_rows() {
            let vals: Vec<String> = cols.iter().map(|&c| tsv_field(self.table.cell(r, c))).collect();
            out.push_str(&vals.join("\t"));
            out.push('\n');
        }
        out
    }

    pub fn export_csv(&self, path: &std::path::Path, only_tagged: bool, all_columns: bool) -> Result<usize, String> {
        let cols: Vec<usize> = if all_columns { (0..self.table.ncols()).collect() } else { self.visible_cols() };
        let mut w = csv::Writer::from_path(path).map_err(|e| e.to_string())?;
        let mut header: Vec<&str> = vec!["Line", "Tag"];
        header.extend(cols.iter().map(|&c| self.table.headers[c].as_str()));
        w.write_record(&header).map_err(|e| e.to_string())?;
        let mut n = 0;
        for &r in &self.filtered {
            let r = r as usize;
            if only_tagged && !self.tags[r] {
                continue;
            }
            let line = (r + 1).to_string();
            let mut rec: Vec<&str> = vec![&line, if self.tags[r] { "TRUE" } else { "" }];
            rec.extend(cols.iter().map(|&c| self.table.cell(r, c)));
            w.write_record(&rec).map_err(|e| e.to_string())?;
            n += 1;
        }
        w.flush().map_err(|e| e.to_string())?;
        Ok(n)
    }
}

fn tsv_field(s: &str) -> String {
    if s.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

struct GroupBuilder<'a> {
    t: &'a Table,
    rows: &'a [u32],
    by: &'a [usize],
    collapsed: &'a HashSet<String>,
    display: Vec<Disp>,
    groups: Vec<GroupInfo>,
}

impl GroupBuilder<'_> {
    fn level(&mut self, level: usize, s: usize, e: usize, prefix: &str) {
        let col = self.by[level];
        let mut i = s;
        while i < e {
            let v = self.t.cell(self.rows[i] as usize, col);
            let mut j = i + 1;
            while j < e && self.t.cell(self.rows[j] as usize, col) == v {
                j += 1;
            }
            let key = format!("{prefix}\u{1f}{v}");
            let collapsed = self.collapsed.contains(&key);
            self.groups.push(GroupInfo { level, col, value: v.to_string(), count: j - i, key: key.clone(), collapsed });
            self.display.push(Disp::Group((self.groups.len() - 1) as u32));
            if !collapsed {
                if level + 1 < self.by.len() {
                    self.level(level + 1, i, j, &key);
                } else {
                    self.display.extend(self.rows[i..j].iter().map(|&r| Disp::Row(r)));
                }
            }
            i = j;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::table_from_rows;
    use crate::filter::{ColumnFilter, Condition, Op};

    fn view() -> TabView {
        let t = table_from_rows(
            "t",
            &["Time", "Src", "N"],
            &[
                vec!["2024-01-03 00:00:00", "b", "10"],
                vec!["2024-01-01 00:00:00", "a", "9"],
                vec!["2024-01-02 00:00:00", "b", "100"],
                vec!["2024-01-04 00:00:00", "a", "1"],
            ],
        );
        let mut v = TabView::new(Arc::new(t));
        v.refresh();
        v
    }

    fn rows(v: &TabView) -> Vec<u32> {
        v.display.iter().filter_map(|d| if let Disp::Row(r) = d { Some(*r) } else { None }).collect()
    }

    #[test]
    fn sort_typed() {
        let mut v = view();
        v.sort = vec![(2, false)];
        v.dirty = true;
        v.refresh();
        assert_eq!(rows(&v), vec![3, 1, 0, 2]);
        v.sort = vec![(0, true)];
        v.dirty = true;
        v.refresh();
        assert_eq!(rows(&v), vec![3, 0, 2, 1]);
    }

    #[test]
    fn grouping_and_collapse() {
        let mut v = view();
        v.group_by = vec![1];
        v.dirty = true;
        v.refresh();
        assert_eq!(v.groups.len(), 2);
        assert_eq!(v.display.len(), 6);
        v.toggle_group(0);
        assert_eq!(v.display.len(), 4);
    }

    #[test]
    fn context_aware_filtering() {
        let mut v = view();
        v.click_row(2, false, false);
        v.col_filters.insert(1, ColumnFilter { values: None, text: "=b".into() });
        v.dirty = true;
        v.refresh();
        assert_eq!(rows(&v), vec![0, 2]);
        assert_eq!(v.cursor_row(), Some(2));
        v.clear_filters();
        v.refresh();
        assert_eq!(v.cursor_row(), Some(2));
        v.filter_tree.push_and(Condition::new("N", Op::Less, "10"), false);
        v.dirty = true;
        v.refresh();
        assert_eq!(rows(&v), vec![1, 3]);
    }

    #[test]
    fn tags_and_navigation() {
        let mut v = view();
        v.click_row(1, false, false);
        v.click_row(3, false, true);
        v.toggle_tags();
        assert_eq!(v.tag_count, 3);
        v.jump_to(0);
        assert!(v.next_tagged(true));
        assert_eq!(v.cursor, 1);
        v.tagged_only = true;
        v.dirty = true;
        v.refresh();
        assert_eq!(rows(&v), vec![1, 2, 3]);
    }
}
