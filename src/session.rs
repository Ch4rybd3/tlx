//! Session files (File > Save session) and persistent application settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::filter::{ColumnFilter, FilterNode, SearchOptions};
use crate::format::CondRule;
use crate::view::TabView;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// .NET style (`yyyy-MM-dd HH:mm:ss`) or strftime (`%Y-%m-%d`) format.
    pub datetime_format: String,
    pub reformat_dates: bool,
    pub tz_offset_minutes: i32,
    pub font_size: f32,
    pub show_filter_row: bool,
    pub show_group_panel: bool,
    pub plaso_colors: bool,
    /// Several EVTX dropped/opened together become one merged tab.
    pub merge_evtx: bool,
    pub details_name_width: f32,
    pub details_wrap: bool,
    pub default_search: SearchOptions,
    pub recent: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            datetime_format: "yyyy-MM-dd HH:mm:ss".into(),
            reformat_dates: true,
            tz_offset_minutes: 0,
            font_size: 13.0,
            show_filter_row: true,
            show_group_panel: true,
            plaso_colors: true,
            merge_evtx: true,
            details_name_width: 150.0,
            details_wrap: true,
            default_search: SearchOptions::default(),
            recent: vec![],
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tlx").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = settings_path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, s);
            }
        }
    }

    pub fn push_recent(&mut self, p: &Path) {
        self.recent.retain(|x| x != p);
        self.recent.insert(0, p.to_path_buf());
        self.recent.truncate(12);
    }
}

// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ColSession {
    pub name: String,
    pub visible: bool,
    pub width: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct TabSession {
    pub path: PathBuf,
    /// Merged source files (EVTX drops); empty for a single file.
    pub sources: Vec<PathBuf>,
    pub columns: Vec<ColSession>,
    pub frozen: usize,
    pub sort: Vec<(String, bool)>,
    pub group_by: Vec<String>,
    pub collapsed: Vec<String>,
    pub col_filters: Vec<(String, ColumnFilter)>,
    pub filter_tree: FilterNode,
    pub filters_enabled: bool,
    pub search: String,
    pub search_opts: SearchOptions,
    pub tagged_only: bool,
    /// Tagged rows as (first 0-based row, count) ranges.
    pub tags: Vec<(usize, usize)>,
    pub rules: Vec<CondRule>,
    pub cursor_line: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Session {
    pub version: u32,
    pub app: String,
    pub active: usize,
    pub tabs: Vec<TabSession>,
}

impl TabSession {
    pub fn capture(v: &TabView) -> Self {
        let h = &v.table.headers;
        let mut tags = vec![];
        let mut r = 0;
        while r < v.tags.len() {
            if v.tags[r] {
                let s = r;
                while r < v.tags.len() && v.tags[r] {
                    r += 1;
                }
                tags.push((s, r - s));
            } else {
                r += 1;
            }
        }
        Self {
            path: v.table.path.clone(),
            sources: v.table.sources.clone(),
            columns: v.columns.iter().map(|c| ColSession { name: h[c.col].clone(), visible: c.visible, width: c.width }).collect(),
            frozen: v.frozen,
            sort: v.sort.iter().map(|&(c, d)| (h[c].clone(), d)).collect(),
            group_by: v.group_by.iter().map(|&c| h[c].clone()).collect(),
            collapsed: v.collapsed.iter().cloned().collect(),
            col_filters: v.col_filters.iter().filter(|(_, f)| f.is_active()).map(|(c, f)| (h[*c].clone(), f.clone())).collect(),
            filter_tree: v.filter_tree.clone(),
            filters_enabled: v.filters_enabled,
            search: v.search.clone(),
            search_opts: v.search_opts,
            tagged_only: v.tagged_only,
            tags,
            rules: v.rules.clone(),
            cursor_line: v.cursor_row(),
        }
    }

    pub fn apply(&self, v: &mut TabView) {
        let idx = |name: &str| v.table.headers.iter().position(|h| h == name);
        if !self.columns.is_empty() {
            let mut cols = vec![];
            for cs in &self.columns {
                if let Some(c) = idx(&cs.name)
                    && !cols.iter().any(|x: &crate::view::ColumnState| x.col == c)
                {
                    cols.push(crate::view::ColumnState { col: c, visible: cs.visible, width: cs.width });
                }
            }
            for c in 0..v.table.ncols() {
                if !cols.iter().any(|x| x.col == c) {
                    cols.push(crate::view::ColumnState { col: c, visible: true, width: 120.0 });
                }
            }
            v.columns = cols;
            v.ui.needs_autofit = false;
        }
        v.frozen = self.frozen;
        v.sort = self.sort.iter().filter_map(|(n, d)| idx(n).map(|c| (c, *d))).collect();
        v.group_by = self.group_by.iter().filter_map(|n| idx(n)).collect();
        v.collapsed = self.collapsed.iter().cloned().collect();
        v.col_filters = self.col_filters.iter().filter_map(|(n, f)| idx(n).map(|c| (c, f.clone()))).collect();
        v.filter_tree = self.filter_tree.clone();
        v.filters_enabled = self.filters_enabled;
        v.search = self.search.clone();
        v.search_opts = self.search_opts;
        v.tagged_only = self.tagged_only;
        v.clear_tags();
        for &(s, n) in &self.tags {
            for r in s..(s + n).min(v.tags.len()) {
                v.set_tag(r, true);
            }
        }
        v.rules = self.rules.clone();
        v.rules_dirty = true;
        v.dirty = true;
        v.refresh();
        if let Some(line) = self.cursor_line {
            let _ = v.go_to_line(line + 1);
        }
    }
}

pub fn save_session(path: &Path, s: &Session) -> Result<(), String> {
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load_session(path: &Path) -> Result<Session, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("Invalid session file: {e}"))
}
