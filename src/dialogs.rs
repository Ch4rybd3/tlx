//! Secondary windows: column chooser, filter editor, conditional formatting,
//! settings, help, Plaso legend, go-to-line.

use egui::{Color32, ComboBox, RichText, TextEdit, Ui};

use crate::filter::{Condition, FilterNode, Logic, Op};
use crate::format::{self, CondRule, Preset, RuleScope};
use crate::session::{Settings, ThemeChoice};
use crate::timefmt::{self, DisplayTime};
use crate::view::TabView;

// ---------------------------------------------------------------- column chooser

#[derive(Default)]
pub struct ColumnChooser {
    pub open: bool,
    pub search: String,
}

pub fn column_chooser(ctx: &egui::Context, st: &mut ColumnChooser, view: &mut TabView) {
    let mut open = st.open;
    let mut changed = false;
    egui::Window::new("Column chooser").open(&mut open).default_width(320.0).default_height(480.0).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(TextEdit::singleline(&mut st.search).hint_text("Find column").desired_width(f32::INFINITY));
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button("Show all").clicked() {
                view.columns.iter_mut().for_each(|c| c.visible = true);
                changed = true;
            }
            if ui.button("Hide all").clicked() {
                view.columns.iter_mut().for_each(|c| c.visible = false);
                changed = true;
            }
            if ui.button("Hide empty columns").on_hover_text("Hide every column that has no value at all").clicked() {
                let t = view.table.clone();
                for c in view.columns.iter_mut() {
                    if t.column_is_empty(c.col) {
                        c.visible = false;
                    }
                }
                changed = true;
            }
            if ui.button("Reset order").clicked() {
                view.columns.sort_by_key(|c| c.col);
                changed = true;
            }
        });
        ui.weak("Tick to show; double-click a name to toggle; use ⏶⏷ to reorder.");
        ui.separator();
        let needle = st.search.to_lowercase();
        let headers = view.table.headers.clone();
        let types = view.table.col_types.clone();
        let mut swap: Option<(usize, usize)> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let n = view.columns.len();
            for i in 0..n {
                let c = view.columns[i].col;
                if !needle.is_empty() && !headers[c].to_lowercase().contains(&needle) {
                    continue;
                }
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut view.columns[i].visible, "").changed() {
                        changed = true;
                    }
                    let tname = match types[c] {
                        crate::data::ColType::Text => "abc",
                        crate::data::ColType::Number => "123",
                        crate::data::ColType::DateTime => "🕑",
                    };
                    ui.weak(tname);
                    let txt = if view.columns[i].visible { RichText::new(&headers[c]) } else { RichText::new(&headers[c]).weak() };
                    if ui.selectable_label(false, txt).double_clicked() {
                        view.columns[i].visible = !view.columns[i].visible;
                        changed = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("⏷").clicked() && i + 1 < n {
                            swap = Some((i, i + 1));
                        }
                        if ui.small_button("⏶").clicked() && i > 0 {
                            swap = Some((i, i - 1));
                        }
                    });
                });
            }
        });
        if let Some((a, b)) = swap {
            view.columns.swap(a, b);
        }
    });
    st.open = open;
    if changed {
        // Search applies to visible columns only.
        view.mark_dirty();
    }
}

// ---------------------------------------------------------------- filter editor

pub struct FilterEditor {
    pub node: FilterNode,
}

enum NodeAction {
    None,
    Remove,
}

fn column_combo(ui: &mut Ui, id: String, value: &mut String, headers: &[String], allow_any: bool) {
    let shown = if value.is_empty() && allow_any { "Any column".to_string() } else { value.clone() };
    ComboBox::from_id_salt(id).selected_text(shown).width(160.0).height(400.0).show_ui(ui, |ui| {
        if allow_any {
            ui.selectable_value(value, String::new(), "Any column");
        }
        for h in headers {
            ui.selectable_value(value, h.clone(), h);
        }
    });
}

fn op_combo(ui: &mut Ui, id: String, op: &mut Op) {
    ComboBox::from_id_salt(id).selected_text(op.label()).width(150.0).show_ui(ui, |ui| {
        for o in Op::ALL {
            ui.selectable_value(op, o, o.label());
        }
    });
}

fn node_ui(ui: &mut Ui, node: &mut FilterNode, headers: &[String], path: &str, depth: usize) -> NodeAction {
    let mut action = NodeAction::None;
    match node {
        FilterNode::Group { logic, negate, children } => {
            ui.horizontal(|ui| {
                ui.checkbox(negate, "NOT");
                ComboBox::from_id_salt(format!("{path}/logic"))
                    .selected_text(if *logic == Logic::And { "AND (all)" } else { "OR (any)" })
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(logic, Logic::And, "AND (all)");
                        ui.selectable_value(logic, Logic::Or, "OR (any)");
                    });
                if ui.button("➕ Condition").clicked() {
                    let col = headers.first().cloned().unwrap_or_default();
                    children.push(FilterNode::Cond { cond: Condition::new(&col, Op::Contains, ""), negate: false });
                }
                if ui.button("➕ Group").clicked() {
                    children.push(FilterNode::Group { logic: Logic::Or, negate: false, children: vec![] });
                }
                if depth > 0 && ui.button("🗑").on_hover_text("Remove group").clicked() {
                    action = NodeAction::Remove;
                }
            });
            let mut remove = None;
            ui.indent(format!("{path}/ind"), |ui| {
                for (i, ch) in children.iter_mut().enumerate() {
                    if let NodeAction::Remove = node_ui(ui, ch, headers, &format!("{path}/{i}"), depth + 1) {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                children.remove(i);
            }
        }
        FilterNode::Cond { cond, negate } => {
            ui.horizontal(|ui| {
                ui.checkbox(negate, "NOT");
                column_combo(ui, format!("{path}/col"), &mut cond.column, headers, false);
                op_combo(ui, format!("{path}/op"), &mut cond.op);
                if cond.op.arity() >= 1 {
                    ui.add(TextEdit::singleline(&mut cond.value).desired_width(160.0));
                }
                if cond.op.arity() == 2 {
                    ui.label("and");
                    ui.add(TextEdit::singleline(&mut cond.value2).desired_width(160.0));
                }
                ui.toggle_value(&mut cond.case_sensitive, "Aa").on_hover_text("Case sensitive");
                if ui.button("🗑").clicked() {
                    action = NodeAction::Remove;
                }
            });
        }
    }
    action
}

/// Returns true when the filter was applied.
pub fn filter_editor(ctx: &egui::Context, st: &mut Option<FilterEditor>, view: &mut TabView) {
    let Some(ed) = st else { return };
    let mut open = true;
    let mut apply = false;
    let mut close = false;
    let headers = view.table.headers.clone();
    egui::Window::new("Filter editor").open(&mut open).default_width(720.0).default_height(360.0).show(ctx, |ui| {
        ui.weak("Build complex criteria with nested AND / OR groups. Column header filters are kept separately.");
        ui.separator();
        egui::ScrollArea::both().auto_shrink([false, true]).max_height(ui.available_height() - 40.0).show(ui, |ui| {
            node_ui(ui, &mut ed.node, &headers, "root", 0);
        });
        ui.separator();
        ui.label(RichText::new(ed.node.describe()).monospace().weak());
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                apply = true;
                close = true;
            }
            if ui.button("Apply").clicked() {
                apply = true;
            }
            if ui.button("Clear").clicked() {
                ed.node = FilterNode::default();
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if apply {
        view.filter_tree = ed.node.clone();
        view.filters_enabled = true;
        view.mark_dirty();
    }
    if close || !open {
        *st = None;
    }
}

// ---------------------------------------------------------------- conditional formatting

pub struct RulesManager {
    pub rules: Vec<CondRule>,
}

fn color_edit(ui: &mut Ui, c: &mut [u8; 4]) -> bool {
    let mut col = Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
    let r = ui.color_edit_button_srgba(&mut col);
    if r.changed() {
        *c = col.to_srgba_unmultiplied();
    }
    r.changed()
}

fn rule_editor(ui: &mut Ui, r: &mut CondRule, headers: &[String], id: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut r.enabled, "");
        let mut col = r.column.clone().unwrap_or_default();
        column_combo(ui, format!("{id}/col"), &mut col, headers, true);
        r.column = if col.is_empty() { None } else { Some(col) };
        op_combo(ui, format!("{id}/op"), &mut r.op);
        if r.op.arity() >= 1 {
            ui.add(TextEdit::singleline(&mut r.value).desired_width(140.0));
        }
        if r.op.arity() == 2 {
            ui.label("and");
            ui.add(TextEdit::singleline(&mut r.value2).desired_width(140.0));
        }
        ui.toggle_value(&mut r.case_sensitive, "Aa").on_hover_text("Case sensitive");
        ComboBox::from_id_salt(format!("{id}/scope"))
            .selected_text(if r.scope == RuleScope::Row { "Whole row" } else { "Cell only" })
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut r.scope, RuleScope::Row, "Whole row");
                ui.selectable_value(&mut r.scope, RuleScope::Cell, "Cell only");
            });
        ui.label("Fill");
        color_edit(ui, &mut r.bg);
        let mut has_fg = r.fg.is_some();
        ui.checkbox(&mut has_fg, "Text");
        match (has_fg, &mut r.fg) {
            (true, Some(fg)) => {
                color_edit(ui, fg);
            }
            (true, None) => r.fg = Some([0, 0, 0, 255]),
            (false, _) => r.fg = None,
        }
        ui.menu_button("Presets", |ui| {
            for p in Preset::ALL {
                let (bg, fg) = p.colors();
                if ui.button(RichText::new(p.label()).background_color(bg).color(fg)).clicked() {
                    r.bg = bg.to_array();
                    r.fg = Some(fg.to_array());
                    ui.close();
                }
            }
        });
    });
}

pub fn rules_manager(ctx: &egui::Context, st: &mut Option<RulesManager>, view: &mut TabView) {
    let Some(m) = st else { return };
    let mut open = true;
    let mut apply = false;
    let mut close = false;
    let headers = view.table.headers.clone();
    egui::Window::new("Conditional formatting rules").open(&mut open).default_width(860.0).default_height(380.0).show(ctx, |ui| {
        ui.weak("Rules are evaluated top to bottom; the first matching row rule wins. Applies to this tab only.");
        ui.separator();
        let mut remove = None;
        let mut swap = None;
        egui::ScrollArea::vertical().auto_shrink([false, true]).max_height(ui.available_height() - 60.0).show(ui, |ui| {
            let n = m.rules.len();
            for i in 0..n {
                ui.horizontal(|ui| {
                    if ui.small_button("⏶").clicked() && i > 0 {
                        swap = Some((i, i - 1));
                    }
                    if ui.small_button("⏷").clicked() && i + 1 < n {
                        swap = Some((i, i + 1));
                    }
                    if ui.small_button("🗑").clicked() {
                        remove = Some(i);
                    }
                    let (bg, fg) = (m.rules[i].bg, m.rules[i].fg.unwrap_or([0, 0, 0, 255]));
                    ui.label(
                        RichText::new(" Sample ")
                            .background_color(Color32::from_rgba_unmultiplied(bg[0], bg[1], bg[2], bg[3]))
                            .color(Color32::from_rgba_unmultiplied(fg[0], fg[1], fg[2], fg[3])),
                    );
                    let desc = m.rules[i].describe();
                    rule_editor(ui, &mut m.rules[i], &headers, &format!("rule{i}"));
                    ui.weak(desc);
                });
                ui.separator();
            }
            if n == 0 {
                ui.weak("No rule yet.");
            }
        });
        if let Some(i) = remove {
            m.rules.remove(i);
        }
        if let Some((a, b)) = swap {
            m.rules.swap(a, b);
        }
        for e in &view.rule_errors {
            ui.colored_label(Color32::LIGHT_RED, e);
        }
        ui.horizontal(|ui| {
            if ui.button("➕ New rule").clicked() {
                m.rules.push(CondRule::new(None, Op::Contains, "", Preset::Yellow));
            }
            if ui.button("OK").clicked() {
                apply = true;
                close = true;
            }
            if ui.button("Apply").clicked() {
                apply = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if apply {
        view.rules = m.rules.clone();
        view.rules_dirty = true;
    }
    if close || !open {
        *st = None;
    }
}

/// Quick "Highlight cell rules > Text that contains…" dialog.
pub fn rule_dialog(ctx: &egui::Context, st: &mut Option<CondRule>, view: &mut TabView) {
    let Some(r) = st else { return };
    let mut open = true;
    let mut done = None;
    let headers = view.table.headers.clone();
    egui::Window::new("New formatting rule").open(&mut open).collapsible(false).default_width(640.0).show(ctx, |ui| {
        ui.label(format!("Format cells that {}:", r.op.label()));
        rule_editor(ui, r, &headers, "quick_rule");
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() {
                done = Some(true);
            }
            if ui.button("Cancel").clicked() {
                done = Some(false);
            }
        });
    });
    if done == Some(true) {
        view.rules.push(r.clone());
        view.rules_dirty = true;
    }
    if done.is_some() || !open {
        *st = None;
    }
}

// ---------------------------------------------------------------- settings

pub fn settings_window(ctx: &egui::Context, open: &mut bool, s: &mut Settings) -> bool {
    let mut changed = false;
    let mut o = *open;
    egui::Window::new("Settings").open(&mut o).default_width(460.0).show(ctx, |ui| {
        egui::Grid::new("settings_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Skin");
            ui.horizontal(|ui| {
                changed |= ui.selectable_value(&mut s.theme, ThemeChoice::Dark, "Dark").changed();
                changed |= ui.selectable_value(&mut s.theme, ThemeChoice::Light, "Light").changed();
                changed |= ui.selectable_value(&mut s.theme, ThemeChoice::System, "System").changed();
            });
            ui.end_row();

            ui.label("Font size");
            changed |= ui.add(egui::Slider::new(&mut s.font_size, 9.0..=22.0).step_by(0.5)).changed();
            ui.end_row();

            ui.label("Datetime format");
            ui.vertical(|ui| {
                changed |= ui.add(TextEdit::singleline(&mut s.datetime_format).desired_width(260.0)).changed();
                ui.horizontal_wrapped(|ui| {
                    for preset in [
                        "yyyy-MM-dd HH:mm:ss",
                        "yyyy-MM-dd HH:mm:ss.fff",
                        "yyyy-MM-dd HH:mm:ss.fffffff",
                        "yyyy-MM-dd'T'HH:mm:ss.fffK",
                        "MM/dd/yyyy hh:mm:ss tt",
                        "dd/MM/yyyy HH:mm:ss",
                    ] {
                        if ui.small_button(preset).clicked() {
                            s.datetime_format = preset.to_string();
                            changed = true;
                        }
                    }
                });
                let preview = DisplayTime::new(&s.datetime_format, s.tz_offset_minutes).format("2024-03-15 13:45:30.1234567");
                match preview {
                    Some(p) => ui.weak(format!("Preview: {p}")),
                    None => ui.colored_label(Color32::LIGHT_RED, "Invalid format"),
                };
                ui.weak(".NET tokens (yyyy MM dd HH hh mm ss fff tt K) or strftime (%Y-%m-%d …)");
            });
            ui.end_row();

            ui.label("Reformat timestamps");
            changed |= ui.checkbox(&mut s.reformat_dates, "Display datetime columns with the format above").changed();
            ui.end_row();

            ui.label("Display time zone");
            ui.horizontal(|ui| {
                let mut h = s.tz_offset_minutes as f32 / 60.0;
                if ui.add(egui::DragValue::new(&mut h).range(-14.0..=14.0).speed(0.25).fixed_decimals(2)).changed() {
                    s.tz_offset_minutes = (h * 60.0).round() as i32;
                    changed = true;
                }
                ui.label(timefmt::format_offset(s.tz_offset_minutes));
                if ui.small_button("UTC").clicked() {
                    s.tz_offset_minutes = 0;
                    changed = true;
                }
            });
            ui.end_row();

            ui.label("Grid");
            ui.vertical(|ui| {
                changed |= ui.checkbox(&mut s.show_filter_row, "Show auto-filter row").changed();
                changed |= ui.checkbox(&mut s.show_group_panel, "Show group-by panel").changed();
                changed |= ui.checkbox(&mut s.plaso_colors, "Colour-code Plaso timelines by source").changed();
            });
            ui.end_row();
        });
    });
    *open = o;
    changed
}

// ---------------------------------------------------------------- help

pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+O", "Open file(s) (CSV, TSV, TXT, XLSX/XLS/ODS, EVTX) — drag & drop also works"),
    ("Drop several .evtx / a folder", "Parsed and merged into one timeline (View > Merge EVTX files opened together)"),
    ("Ctrl+S / Ctrl+Shift+O", "Save session / Load session"),
    ("Ctrl+W / Ctrl+Q", "Close current tab / Close all tabs"),
    ("Ctrl+Tab, Ctrl+PgDn / Ctrl+Shift+Tab, Ctrl+PgUp", "Next / previous tab"),
    ("Ctrl+F", "Focus the search box (Enter / F3 = next match, Shift+F3 = previous)"),
    ("Ctrl+T or Space", "Tag / untag selected rows"),
    ("Ctrl+Shift+T", "Show only tagged rows (toggle)"),
    ("F2 / Shift+F2", "Jump to next / previous tagged row"),
    ("Ctrl+D", "Details pane for the current row (also double-click a row)"),
    ("Ctrl+R", "Reset column widths (best fit all columns)"),
    ("Ctrl+G", "Go to line number"),
    ("Ctrl+E", "Clear all filters (column filters + filter editor)"),
    ("Ctrl+Shift+E", "Enable / disable the active filter"),
    ("Ctrl+K", "Column chooser"),
    ("Ctrl+L", "Filter editor"),
    ("Ctrl+C / Ctrl+Shift+C", "Copy selected rows with header / copy current cell"),
    ("Ctrl+A", "Select all visible rows"),
    ("↑ ↓ PgUp PgDn Home End", "Move (add Shift to extend the selection)"),
    ("Ctrl+Home / Ctrl+End", "First / last row"),
    ("← → (Ctrl: first/last)", "Move the current cell between columns"),
    ("Esc", "Close the filter popup / leave the search box"),
    ("Header click / Shift+click / Ctrl+click", "Sort / add secondary sort / remove sort"),
    ("Drag header", "Reorder columns; drop on the group panel to group"),
    ("Double-click header edge", "Best fit that column"),
    ("Funnel icon in header", "Instant value filter (Shift-click range, then Space toggles)"),
    ("Double-click path in status bar", "Copy the file path"),
];

pub const SEARCH_HELP: &[(&str, &str)] = &[
    ("word1 word2", "Rows containing word1 OR word2 (match mode \"Any\"), both with \"All\""),
    ("\"two words\"", "Exact phrase"),
    ("+word", "Word is required"),
    ("-word", "Rows containing word are excluded"),
    ("Column:value", "Search value only in that column (unique prefix of the name is enough)"),
    ("\"Column name\":\"value\"", "Quoted column name and/or value"),
    ("Options ⚙", "Filter vs Highlight, Contains / Starts with / Equals / Like / Regex, case sensitivity"),
];

pub const AUTOFILTER_HELP: &[(&str, &str)] = &[
    ("text", "Contains (case-insensitive)"),
    ("=text", "Equals"),
    ("!text / <>text", "Does not contain / does not equal"),
    ("^text / text$", "Begins with / ends with"),
    ("> >= < <= value", "Typed comparison (numbers, timestamps, else text)"),
    ("a*b?c", "Wildcards"),
    ("/regex/", "Regular expression"),
];

pub fn help_window(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("Quick help").open(open).default_width(620.0).default_height(560.0).show(ctx, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let table = |ui: &mut Ui, id: &str, rows: &[(&str, &str)]| {
                egui::Grid::new(id).striped(true).num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    for (k, v) in rows {
                        ui.label(RichText::new(*k).monospace().strong());
                        ui.label(*v);
                        ui.end_row();
                    }
                });
            };
            ui.heading("Keyboard & mouse");
            table(ui, "help_keys", SHORTCUTS);
            ui.add_space(10.0);
            ui.heading("Search box (power filtering)");
            table(ui, "help_search", SEARCH_HELP);
            ui.add_space(10.0);
            ui.heading("Auto-filter row (under the headers)");
            table(ui, "help_af", AUTOFILTER_HELP);
            ui.add_space(10.0);
            ui.heading("Forensic soundness");
            ui.label("Source files are opened read-only and are never modified. Tags, notes, layout and filters live in session files (*.tlxs).");
        });
    });
}

pub fn legend_window(ctx: &egui::Context, open: &mut bool, dark: bool) {
    egui::Window::new("Plaso colour legend").open(open).default_width(380.0).show(ctx, |ui| {
        ui.weak("Rows of Plaso (l2tcsv / dynamic) timelines are coloured by their 'source' column.");
        egui::Grid::new("legend").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            for (k, desc, _) in format::PLASO_LEGEND {
                let c = format::plaso_color(k, dark).unwrap();
                ui.label(RichText::new(format!("  {k}  ")).background_color(c).monospace());
                ui.label(*desc);
                ui.end_row();
            }
        });
    });
}

pub fn about_window(ctx: &egui::Context, open: &mut bool) {
    egui::Window::new("About TLX").open(open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.heading("TLX — Timeline eXplorer");
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.label("Cross-platform (Linux / Windows) forensic timeline & CSV viewer written in Rust.");
        ui.label("Inspired by Eric Zimmerman's Timeline Explorer.");
    });
}

/// Returns Some(line) when the user validates.
pub fn goto_window(ctx: &egui::Context, st: &mut Option<String>) -> Option<usize> {
    let s = st.as_mut()?;
    let mut open = true;
    let mut result = None;
    egui::Window::new("Go to line").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label("Line:");
            let r = ui.add(TextEdit::singleline(s).desired_width(120.0));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                result = s.trim().parse().ok();
            }
            if ui.button("Go").clicked() {
                result = s.trim().parse().ok();
            }
        });
    });
    if result.is_some() || !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        *st = None;
    }
    result
}
