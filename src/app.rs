//! Main application window: menus, tabs, search bar, panels and shortcuts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

use egui::{Color32, Event, Key, Modifiers, RichText, TextEdit, ThemePreference, vec2};

use crate::data::{self, Progress, Table, TableKind};
use crate::dialogs::{self, ColumnChooser, FilterEditor, RulesManager};
use crate::filter::{MatchMode, SearchBehavior, SearchCondition};
use crate::format::CondRule;
use crate::grid::{self, GridAction, GridConfig};
use crate::session::{self, Session, Settings, TabSession, ThemeChoice};
use crate::timefmt::DisplayTime;
use crate::view::{Disp, TabView};

pub const SESSION_EXT: &str = "tlxs";

#[allow(clippy::large_enum_variant)]
enum TabContent {
    Loading { progress: Arc<Progress>, rx: Receiver<Result<Table, String>>, pending: Option<TabSession> },
    Ready(Box<TabView>),
    Failed(String),
}

struct Tab {
    path: PathBuf,
    title: String,
    content: TabContent,
}

#[derive(Default)]
struct Dialogs {
    columns: ColumnChooser,
    filter_editor: Option<FilterEditor>,
    rules: Option<RulesManager>,
    rule: Option<CondRule>,
    settings: bool,
    help: bool,
    legend: bool,
    about: bool,
    goto: Option<String>,
}

pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    settings: Settings,
    display_time: DisplayTime,
    tab_filter: String,
    dlg: Dialogs,
    details: bool,
    details_filter: String,
    details_hide_empty: bool,
    status: Option<(String, f64, bool)>,
    focus_search: bool,
    session_path: Option<PathBuf>,
    applied_font: f32,
    last_title: String,
}

fn search_id() -> egui::Id {
    egui::Id::unique("global_search")
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        let settings = Settings::load();
        let display_time = DisplayTime::new(&settings.datetime_format, settings.tz_offset_minutes);
        let mut app = Self {
            tabs: vec![],
            active: 0,
            settings,
            display_time,
            tab_filter: String::new(),
            dlg: Dialogs::default(),
            details: false,
            details_filter: String::new(),
            details_hide_empty: false,
            status: None,
            focus_search: false,
            session_path: None,
            applied_font: 0.0,
            last_title: String::new(),
        };
        app.apply_theme(&cc.egui_ctx);
        if !files.is_empty() {
            app.open_many(&cc.egui_ctx, files);
        }
        app
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        ctx.set_theme(match self.settings.theme {
            ThemeChoice::Dark => ThemePreference::Dark,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::System => ThemePreference::System,
        });
        if self.applied_font != self.settings.font_size {
            let size = self.settings.font_size;
            ctx.global_style_mut(|s| {
                for (style, font) in s.text_styles.iter_mut() {
                    font.size = match style {
                        egui::TextStyle::Heading => size + 5.0,
                        egui::TextStyle::Small => size - 3.0,
                        _ => size,
                    };
                }
            });
            self.applied_font = size;
        }
        self.display_time = DisplayTime::new(&self.settings.datetime_format, self.settings.tz_offset_minutes);
    }

    fn set_status(&mut self, ctx: &egui::Context, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), ctx.input(|i| i.time), error));
    }

    fn view(&mut self) -> Option<&mut TabView> {
        match &mut self.tabs.get_mut(self.active)?.content {
            TabContent::Ready(v) => Some(v),
            _ => None,
        }
    }

    // ------------------------------------------------------------ files

    fn open_any(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.open_many(ctx, vec![path]);
    }

    /// Opens dropped / picked paths. Several EVTX files are merged into one
    /// tab (when enabled); a folder opens all the EVTX files it contains.
    fn open_many(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        let mut evtx = vec![];
        for p in paths {
            if p.extension().is_some_and(|e| e.eq_ignore_ascii_case(SESSION_EXT)) {
                self.load_session(ctx, &p);
            } else if data::is_evtx(&p) && self.settings.merge_evtx {
                evtx.push(p);
            } else {
                self.open_file(ctx, p, None);
            }
        }
        match evtx.len() {
            0 => {}
            1 => self.open_file(ctx, evtx.pop().unwrap(), None),
            _ => {
                let label = common_parent(&evtx);
                self.open_sources(ctx, label, evtx, None);
            }
        }
    }

    fn open_file(&mut self, ctx: &egui::Context, path: PathBuf, pending: Option<TabSession>) {
        self.open_sources(ctx, path, vec![], pending);
    }

    fn open_sources(&mut self, ctx: &egui::Context, path: PathBuf, sources: Vec<PathBuf>, pending: Option<TabSession>) {
        if pending.is_none()
            && sources.is_empty()
            && let Some(i) = self.tabs.iter().position(|t| t.path == path)
        {
            self.active = i;
            return;
        }
        let progress = Arc::new(Progress::default());
        let (tx, rx) = channel();
        let (p2, path2, src2, ctx2) = (progress.clone(), path.clone(), sources.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = data::load_sources(&path2, &src2, p2);
            let _ = tx.send(r);
            ctx2.request_repaint();
        });
        let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        let title = if !sources.is_empty() {
            format!("{name} ({} EVTX)", sources.len())
        } else if path.is_dir() {
            format!("{name} (EVTX folder)")
        } else {
            name
        };
        if sources.is_empty() {
            self.settings.push_recent(&path);
            self.settings.save();
        }
        self.tabs.push(Tab { path, title, content: TabContent::Loading { progress, rx, pending } });
        self.active = self.tabs.len() - 1;
    }

    fn poll_loading(&mut self, ctx: &egui::Context) {
        let mut any_loading = false;
        let mut msgs = vec![];
        for tab in &mut self.tabs {
            if let TabContent::Loading { rx, pending, .. } = &mut tab.content {
                match rx.try_recv() {
                    Ok(Ok(table)) => {
                        let rows = table.nrows;
                        let plaso = matches!(table.kind, TableKind::Plaso { .. });
                        let mut v = TabView::new(Arc::new(table));
                        v.search_opts = self.settings.default_search;
                        if let Some(p) = pending.take() {
                            p.apply(&mut v);
                        }
                        v.refresh();
                        let notes = v.table.notes.join("; ");
                        msgs.push(format!(
                            "Loaded {} — {rows} lines{}{}",
                            tab.title,
                            if plaso { " (Plaso timeline detected)" } else { "" },
                            if notes.is_empty() { String::new() } else { format!(" — {notes}") }
                        ));
                        tab.content = TabContent::Ready(Box::new(v));
                    }
                    Ok(Err(e)) => tab.content = TabContent::Failed(e),
                    Err(std::sync::mpsc::TryRecvError::Empty) => any_loading = true,
                    Err(_) => tab.content = TabContent::Failed("Loader thread crashed".into()),
                }
            }
        }
        for m in msgs {
            self.set_status(ctx, m, false);
        }
        if any_loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            if let TabContent::Loading { progress, .. } = &self.tabs[i].content {
                progress.cancel.store(true, Ordering::Relaxed);
            }
            self.tabs.remove(i);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let files = rfd::FileDialog::new()
            .set_title("Open timeline / CSV files")
            .add_filter("Supported files", &["csv", "tsv", "txt", "tab", "xlsx", "xlsm", "xls", "xlsb", "ods", "evtx", SESSION_EXT])
            .add_filter("Windows event logs", &["evtx"])
            .add_filter("All files", &["*"])
            .pick_files();
        let files = files.unwrap_or_default();
        if !files.is_empty() {
            self.open_many(ctx, files);
        }
    }

    fn open_folder_dialog(&mut self, ctx: &egui::Context) {
        if let Some(dir) = rfd::FileDialog::new().set_title("Open a folder of EVTX files (recursive, merged)").pick_folder() {
            self.open_file(ctx, dir, None);
        }
    }

    fn save_session(&mut self, ctx: &egui::Context, save_as: bool) {
        let path = match (&self.session_path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new()
                .set_title("Save session")
                .add_filter("TLX session", &[SESSION_EXT])
                .set_file_name(format!("session.{SESSION_EXT}"))
                .save_file(),
        };
        let Some(path) = path else { return };
        let tabs: Vec<TabSession> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.content {
                TabContent::Ready(v) => Some(TabSession::capture(v)),
                _ => None,
            })
            .collect();
        let s = Session { version: 1, app: format!("tlx {}", env!("CARGO_PKG_VERSION")), active: self.active, tabs };
        match session::save_session(&path, &s) {
            Ok(()) => {
                self.set_status(ctx, format!("Session saved to {}", path.display()), false);
                self.session_path = Some(path);
            }
            Err(e) => self.set_status(ctx, e, true),
        }
    }

    fn load_session_dialog(&mut self, ctx: &egui::Context) {
        if let Some(p) = rfd::FileDialog::new().add_filter("TLX session", &[SESSION_EXT]).pick_file() {
            self.load_session(ctx, &p);
        }
    }

    fn load_session(&mut self, ctx: &egui::Context, path: &Path) {
        match session::load_session(path) {
            Ok(s) => {
                self.tabs.clear();
                for t in s.tabs {
                    if !t.sources.is_empty() {
                        let (path, sources) = (t.path.clone(), t.sources.clone());
                        self.open_sources(ctx, path, sources, Some(t));
                    } else if t.path.exists() {
                        self.open_file(ctx, t.path.clone(), Some(t));
                    } else {
                        self.set_status(ctx, format!("Missing file: {}", t.path.display()), true);
                    }
                }
                self.active = s.active.min(self.tabs.len().saturating_sub(1));
                self.session_path = Some(path.to_path_buf());
            }
            Err(e) => self.set_status(ctx, e, true),
        }
    }

    fn export(&mut self, ctx: &egui::Context, only_tagged: bool) {
        let Some(v) = self.view() else { return };
        let stem = v.table.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let name = if only_tagged { format!("{stem}_tagged.csv") } else { format!("{stem}_filtered.csv") };
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name(name).save_file() else { return };
        if path == v.table.path {
            self.set_status(ctx, "Refusing to overwrite the source file", true);
            return;
        }
        let r = v.export_csv(&path, only_tagged, false);
        match r {
            Ok(n) => self.set_status(ctx, format!("Exported {n} rows to {}", path.display()), false),
            Err(e) => self.set_status(ctx, e, true),
        }
    }

    // ------------------------------------------------------------ keyboard

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let mut presses: Vec<(Key, Modifiers)> = vec![];
        let mut copy = false;
        ctx.input(|i| {
            for e in &i.events {
                match e {
                    Event::Key { key, pressed: true, modifiers, .. } => presses.push((*key, *modifiers)),
                    Event::Copy => copy = true,
                    _ => {}
                }
            }
        });
        for (key, m) in presses {
            let cmd = m.command;
            let shift = m.shift;
            // Global (also while typing)
            match (key, cmd, shift) {
                (Key::O, true, false) => self.open_dialog(ctx),
                (Key::O, true, true) => self.load_session_dialog(ctx),
                (Key::S, true, s) => self.save_session(ctx, s),
                (Key::W, true, _) => self.close_tab(self.active),
                (Key::Q, true, _) => {
                    while !self.tabs.is_empty() {
                        self.close_tab(0);
                    }
                }
                (Key::Tab | Key::PageDown, true, false) if !self.tabs.is_empty() => self.active = (self.active + 1) % self.tabs.len(),
                (Key::Tab, true, true) | (Key::PageUp, true, _) if !self.tabs.is_empty() => {
                    self.active = (self.active + self.tabs.len() - 1) % self.tabs.len()
                }
                (Key::F, true, _) => self.focus_search = true,
                (Key::F3, _, s) => {
                    if let Some(v) = self.view()
                        && !v.next_match(!s)
                    {
                        self.set_status(ctx, "No match", true);
                    }
                }
                (Key::Escape, _, _) => {
                    if let Some(v) = self.view() {
                        v.ui.filter_popup = None;
                    }
                    ctx.memory_mut(|mem| mem.surrender_focus(search_id()));
                }
                _ => {}
            }
            if typing {
                continue;
            }
            let Some(v) = self.view() else { continue };
            let page = 25isize;
            match (key, cmd, shift) {
                (Key::T, true, true) => {
                    v.tagged_only = !v.tagged_only;
                    v.mark_dirty();
                }
                (Key::Space, false, false) if v.ui.filter_popup.is_some() => {}
                (Key::T, true, false) | (Key::Space, false, false) => v.toggle_tags(),
                (Key::F2, _, s) => {
                    if !v.next_tagged(!s) {
                        self.set_status(ctx, "No tagged row", true);
                    }
                }
                (Key::D, true, _) => self.details = !self.details,
                (Key::R, true, _) => v.ui.needs_autofit = true,
                (Key::G, true, _) => self.dlg.goto = Some(String::new()),
                (Key::E, true, true) => {
                    v.filters_enabled = !v.filters_enabled;
                    v.mark_dirty();
                }
                (Key::E, true, false) => v.clear_filters(),
                (Key::K, true, _) => self.dlg.columns.open = true,
                (Key::L, true, _) => {
                    let node = v.filter_tree.clone();
                    self.dlg.filter_editor = Some(FilterEditor { node });
                }
                (Key::A, true, _) => v.select_all(),
                (Key::C, true, true) => {
                    if let Some(r) = v.cursor_row() {
                        let cols = v.visible_cols();
                        if let Some(&c) = cols.get(v.cursor_col) {
                            ctx.copy_text(v.table.cell(r, c).to_string());
                        }
                    }
                }
                (Key::ArrowDown, true, s) => v.move_cursor(isize::MAX / 2, s),
                (Key::ArrowUp, true, s) => v.move_cursor(-(isize::MAX / 2), s),
                (Key::ArrowDown, false, s) => v.move_cursor(1, s),
                (Key::ArrowUp, false, s) => v.move_cursor(-1, s),
                (Key::PageDown, false, s) => v.move_cursor(page, s),
                (Key::PageUp, false, s) => v.move_cursor(-page, s),
                (Key::Home, true, s) => v.move_cursor(-(isize::MAX / 2), s),
                (Key::End, true, s) => v.move_cursor(isize::MAX / 2, s),
                (Key::Home, false, _) => {
                    v.cursor_col = 0;
                    v.scroll_to_cursor = true;
                }
                (Key::End, false, _) => {
                    v.cursor_col = v.visible_cols().len().saturating_sub(1);
                    v.scroll_to_cursor = true;
                }
                (Key::ArrowLeft, c, _) => {
                    v.cursor_col = if c { 0 } else { v.cursor_col.saturating_sub(1) };
                    v.scroll_to_cursor = true;
                }
                (Key::ArrowRight, c, _) => {
                    let n = v.visible_cols().len().saturating_sub(1);
                    v.cursor_col = if c { n } else { (v.cursor_col + 1).min(n) };
                    v.scroll_to_cursor = true;
                }
                (Key::Enter, false, _) => {
                    if let Some(Disp::Group(g)) = v.display.get(v.cursor).copied() {
                        v.toggle_group(g as usize);
                    }
                }
                _ => {}
            }
        }
        if copy
            && !typing
            && let Some(v) = self.view()
        {
            let text = v.copy_rows(true);
            let n = text.lines().count().saturating_sub(1);
            ctx.copy_text(text);
            self.set_status(ctx, format!("Copied {n} row(s) to the clipboard"), false);
        }
    }

    // ------------------------------------------------------------ panels

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open…            Ctrl+O").clicked() {
                    ui.close();
                    self.open_dialog(&ctx);
                }
                if ui.button("Open EVTX folder…").on_hover_text("Parse every .evtx below a folder into one merged timeline").clicked() {
                    ui.close();
                    self.open_folder_dialog(&ctx);
                }
                ui.menu_button("Recent files", |ui| {
                    let recent = self.settings.recent.clone();
                    if recent.is_empty() {
                        ui.weak("(none)");
                    }
                    for p in recent {
                        if ui.button(p.display().to_string()).clicked() {
                            ui.close();
                            self.open_any(&ctx, p);
                        }
                    }
                });
                ui.separator();
                if ui.button("Load session…    Ctrl+Shift+O").clicked() {
                    ui.close();
                    self.load_session_dialog(&ctx);
                }
                if ui.button("Save session      Ctrl+S").clicked() {
                    ui.close();
                    self.save_session(&ctx, false);
                }
                if ui.button("Save session as…  Ctrl+Shift+S").clicked() {
                    ui.close();
                    self.save_session(&ctx, true);
                }
                ui.separator();
                if ui.button("Export visible rows to CSV…").clicked() {
                    ui.close();
                    self.export(&ctx, false);
                }
                if ui.button("Export tagged rows to CSV…").clicked() {
                    ui.close();
                    self.export(&ctx, true);
                }
                ui.separator();
                if ui.button("Close tab         Ctrl+W").clicked() {
                    ui.close();
                    self.close_tab(self.active);
                }
                if ui.button("Close all tabs    Ctrl+Q").clicked() {
                    ui.close();
                    self.tabs.clear();
                }
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                let has = self.view().is_some();
                ui.add_enabled_ui(has, |ui| {
                    if ui.button("Copy rows        Ctrl+C").clicked() {
                        if let Some(v) = self.view() {
                            ctx.copy_text(v.copy_rows(true));
                        }
                        ui.close();
                    }
                    if ui.button("Select all        Ctrl+A").clicked() {
                        if let Some(v) = self.view() {
                            v.select_all();
                        }
                        ui.close();
                    }
                    if ui.button("Go to line…      Ctrl+G").clicked() {
                        self.dlg.goto = Some(String::new());
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Tag/untag selection  Ctrl+T").clicked() {
                        if let Some(v) = self.view() {
                            v.toggle_tags();
                        }
                        ui.close();
                    }
                    if ui.button("Clear all tags").clicked() {
                        if let Some(v) = self.view() {
                            v.clear_tags();
                        }
                        ui.close();
                    }
                });
            });
            ui.menu_button("View", |ui| {
                let has = self.view().is_some();
                ui.add_enabled_ui(has, |ui| {
                    if ui.button("Column chooser…   Ctrl+K").clicked() {
                        self.dlg.columns.open = true;
                        ui.close();
                    }
                    if ui.button("Filter editor…    Ctrl+L").clicked() {
                        if let Some(v) = self.view() {
                            let node = v.filter_tree.clone();
                            self.dlg.filter_editor = Some(FilterEditor { node });
                        }
                        ui.close();
                    }
                    if ui.button("Clear all filters  Ctrl+E").clicked() {
                        if let Some(v) = self.view() {
                            v.clear_filters();
                        }
                        ui.close();
                    }
                    if ui.button("Conditional formatting…").clicked() {
                        if let Some(v) = self.view() {
                            let rules = v.rules.clone();
                            self.dlg.rules = Some(RulesManager { rules });
                        }
                        ui.close();
                    }
                    ui.separator();
                    if let Some(v) = self.view() {
                        if ui.checkbox(&mut v.tagged_only, "Show only tagged rows  Ctrl+Shift+T").changed() {
                            v.mark_dirty();
                        }
                        if !v.group_by.is_empty() {
                            if ui.button("Expand all groups").clicked() {
                                v.set_all_groups(false);
                                ui.close();
                            }
                            if ui.button("Collapse all groups").clicked() {
                                v.set_all_groups(true);
                                ui.close();
                            }
                            if ui.button("Clear grouping").clicked() {
                                v.group_by.clear();
                                v.mark_dirty();
                                ui.close();
                            }
                        }
                        if ui.button("Best fit all columns  Ctrl+R").clicked() {
                            v.ui.needs_autofit = true;
                            ui.close();
                        }
                    }
                });
                ui.checkbox(&mut self.details, "Details pane  Ctrl+D");
                ui.separator();
                let mut changed = false;
                changed |= ui.checkbox(&mut self.settings.show_filter_row, "Auto-filter row").changed();
                changed |= ui.checkbox(&mut self.settings.show_group_panel, "Group-by panel").changed();
                changed |= ui.checkbox(&mut self.settings.plaso_colors, "Plaso colour coding").changed();
                changed |= ui
                    .checkbox(&mut self.settings.merge_evtx, "Merge EVTX files opened together")
                    .on_hover_text("Several .evtx dropped at once become one tab sorted by time")
                    .changed();
                if changed {
                    self.settings.save();
                }
            });
            ui.menu_button("Tools", |ui| {
                if ui.button("Settings (skins, datetime format, …)").clicked() {
                    self.dlg.settings = true;
                    ui.close();
                }
                ui.menu_button("Skin", |ui| {
                    for (t, l) in [(ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light"), (ThemeChoice::System, "System")] {
                        if ui.radio_value(&mut self.settings.theme, t, l).clicked() {
                            self.apply_theme(&ctx);
                            self.settings.save();
                            ui.close();
                        }
                    }
                });
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Quick help").clicked() {
                    self.dlg.help = true;
                    ui.close();
                }
                if ui.button("Plaso colour legend").clicked() {
                    self.dlg.legend = true;
                    ui.close();
                }
                if ui.button("About").clicked() {
                    self.dlg.about = true;
                    ui.close();
                }
            });

            // Search box on the right (like Timeline Explorer's top-right search).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let focus = std::mem::take(&mut self.focus_search);
                let help = &mut self.dlg.help;
                let v = match self.tabs.get_mut(self.active).map(|t| &mut t.content) {
                    Some(TabContent::Ready(v)) => v,
                    _ => {
                        ui.weak("No file open");
                        return;
                    }
                };
                if ui.button("?").on_hover_text("Search syntax").clicked() {
                    *help = true;
                }
                let mut changed = false;
                ui.menu_button("⚙", |ui| {
                    ui.label(RichText::new("Behaviour").strong());
                    changed |= ui.radio_value(&mut v.search_opts.behavior, SearchBehavior::Filter, "Filter rows").changed();
                    changed |= ui.radio_value(&mut v.search_opts.behavior, SearchBehavior::Highlight, "Highlight only (F3 to navigate)").changed();
                    ui.separator();
                    ui.label(RichText::new("Condition").strong());
                    for (c, l) in [
                        (SearchCondition::Contains, "Contains"),
                        (SearchCondition::StartsWith, "Starts with"),
                        (SearchCondition::Equals, "Equals (whole cell)"),
                        (SearchCondition::Like, "Like (* ? wildcards)"),
                        (SearchCondition::Regex, "Regular expression"),
                    ] {
                        changed |= ui.radio_value(&mut v.search_opts.condition, c, l).changed();
                    }
                    ui.separator();
                    ui.label(RichText::new("Match").strong());
                    changed |= ui.radio_value(&mut v.search_opts.match_mode, MatchMode::Any, "Any word").changed();
                    changed |= ui.radio_value(&mut v.search_opts.match_mode, MatchMode::All, "All words").changed();
                    changed |= ui.radio_value(&mut v.search_opts.match_mode, MatchMode::Exact, "Exact phrase").changed();
                    ui.separator();
                    changed |= ui.checkbox(&mut v.search_opts.case_sensitive, "Case sensitive").changed();
                });
                if !v.search.is_empty() && ui.small_button("✕").clicked() {
                    v.search.clear();
                    changed = true;
                }
                let r = ui.add(
                    TextEdit::singleline(&mut v.search)
                        .id(search_id())
                        .hint_text("Search (Ctrl+F)")
                        .desired_width(300.0),
                );
                if focus {
                    r.request_focus();
                }
                if r.changed() {
                    v.ui.filter_edit_at = Some(ui.input(|i| i.time));
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    v.refresh_now();
                    v.next_match(!ui.input(|i| i.modifiers.shift));
                    r.request_focus();
                }
                if changed {
                    v.mark_dirty();
                }
            });
        });
    }

    fn tabs_bar(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        ui.horizontal(|ui| {
            if self.tabs.len() > 3 {
                ui.add(TextEdit::singleline(&mut self.tab_filter).hint_text("Filter tabs").desired_width(120.0));
            }
            egui::ScrollArea::horizontal().id_salt("tabs_scroll").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let needle = self.tab_filter.to_lowercase();
                    for (i, t) in self.tabs.iter().enumerate() {
                        if !needle.is_empty() && !t.title.to_lowercase().contains(&needle) {
                            continue;
                        }
                        let label = match &t.content {
                            TabContent::Loading { progress, .. } => format!("⏳ {} {:.0}%", t.title, progress.fraction() * 100.0),
                            TabContent::Failed(_) => format!("⚠ {}", t.title),
                            TabContent::Ready(v) if v.tag_count > 0 => format!("{} ({}🏷)", t.title, v.tag_count),
                            TabContent::Ready(_) => t.title.clone(),
                        };
                        let r = ui.selectable_label(i == self.active, label).on_hover_text(t.path.display().to_string());
                        if r.clicked() {
                            self.active = i;
                        }
                        if r.middle_clicked() {
                            close = Some(i);
                        }
                        if ui.small_button("✕").on_hover_text("Close (Ctrl+W)").clicked() {
                            close = Some(i);
                        }
                        ui.separator();
                    }
                });
            });
        });
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let status = self.status.clone();
        ui.horizontal(|ui| {
            let Some(tab) = self.tabs.get(self.active) else {
                ui.weak("Ready — drop files here or use File > Open");
                return;
            };
            let path = tab.path.display().to_string();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let TabContent::Ready(v) = &self.tabs[self.active].content {
                    let rows = v.display.iter().filter(|d| matches!(d, Disp::Row(_))).count();
                    ui.label(format!(
                        "Total lines: {}   Visible lines: {}   Tagged: {}   Selected: {}",
                        v.table.nrows, rows, v.tag_count, v.sel_count
                    ))
                    .on_hover_text(format!(
                        "Filter/sort time: {} ms\nFile size: {:.1} MB",
                        v.last_refresh_ms,
                        v.table.file_size as f64 / 1e6
                    ));
                    if v.tagged_only {
                        ui.colored_label(ui.visuals().hyperlink_color, "[tagged only]");
                    }
                }
                if let Some((msg, t, err)) = &status
                    && now - t < 6.0
                {
                    ui.separator();
                    let color = if *err { Color32::LIGHT_RED } else { ui.visuals().hyperlink_color };
                    ui.colored_label(color, msg);
                }
                ui.separator();
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let r = ui
                        .add(egui::Label::new(RichText::new(&path).weak()).sense(egui::Sense::click()).truncate())
                        .on_hover_text(format!("{path}\nDouble-click to copy the path"));
                    if r.double_clicked() {
                        ctx.copy_text(path.clone());
                        self.status = Some(("Path copied to the clipboard".into(), now, false));
                    }
                });
            });
        });
    }

    fn filter_bar(ui: &mut egui::Ui, v: &mut TabView, open_editor: &mut bool) {
        ui.horizontal(|ui| {
            if ui.checkbox(&mut v.filters_enabled, "").on_hover_text("Enable / disable the filter (Ctrl+Shift+E)").changed() {
                v.mark_dirty();
            }
            let desc = v.filter_description();
            let text = if v.filters_enabled { RichText::new(&desc) } else { RichText::new(&desc).strikethrough().weak() };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕ Clear").on_hover_text("Clear all filters (Ctrl+E)").clicked() {
                    v.clear_filters();
                }
                if ui.button("Edit filter").clicked() {
                    *open_editor = true;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(text).truncate()).on_hover_text(desc);
                });
            });
        });
    }

    fn group_panel(ui: &mut egui::Ui, v: &mut TabView) {
        let dragging = v.ui.dragging_col.is_some();
        let frame = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 4))
            .fill(if dragging { ui.visuals().selection.bg_fill.gamma_multiply(0.35) } else { ui.visuals().faint_bg_color });
        let r = frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                if v.group_by.is_empty() {
                    ui.weak("Drag a column header here to group by that column");
                    return;
                }
                ui.weak("Grouped by:");
                let mut remove = None;
                for (i, &c) in v.group_by.iter().enumerate() {
                    if i > 0 {
                        ui.weak("›");
                    }
                    if ui.button(format!("{}  ✕", v.table.headers[c])).on_hover_text("Click to ungroup").clicked() {
                        remove = Some(i);
                    }
                }
                if let Some(i) = remove {
                    v.group_by.remove(i);
                    v.mark_dirty();
                }
                ui.separator();
                if ui.small_button("Expand all").clicked() {
                    v.set_all_groups(false);
                }
                if ui.small_button("Collapse all").clicked() {
                    v.set_all_groups(true);
                }
                if ui.small_button("Clear grouping").clicked() {
                    v.group_by.clear();
                    v.mark_dirty();
                }
            });
        });
        v.ui.group_panel_rect = Some(r.response.rect);
    }

    fn details_panel(&mut self, ui: &mut egui::Ui) {
        let filter = &mut self.details_filter;
        let hide_empty = &mut self.details_hide_empty;
        let mut close = false;
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let TabContent::Ready(v) = &mut tab.content else { return };
        ui.horizontal(|ui| {
            ui.heading("Details");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").clicked() {
                    close = true;
                }
            });
        });
        let Some(r) = v.cursor_row() else {
            ui.weak("Select a row");
            return;
        };
        let t = v.table.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Line {}", r + 1)).strong());
            let mut tag = v.tags[r];
            if ui.checkbox(&mut tag, "Tagged").changed() {
                v.set_tag(r, tag);
            }
            if ui.button("📋 Copy all").clicked() {
                let s: String = t.headers.iter().zip(t.row_values(r)).map(|(h, val)| format!("{h}: {val}\n")).collect();
                ui.ctx().copy_text(s);
            }
        });
        ui.horizontal(|ui| {
            ui.add(TextEdit::singleline(filter).hint_text("Filter fields").desired_width(160.0));
            ui.checkbox(hide_empty, "Hide empty");
        });
        ui.separator();
        let needle = filter.to_lowercase();
        let hidden: Vec<bool> = {
            let mut h = vec![true; t.ncols()];
            for c in v.visible_cols() {
                h[c] = false;
            }
            h
        };
        let order: Vec<usize> = v.columns.iter().map(|c| c.col).collect();
        let settings = &mut self.settings;
        let display_time = &self.display_time;
        ui.horizontal(|ui| {
            ui.checkbox(&mut settings.details_wrap, "Wrap values");
            ui.weak("· drag the divider to resize, double-click it to fit names");
        });
        let avail_w = ui.available_width();
        settings.details_name_width = settings.details_name_width.clamp(50.0, (avail_w - 90.0).max(50.0));
        let name_w = settings.details_name_width;
        let left = ui.cursor().left();
        let list_top = ui.cursor().top();
        let gap = ui.spacing().item_spacing.x;
        let field_row = |ui: &mut egui::Ui, w: f32, text: RichText| {
            ui.allocate_ui_with_layout(vec2(w, 0.0), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                ui.set_min_width(w);
                ui.set_max_width(w);
                ui.add(egui::Label::new(text).truncate())
            })
            .inner
        };
        ui.horizontal_top(|ui| {
            field_row(ui, name_w, RichText::new("Field").weak());
            ui.label(RichText::new("Value").weak());
        });
        let stripe = ui.visuals().faint_bg_color;
        let mut longest_name = 0.0f32;
        let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let mut k = 0;
            for c in order {
                let val = t.cell(r, c);
                if *hide_empty && val.trim().is_empty() {
                    continue;
                }
                if !needle.is_empty() && !t.headers[c].to_lowercase().contains(&needle) && !val.to_lowercase().contains(&needle) {
                    continue;
                }
                let bg = ui.painter().add(egui::Shape::Noop);
                let row = ui.horizontal_top(|ui| {
                    let name = RichText::new(&t.headers[c]).strong();
                    let lbl = field_row(ui, name_w, if hidden[c] { name.weak() } else { name });
                    longest_name = longest_name.max(
                        ui.painter().layout_no_wrap(t.headers[c].clone(), egui::TextStyle::Body.resolve(ui.style()), Color32::WHITE).size().x,
                    );
                    if hidden[c] {
                        lbl.on_hover_text(format!("{} (hidden column)", t.headers[c]));
                    }
                    if ui.small_button("📋").on_hover_text("Copy value").clicked() {
                        ui.ctx().copy_text(val.to_string());
                    }
                    let mut shown = match (t.col_types[c], settings.reformat_dates) {
                        (data::ColType::DateTime, true) => display_time.format(val).unwrap_or_else(|| val.to_string()),
                        _ => val.to_string(),
                    };
                    if settings.details_wrap && t.headers[c] == "EventData" {
                        shown = shown.replace(" | ", "\n");
                    }
                    let rest = ui.available_width().max(40.0);
                    ui.allocate_ui_with_layout(vec2(rest, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_max_width(rest);
                        let label = egui::Label::new(shown).selectable(true);
                        ui.add(if settings.details_wrap { label.wrap() } else { label.truncate() });
                    });
                });
                if k % 2 == 1 {
                    let rr = row.response.rect.expand2(vec2(2.0, 1.0));
                    ui.painter().set(bg, egui::Shape::rect_filled(rr, 2.0, stripe));
                }
                k += 1;
            }
        });
        // Draggable divider between field names and values.
        let x = left + name_w + gap / 2.0;
        let handle = egui::Rect::from_min_max(egui::pos2(x - 4.0, list_top), egui::pos2(x + 4.0, out.inner_rect.bottom()));
        let resp = ui.interact(handle, egui::Id::unique("details_divider"), egui::Sense::click_and_drag());
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if resp.dragged() {
            settings.details_name_width = (name_w + resp.drag_delta().x).clamp(50.0, (avail_w - 90.0).max(50.0));
        }
        if resp.double_clicked() && longest_name > 0.0 {
            settings.details_name_width = (longest_name + 12.0).clamp(50.0, (avail_w - 90.0).max(50.0));
        }
        if resp.drag_stopped() || resp.double_clicked() {
            settings.save();
        }
        let stroke = if resp.hovered() || resp.dragged() {
            egui::Stroke::new(2.0, ui.visuals().hyperlink_color)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        };
        ui.painter().vline(x, handle.y_range(), stroke);
        if close {
            self.details = false;
        }
    }
}

impl TabView {
    /// Refresh immediately, cancelling a pending debounced refresh.
    pub fn refresh_now(&mut self) {
        if self.ui.filter_edit_at.take().is_some() {
            self.dirty = true;
        }
        self.refresh();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Dropped files
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            self.open_many(&ctx, dropped);
        }
        self.poll_loading(&ctx);
        self.shortcuts(&ctx);

        // Debounced filter/search typing
        let now = ctx.input(|i| i.time);
        if let Some(v) = self.view()
            && let Some(t) = v.ui.filter_edit_at
        {
            if now - t > 0.25 {
                v.ui.filter_edit_at = None;
                v.mark_dirty();
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(60));
            }
        }
        if let Some(v) = self.view() {
            v.refresh();
        }

        // Window title
        let title = match self.tabs.get(self.active) {
            Some(t) => format!("TLX — Timeline eXplorer — {}", t.path.display()),
            None => "TLX — Timeline eXplorer".to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        if !self.tabs.is_empty() {
            egui::Panel::top("tabs").show(ui, |ui| self.tabs_bar(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));

        let mut open_editor = false;
        if let Some(v) = self.view()
            && (v.has_filters() || !v.errors.is_empty()) {
                egui::Panel::bottom("filterbar").show(ui, |ui| {
                    for e in &v.errors {
                        ui.colored_label(Color32::LIGHT_RED, format!("⚠ {e}"));
                    }
                    if v.has_filters() {
                        Self::filter_bar(ui, v, &mut open_editor);
                    }
                });
            }
        if open_editor && let Some(v) = self.view() {
            let node = v.filter_tree.clone();
            self.dlg.filter_editor = Some(FilterEditor { node });
        }
        if self.details && self.view().is_some() {
            egui::Panel::right("details").resizable(true).default_size(380.0).show(ui, |ui| self.details_panel(ui));
        }

        let show_group = self.settings.show_group_panel;
        let cfg_font = egui::FontId::proportional(self.settings.font_size);
        let row_h = (self.settings.font_size + 9.0).round();
        let show_filter_row = self.settings.show_filter_row;
        let time = self.settings.reformat_dates.then_some(&self.display_time);
        let plaso_colors = self.settings.plaso_colors;
        let mut actions = vec![];
        let tabs_empty = self.tabs.is_empty();
        let recent = self.settings.recent.clone();
        let mut open_recent = None;
        let mut want_open = false;

        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(0)).show(ui, |ui| {
            if tabs_empty {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.25);
                    ui.heading("TLX — Timeline eXplorer");
                    ui.add_space(8.0);
                    ui.label("Drop CSV / TSV / Excel / EVTX files, a folder of EVTX, or a session (.tlxs) here");
                    if ui.button("Open file…  (Ctrl+O)").clicked() {
                        want_open = true;
                    }
                    if !recent.is_empty() {
                        ui.add_space(16.0);
                        ui.weak("Recent files");
                        for p in recent.iter().take(8) {
                            if ui.link(p.display().to_string()).clicked() {
                                open_recent = Some(p.clone());
                            }
                        }
                    }
                });
                return;
            }
            let dark = ui.visuals().dark_mode;
            match &mut self.tabs[self.active].content {
                TabContent::Loading { progress, .. } => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.3);
                        let recs = progress.records.load(Ordering::Relaxed);
                        if recs > 0 {
                            ui.label(format!("Parsing event logs… {recs} records"));
                        } else {
                            ui.label(format!("Loading… {:.1} MB", progress.bytes.load(Ordering::Relaxed) as f64 / 1e6));
                        }
                        ui.add(egui::ProgressBar::new(progress.fraction()).show_percentage().desired_width(400.0));
                        if ui.button("Cancel").clicked() {
                            progress.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                }
                TabContent::Failed(e) => {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.colored_label(Color32::LIGHT_RED, format!("Could not open the file:\n{e}"));
                    });
                }
                TabContent::Ready(v) => {
                    if show_group {
                        Self::group_panel(ui, v);
                    } else {
                        v.ui.group_panel_rect = None;
                    }
                    let cfg = GridConfig { font: cfg_font.clone(), row_h, show_filter_row, time, plaso_colors, dark };
                    actions = grid::show(ui, v, &cfg);
                }
            }
        });
        if want_open {
            self.open_dialog(&ctx);
        }
        if let Some(p) = open_recent {
            self.open_any(&ctx, p);
        }

        for a in actions {
            match a {
                GridAction::ShowDetails => self.details = true,
                GridAction::ColumnChooser => self.dlg.columns.open = true,
                GridAction::FilterEditor => {
                    if let Some(v) = self.view() {
                        let node = v.filter_tree.clone();
                        self.dlg.filter_editor = Some(FilterEditor { node });
                    }
                }
                GridAction::RuleDialog(r) => self.dlg.rule = Some(r),
                GridAction::ManageRules => {
                    if let Some(v) = self.view() {
                        let rules = v.rules.clone();
                        self.dlg.rules = Some(RulesManager { rules });
                    }
                }
                GridAction::SetSearch(s) => {
                    if let Some(v) = self.view() {
                        v.search = s;
                        v.mark_dirty();
                    }
                }
            }
        }

        // Windows
        let mut goto = None;
        if let Some(tab) = self.tabs.get_mut(self.active)
            && let TabContent::Ready(v) = &mut tab.content
        {
            grid::filter_popup_ui(&ctx, v);
            if self.dlg.columns.open {
                dialogs::column_chooser(&ctx, &mut self.dlg.columns, v);
            }
            dialogs::filter_editor(&ctx, &mut self.dlg.filter_editor, v);
            dialogs::rules_manager(&ctx, &mut self.dlg.rules, v);
            dialogs::rule_dialog(&ctx, &mut self.dlg.rule, v);
            goto = dialogs::goto_window(&ctx, &mut self.dlg.goto).map(|l| v.go_to_line(l));
        }
        if let Some(Err(e)) = goto {
            self.set_status(&ctx, e, true);
        }
        if self.dlg.settings && dialogs::settings_window(&ctx, &mut self.dlg.settings, &mut self.settings) {
            self.apply_theme(&ctx);
            self.settings.save();
            for t in &mut self.tabs {
                if let TabContent::Ready(v) = &mut t.content {
                    v.ui.needs_autofit = true;
                }
            }
        }
        if self.dlg.help {
            dialogs::help_window(&ctx, &mut self.dlg.help);
        }
        if self.dlg.legend {
            let dark = ctx.global_style().visuals.dark_mode;
            dialogs::legend_window(&ctx, &mut self.dlg.legend, dark);
        }
        if self.dlg.about {
            dialogs::about_window(&ctx, &mut self.dlg.about);
        }

        // Drop hint overlay
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Drop to open", egui::FontId::proportional(28.0), Color32::WHITE);
        }
    }
}

/// Deepest directory containing every path (label of merged tabs).
fn common_parent(paths: &[PathBuf]) -> PathBuf {
    let mut base = paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
    while !paths.iter().all(|p| p.starts_with(&base)) {
        if !base.pop() {
            break;
        }
    }
    base
}
