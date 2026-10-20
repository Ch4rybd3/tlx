//! Virtualised data grid: only the visible rows are laid out each frame, so
//! multi-million row timelines scroll smoothly. Scrolling is row-based (not
//! pixel-based) to avoid f32 precision issues on huge tables.

use std::collections::HashSet;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextEdit, Ui, UiBuilder, Vec2, pos2, vec2};

use crate::data::{ColType, Table, TableKind};
use crate::filter::{ColumnFilter, Condition, Op, ValueFilter};
use crate::format::{self, CondRule, Preset, RuleScope};
use crate::timefmt::DisplayTime;
use crate::view::{Disp, TabView};

const SB: f32 = 12.0; // scrollbar thickness
const TAG_W: f32 = 26.0;
const MAX_CELL_CHARS: usize = 400;

#[derive(Default, Clone, Copy, Debug)]
enum Drag {
    #[default]
    None,
    Resize { vis: usize, start_w: f32, start_x: f32 },
    MoveCol { col: usize },
    VThumb { grab: f32 },
    HThumb { grab: f32 },
    Select,
}

#[derive(Clone, Copy, Debug)]
pub enum CtxTarget {
    Header(usize),
    Cell { col: Option<usize> },
}

pub struct FilterPopup {
    pub col: usize,
    pub pos: Pos2,
    pub values: Vec<(String, u32)>,
    pub checked: Vec<bool>,
    pub search: String,
    pub highlighted: HashSet<usize>,
    pub anchor: Option<usize>,
    pub only_visible: bool,
}

#[derive(Default)]
pub struct GridState {
    pub needs_autofit: bool,
    drag: Drag,
    wheel_accum: f32,
    pub ctx_target: Option<CtxTarget>,
    pub filter_popup: Option<FilterPopup>,
    /// Data column currently dragged from the header (for the group panel).
    pub dragging_col: Option<usize>,
    pub group_panel_rect: Option<Rect>,
    pub filter_edit_at: Option<f64>,
    move_target: Option<usize>,
}

pub struct GridConfig<'a> {
    pub font: FontId,
    pub row_h: f32,
    pub show_filter_row: bool,
    pub time: Option<&'a DisplayTime>,
    pub plaso_colors: bool,
    pub dark: bool,
}

pub enum GridAction {
    ShowDetails,
    ColumnChooser,
    FilterEditor,
    RuleDialog(CondRule),
    ManageRules,
    SetSearch(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Line,
    Tag,
    Col(usize),
}

struct Layout {
    inner: Rect,
    header: Rect,
    filter_row: Rect,
    body: Rect,
    line_w: f32,
    fixed_w: f32,
    /// (data col, screen x, width, frozen)
    cols: Vec<(usize, f32, f32, bool)>,
    visible_rows: usize,
    max_top: usize,
    scroll_total: f32,
    scroll_view_w: f32,
    vtrack: Rect,
    htrack: Rect,
}

impl Layout {
    fn hit_x(&self, x: f32) -> Option<Hit> {
        let left = self.inner.left();
        if x < left || x > self.inner.right() {
            return None;
        }
        if x < left + self.line_w {
            return Some(Hit::Line);
        }
        if x < left + self.line_w + TAG_W {
            return Some(Hit::Tag);
        }
        // Frozen first (drawn on top), then scrollable columns.
        for (i, (_, cx, w, frozen)) in self.cols.iter().enumerate() {
            if *frozen && x >= *cx && x < cx + w {
                return Some(Hit::Col(i));
            }
        }
        if x >= left + self.fixed_w {
            for (i, (_, cx, w, frozen)) in self.cols.iter().enumerate() {
                if !*frozen && x >= *cx && x < cx + w {
                    return Some(Hit::Col(i));
                }
            }
        }
        None
    }

    fn row_at(&self, y: f32, row_h: f32) -> Option<usize> {
        if y < self.body.top() || y >= self.body.bottom() {
            return None;
        }
        Some(((y - self.body.top()) / row_h) as usize)
    }

    fn scroll_clip(&self) -> Rect {
        Rect::from_min_max(pos2(self.inner.left() + self.fixed_w, self.inner.top()), self.inner.max)
    }

    fn frozen_clip(&self) -> Rect {
        Rect::from_min_max(self.inner.min, pos2(self.inner.left() + self.fixed_w, self.inner.bottom()))
    }

    fn clip_for(&self, frozen: bool) -> Rect {
        if frozen { self.frozen_clip() } else { self.scroll_clip() }
    }
}

fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CELL_CHARS + 4));
    for (n, c) in s.chars().enumerate() {
        if n >= MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        out.push(match c {
            '\r' | '\n' | '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        });
    }
    out
}

pub fn display_text(t: &Table, row: usize, col: usize, time: Option<&DisplayTime>) -> String {
    let raw = t.cell(row, col);
    if let (Some(dt), ColType::DateTime) = (time, t.col_types[col])
        && let Some(f) = dt.format(raw)
    {
        return f;
    }
    sanitize(raw)
}

fn cell_job(text: &str, ranges: &[(usize, usize)], font: &FontId, color: Color32, hl_bg: Color32, hl_fg: Color32, max_w: f32) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: TextWrapping { max_width: max_w.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') },
        ..Default::default()
    };
    let normal = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hl = TextFormat { font_id: font.clone(), color: hl_fg, background: hl_bg, ..Default::default() };
    let mut pos = 0;
    for &(s, e) in ranges {
        if s > pos {
            job.append(&text[pos..s], 0.0, normal.clone());
        }
        job.append(&text[s..e], 0.0, hl.clone());
        pos = e;
    }
    if pos < text.len() || text.is_empty() {
        job.append(&text[pos..], 0.0, normal);
    }
    job
}

/// Width needed to show the longest value of each column (header included).
pub fn autofit(ui: &Ui, view: &mut TabView, only: Option<usize>, cfg: &GridConfig) {
    let t = view.table.clone();
    let sample: Vec<usize> = {
        let mut s: Vec<usize> = view.filtered.iter().take(400).map(|&r| r as usize).collect();
        s.extend(view.display.iter().skip(view.top).take(100).filter_map(|d| match d {
            Disp::Row(r) => Some(*r as usize),
            _ => None,
        }));
        s
    };
    let painter = ui.painter();
    for cs in view.columns.iter_mut() {
        if only.is_some_and(|c| c != cs.col) {
            continue;
        }
        let longest = sample
            .iter()
            .map(|&r| display_text(&t, r, cs.col, cfg.time))
            .max_by_key(|s| s.chars().count())
            .unwrap_or_default();
        let wv = painter.layout_no_wrap(longest.chars().take(80).collect(), cfg.font.clone(), Color32::WHITE).size().x;
        let wh = painter.layout_no_wrap(t.headers[cs.col].clone(), cfg.font.clone(), Color32::WHITE).size().x + 34.0;
        cs.width = wv.max(wh).clamp(40.0, 650.0) + 12.0;
    }
}

fn sort_click(view: &mut TabView, col: usize, shift: bool, ctrl: bool) {
    let pos = view.sort.iter().position(|s| s.0 == col);
    if ctrl {
        if let Some(p) = pos {
            view.sort.remove(p);
        }
    } else if shift {
        match pos {
            Some(p) => view.sort[p].1 = !view.sort[p].1,
            None => view.sort.push((col, false)),
        }
    } else {
        let desc = match pos {
            Some(p) if view.sort.len() == 1 => !view.sort[p].1,
            _ => false,
        };
        view.sort = vec![(col, desc)];
    }
    view.mark_dirty();
}

pub fn open_filter_popup(view: &mut TabView, col: usize, pos: Pos2, only_visible: bool) {
    let rows = if only_visible { Some(view.filtered.as_slice()) } else { None };
    let values = view.table.distinct(col, rows);
    let current = view.col_filters.get(&col).and_then(|f| f.values.clone());
    let checked = values
        .iter()
        .map(|(v, _)| match &current {
            None => true,
            Some(vf) => vf.set.contains(v) == vf.include,
        })
        .collect();
    view.ui.filter_popup =
        Some(FilterPopup { col, pos, values, checked, search: String::new(), highlighted: HashSet::new(), anchor: None, only_visible });
}

fn apply_popup(view: &mut TabView) {
    let Some(p) = &view.ui.filter_popup else { return };
    let col = p.col;
    let n_checked = p.checked.iter().filter(|c| **c).count();
    let vf = if n_checked == p.values.len() {
        None
    } else if n_checked * 2 <= p.values.len() {
        Some(ValueFilter {
            include: true,
            set: p.values.iter().zip(&p.checked).filter(|(_, c)| **c).map(|(v, _)| v.0.clone()).collect(),
        })
    } else {
        Some(ValueFilter {
            include: false,
            set: p.values.iter().zip(&p.checked).filter(|(_, c)| !**c).map(|(v, _)| v.0.clone()).collect(),
        })
    };
    // Values not present in a "visible only" list must not be silently dropped.
    let vf = match (vf, p.only_visible) {
        (Some(mut f), true) if f.include => {
            if let Some(old) = view.col_filters.get(&col).and_then(|c| c.values.as_ref()).filter(|o| o.include) {
                let listed: HashSet<&String> = p.values.iter().map(|v| &v.0).collect();
                f.set.extend(old.set.iter().filter(|v| !listed.contains(v)).cloned());
            }
            Some(f)
        }
        (f, _) => f,
    };
    let e = view.col_filters.entry(col).or_default();
    e.values = vf;
    if !e.is_active() {
        view.col_filters.remove(&col);
    }
    view.mark_dirty();
}

/// Header drop-down filter window (instant filtering, like Timeline Explorer).
pub fn filter_popup_ui(ctx: &egui::Context, view: &mut TabView) {
    let Some(p) = &mut view.ui.filter_popup else { return };
    let title = format!("Filter: {}", view.table.headers[p.col]);
    let mut open = true;
    let mut changed = false;
    let mut close = false;
    let mut reload: Option<bool> = None;
    egui::Window::new(title)
        .id(egui::Id::unique(("col_filter_popup", p.col)))
        .open(&mut open)
        .default_pos(p.pos)
        .default_width(320.0)
        .default_height(420.0)
        .collapsible(false)
        .resizable(true)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔍");
                ui.add(TextEdit::singleline(&mut p.search).hint_text("Search values").desired_width(f32::INFINITY));
            });
            let needle = p.search.to_lowercase();
            let shown: Vec<usize> = (0..p.values.len())
                .filter(|&i| needle.is_empty() || p.values[i].0.to_lowercase().contains(&needle))
                .collect();
            ui.horizontal(|ui| {
                let all = shown.iter().all(|&i| p.checked[i]);
                let mut a = all;
                if ui.checkbox(&mut a, "(Select All)").changed() {
                    for &i in &shown {
                        p.checked[i] = a;
                    }
                    changed = true;
                }
                ui.weak(format!("{} values", p.values.len()));
            });
            let mut vis_only = p.only_visible;
            if ui
                .checkbox(&mut vis_only, "Only values of visible rows")
                .on_hover_text("List only the values present in the currently filtered rows")
                .changed()
            {
                reload = Some(vis_only);
            }
            ui.separator();
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
            let avail = ui.available_height() - 36.0;
            egui::ScrollArea::vertical().max_height(avail.max(80.0)).auto_shrink([false, false]).show_rows(
                ui,
                row_h,
                shown.len(),
                |ui, range| {
                    for k in range {
                        let i = shown[k];
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut p.checked[i], "").changed() {
                                changed = true;
                            }
                            let (v, n) = &p.values[i];
                            let label = if v.is_empty() { "(Blanks)".to_string() } else { sanitize(v) };
                            let hl = p.highlighted.contains(&i);
                            let r = ui.selectable_label(hl, format!("{label}   ({n})"));
                            if r.clicked() {
                                let m = ui.input(|i| i.modifiers);
                                if m.shift && let Some(a) = p.anchor {
                                    let (pa, pb) = (shown.iter().position(|&x| x == a).unwrap_or(k), k);
                                    for &j in &shown[pa.min(pb)..=pa.max(pb)] {
                                        p.highlighted.insert(j);
                                    }
                                } else if m.command {
                                    if !p.highlighted.remove(&i) {
                                        p.highlighted.insert(i);
                                    }
                                    p.anchor = Some(i);
                                } else {
                                    p.highlighted.clear();
                                    p.highlighted.insert(i);
                                    p.anchor = Some(i);
                                }
                            }
                            if r.double_clicked() {
                                // Double-click: keep only this value.
                                p.checked.iter_mut().for_each(|c| *c = false);
                                p.checked[i] = true;
                                changed = true;
                            }
                        });
                    }
                },
            );
            // Space toggles every highlighted value at once (drag/shift select then space).
            if !p.highlighted.is_empty() && !ctx.egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space)) {
                let target = !p.highlighted.iter().all(|&i| p.checked[i]);
                for &i in &p.highlighted {
                    p.checked[i] = target;
                }
                changed = true;
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Clear filter").clicked() {
                    p.checked.iter_mut().for_each(|c| *c = true);
                    changed = true;
                }
                if ui.button("Invert").clicked() {
                    for &i in &shown {
                        p.checked[i] = !p.checked[i];
                    }
                    changed = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
    if changed {
        apply_popup(view);
    }
    if let Some(v) = reload {
        let (col, pos) = view.ui.filter_popup.as_ref().map(|p| (p.col, p.pos)).unwrap();
        open_filter_popup(view, col, pos, v);
    }
    if !open || close {
        view.ui.filter_popup = None;
    }
}

/// Draws the grid and handles its mouse interactions.
pub fn show(ui: &mut Ui, view: &mut TabView, cfg: &GridConfig) -> Vec<GridAction> {
    let mut actions = vec![];
    if view.ui.needs_autofit {
        autofit(ui, view, None, cfg);
        view.ui.needs_autofit = false;
    }
    let t = view.table.clone();
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let row_h = cfg.row_h;
    let visuals = ui.visuals().clone();
    let text_color = visuals.text_color();
    let weak = visuals.weak_text_color();
    let body_bg = visuals.extreme_bg_color;
    let stripe = visuals.faint_bg_color;
    let header_bg = visuals.widgets.inactive.weak_bg_fill;
    let line_color = visuals.widgets.noninteractive.bg_stroke.color;
    let sel_bg = visuals.selection.bg_fill;
    let sel_fg = visuals.selection.stroke.color;
    let accent = visuals.hyperlink_color;
    let (hl_bg, hl_fg) = (Color32::from_rgb(255, 214, 0), Color32::BLACK);

    // ---------------------------------------------------------- layout
    let digits = (t.nrows.max(1) as f64).log10().floor() as usize + 1;
    let digit_w = ui.painter().layout_no_wrap("0".into(), cfg.font.clone(), text_color).size().x;
    let line_w = (digits.max(4) as f32) * digit_w + 14.0;
    let header_h = row_h + 4.0;
    let filter_h = if cfg.show_filter_row { row_h + 4.0 } else { 0.0 };
    let inner = Rect::from_min_max(rect.min, pos2(rect.right() - SB, rect.bottom() - SB));
    let vis: Vec<(usize, f32)> = view.columns.iter().filter(|c| c.visible).map(|c| (c.col, c.width)).collect();
    let frozen_n = view.frozen.min(vis.len());
    let fixed_w = line_w + TAG_W + vis[..frozen_n].iter().map(|c| c.1).sum::<f32>();
    let scroll_total: f32 = vis[frozen_n..].iter().map(|c| c.1).sum();
    let scroll_view_w = (inner.width() - fixed_w).max(0.0);
    let max_h = (scroll_total - scroll_view_w).max(0.0);
    let body = Rect::from_min_max(pos2(inner.left(), inner.top() + header_h + filter_h), inner.max);
    let visible_rows = ((body.height() / row_h).floor() as usize).max(1);
    let len = view.display.len();
    let max_top = len.saturating_sub(visible_rows);

    if view.scroll_to_cursor {
        if view.cursor < view.top {
            view.top = view.cursor;
        } else if view.cursor >= view.top + visible_rows {
            view.top = view.cursor + 1 - visible_rows;
        }
        // Horizontal: bring the cursor column into view.
        if view.cursor_col >= frozen_n && view.cursor_col < vis.len() {
            let start: f32 = vis[frozen_n..view.cursor_col].iter().map(|c| c.1).sum();
            let w = vis[view.cursor_col].1;
            if start < view.h_offset {
                view.h_offset = start;
            } else if start + w > view.h_offset + scroll_view_w {
                view.h_offset = start + w - scroll_view_w;
            }
        }
        view.scroll_to_cursor = false;
    }

    // ---------------------------------------------------------- wheel
    if ui.rect_contains_pointer(rect) {
        let d = ui.input(|i| i.smooth_scroll_delta());
        if d != Vec2::ZERO {
            view.ui.wheel_accum += d.y;
            let steps = (view.ui.wheel_accum / row_h).trunc();
            if steps != 0.0 {
                view.ui.wheel_accum -= steps * row_h;
                view.top = (view.top as isize - steps as isize).max(0) as usize;
            }
            view.h_offset -= d.x;
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
    }
    view.top = view.top.min(max_top);
    view.h_offset = view.h_offset.clamp(0.0, max_h);

    let mut cols = Vec::with_capacity(vis.len());
    let mut x = inner.left() + line_w + TAG_W;
    for (i, &(c, w)) in vis.iter().enumerate() {
        if i == frozen_n {
            x = inner.left() + fixed_w - view.h_offset;
        }
        cols.push((c, x, w, i < frozen_n));
        x += w;
    }
    let lay = Layout {
        inner,
        header: Rect::from_min_size(inner.min, vec2(inner.width(), header_h)),
        filter_row: Rect::from_min_size(pos2(inner.left(), inner.top() + header_h), vec2(inner.width(), filter_h)),
        body,
        line_w,
        fixed_w,
        cols,
        visible_rows,
        max_top,
        scroll_total,
        scroll_view_w,
        vtrack: Rect::from_min_max(pos2(inner.right(), body.top()), pos2(rect.right(), body.bottom())),
        htrack: Rect::from_min_max(pos2(inner.left() + fixed_w, inner.bottom()), pos2(inner.right(), rect.bottom())),
    };

    // ---------------------------------------------------------- interactions
    let mods = ui.input(|i| i.modifiers);
    let hover = resp.hover_pos();
    let near_edge = |p: Pos2| -> Option<usize> {
        if !lay.header.contains(p) {
            return None;
        }
        lay.cols.iter().enumerate().find_map(|(i, (_, cx, w, frozen))| {
            let edge = cx + w;
            let clip = lay.clip_for(*frozen);
            ((p.x - edge).abs() <= 4.0 && edge >= clip.left() && edge <= clip.right() + 1.0).then_some(i)
        })
    };
    if let Some(p) = hover
        && near_edge(p).is_some()
    {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    let vthumb = vthumb_rect(&lay, view.top, len);
    let hthumb = hthumb_rect(&lay, view.h_offset);

    if resp.drag_started()
        && let Some(p) = ui.input(|i| i.pointer.press_origin())
    {
        view.ui.drag = if let Some(i) = near_edge(p) {
            Drag::Resize { vis: i, start_w: lay.cols[i].2, start_x: p.x }
        } else if vthumb.is_some_and(|r| r.contains(p)) {
            Drag::VThumb { grab: p.y - vthumb.unwrap().top() }
        } else if hthumb.is_some_and(|r| r.contains(p)) {
            Drag::HThumb { grab: p.x - hthumb.unwrap().left() }
        } else if lay.header.contains(p) {
            match lay.hit_x(p.x) {
                Some(Hit::Col(i)) => {
                    view.ui.dragging_col = Some(lay.cols[i].0);
                    Drag::MoveCol { col: lay.cols[i].0 }
                }
                _ => Drag::None,
            }
        } else if body.contains(p) {
            if let (Some(k), Some(hit)) = (lay.row_at(p.y, row_h), lay.hit_x(p.x))
                && hit != Hit::Tag {
                    let idx = view.top + k;
                    if idx < len && !mods.shift && !mods.command {
                        view.click_row(idx, false, false);
                    }
                }
            Drag::Select
        } else {
            Drag::None
        };
    }
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        match view.ui.drag {
            Drag::Resize { vis, start_w, start_x } => {
                let col = lay.cols[vis].0;
                if let Some(cs) = view.column_state(col) {
                    cs.width = (start_w + p.x - start_x).max(24.0);
                }
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            Drag::VThumb { grab } => {
                let thumb_h = vthumb.map(|r| r.height()).unwrap_or(20.0);
                let span = (lay.vtrack.height() - thumb_h).max(1.0);
                let f = ((p.y - grab - lay.vtrack.top()) / span).clamp(0.0, 1.0);
                view.top = (f as f64 * max_top as f64).round() as usize;
            }
            Drag::HThumb { grab } => {
                let thumb_w = hthumb.map(|r| r.width()).unwrap_or(20.0);
                let span = (lay.htrack.width() - thumb_w).max(1.0);
                let f = ((p.x - grab - lay.htrack.left()) / span).clamp(0.0, 1.0);
                view.h_offset = f * max_h;
            }
            Drag::Select => {
                if p.y < body.top() {
                    view.top = view.top.saturating_sub(1);
                    ui.ctx().request_repaint();
                } else if p.y >= body.bottom() {
                    view.top = (view.top + 1).min(max_top);
                    ui.ctx().request_repaint();
                }
                let k = ((p.y.clamp(body.top(), body.bottom() - 1.0) - body.top()) / row_h) as usize;
                let idx = (view.top + k).min(len.saturating_sub(1));
                if len > 0 && idx != view.cursor {
                    view.click_row(idx, mods.command, true);
                }
            }
            Drag::MoveCol { .. } => {
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                view.ui.move_target = lay.header.x_range().contains(p.x).then(|| insertion_index(&lay, p.x));
            }
            Drag::None => {}
        }
    }
    if resp.drag_stopped() {
        if let Drag::MoveCol { col } = view.ui.drag {
            let p = resp.interact_pointer_pos().or(hover);
            let over_group = p.is_some_and(|p| view.ui.group_panel_rect.is_some_and(|r| r.contains(p)));
            if over_group {
                if !view.group_by.contains(&col) {
                    view.group_by.push(col);
                    view.mark_dirty();
                }
            } else if let (Some(target), Some(p)) = (view.ui.move_target, p)
                && lay.header.expand(row_h).contains(p)
            {
                move_column(view, col, target, &lay);
            }
        }
        view.ui.drag = Drag::None;
        view.ui.dragging_col = None;
        view.ui.move_target = None;
    }

    if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        if lay.header.contains(p) {
            if near_edge(p).is_some() {
                // Edge clicks belong to resizing / best fit (double-click).
            } else if let Some(Hit::Col(i)) = lay.hit_x(p.x) {
                let (c, cx, w, _) = lay.cols[i];
                if p.x >= cx + w - 20.0 {
                    open_filter_popup(view, c, pos2(cx + w - 20.0, lay.header.bottom()), false);
                } else {
                    sort_click(view, c, mods.shift, mods.command);
                }
            }
        } else if lay.vtrack.contains(p) {
            if let Some(th) = vthumb {
                if p.y < th.top() {
                    view.top = view.top.saturating_sub(visible_rows);
                } else if p.y > th.bottom() {
                    view.top = (view.top + visible_rows).min(max_top);
                }
            }
        } else if lay.htrack.contains(p) {
            if let Some(th) = hthumb {
                if p.x < th.left() {
                    view.h_offset = (view.h_offset - scroll_view_w).max(0.0);
                } else if p.x > th.right() {
                    view.h_offset = (view.h_offset + scroll_view_w).min(max_h);
                }
            }
        } else if let (Some(k), Some(hit)) = (lay.row_at(p.y, row_h), lay.hit_x(p.x)) {
            let idx = view.top + k;
            if idx < len {
                match view.display[idx] {
                    Disp::Group(g) => {
                        view.cursor = idx;
                        view.toggle_group(g as usize);
                    }
                    Disp::Row(r) => {
                        if hit == Hit::Tag {
                            let on = !view.tags[r as usize];
                            if view.selected[r as usize] && view.sel_count > 1 {
                                view.toggle_tags();
                            } else {
                                view.set_tag(r as usize, on);
                                if view.tagged_only {
                                    view.mark_dirty();
                                }
                            }
                            view.cursor = idx;
                        } else {
                            view.click_row(idx, mods.command, mods.shift);
                            if let Hit::Col(i) = hit {
                                view.cursor_col = i;
                            }
                        }
                    }
                }
            }
        }
    }
    if resp.double_clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        if let Some(i) = near_edge(p) {
            let col = lay.cols[i].0;
            autofit(ui, view, Some(col), cfg);
        } else if body.contains(p)
            && let Some(k) = lay.row_at(p.y, row_h)
            && matches!(view.display.get(view.top + k), Some(Disp::Row(_)))
            && !matches!(lay.hit_x(p.x), Some(Hit::Tag))
        {
            actions.push(GridAction::ShowDetails);
        }
    }
    if resp.secondary_clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        view.ui.ctx_target = None;
        if lay.header.contains(p) {
            if let Some(Hit::Col(i)) = lay.hit_x(p.x) {
                view.ui.ctx_target = Some(CtxTarget::Header(lay.cols[i].0));
            }
        } else if let Some(k) = lay.row_at(p.y, row_h) {
            let idx = view.top + k;
            if let Some(Disp::Row(r)) = view.display.get(idx) {
                if !view.selected[*r as usize] {
                    view.click_row(idx, false, false);
                }
                view.cursor = idx;
                let col = match lay.hit_x(p.x) {
                    Some(Hit::Col(i)) => {
                        view.cursor_col = i;
                        Some(lay.cols[i].0)
                    }
                    _ => None,
                };
                view.ui.ctx_target = Some(CtxTarget::Cell { col });
            }
        }
    }

    // ---------------------------------------------------------- paint
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, body_bg);
    let first = view.top;
    let last = (first + visible_rows + 1).min(len);
    let vis_data_cols: Vec<usize> = lay.cols.iter().map(|c| c.0).collect();
    let plaso_src = match (cfg.plaso_colors, t.kind) {
        (true, TableKind::Plaso { source_col }) => Some(source_col),
        _ => None,
    };
    let search = view.compiled_search.clone();
    for (k, idx) in (first..last).enumerate() {
        let y = body.top() + k as f32 * row_h;
        let row_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
        match view.display[idx] {
            Disp::Group(g) => {
                let gi = &view.groups[g as usize];
                painter.rect_filled(row_rect, 0.0, header_bg);
                let arrow = if gi.collapsed { "⏵" } else { "⏷" };
                let label = format!("{arrow}  {}: {}  ({} items)", t.headers[gi.col], sanitize(&gi.value), gi.count);
                let indent = 6.0 + gi.level as f32 * 18.0;
                painter.with_clip_rect(row_rect).text(
                    pos2(inner.left() + indent, y + row_h / 2.0),
                    Align2::LEFT_CENTER,
                    label,
                    cfg.font.clone(),
                    text_color,
                );
                if idx == view.cursor {
                    painter.rect_stroke(row_rect.shrink(0.5), 0.0, Stroke::new(1.0, accent), StrokeKind::Inside);
                }
            }
            Disp::Row(r) => {
                let r = r as usize;
                let selected = view.selected[r];
                let mut bg = if k % 2 == 1 { stripe } else { body_bg };
                let mut fg = text_color;
                if let Some(sc) = plaso_src
                    && let Some(c) = format::plaso_color(t.cell(r, sc), cfg.dark)
                {
                    bg = c;
                }
                let mut cell_rules = vec![];
                for rule in &view.compiled_rules {
                    match rule.scope {
                        RuleScope::Row => {
                            if rule.matches_row(&t, r, &vis_data_cols) {
                                bg = rule.bg;
                                if let Some(f) = rule.fg {
                                    fg = f;
                                }
                                break;
                            }
                        }
                        RuleScope::Cell => cell_rules.push(rule),
                    }
                }
                if selected {
                    bg = sel_bg;
                    fg = sel_fg;
                }
                painter.rect_filled(row_rect, 0.0, bg);
                // Scrollable cells first, frozen ones over them.
                for pass_frozen in [false, true] {
                    let clip = lay.clip_for(pass_frozen);
                    let p = painter.with_clip_rect(clip.intersect(row_rect));
                    for (vi, &(c, cx, w, frozen)) in lay.cols.iter().enumerate() {
                        if frozen != pass_frozen || cx + w < clip.left() || cx > clip.right() {
                            continue;
                        }
                        let cell = Rect::from_min_size(pos2(cx, y), vec2(w, row_h));
                        if pass_frozen {
                            p.rect_filled(cell, 0.0, bg);
                        }
                        let mut cfg_fg = fg;
                        if !selected {
                            for rule in &cell_rules {
                                if rule.col.is_none_or(|rc| rc == c) && rule.matches_cell(&t, r, c) {
                                    p.rect_filled(cell, 0.0, rule.bg);
                                    if let Some(f) = rule.fg {
                                        cfg_fg = f;
                                    }
                                    break;
                                }
                            }
                        }
                        let text = display_text(&t, r, c, cfg.time);
                        let ranges = search.as_ref().map(|s| s.highlight_ranges(c, &text)).unwrap_or_default();
                        let job = cell_job(&text, &ranges, &cfg.font, cfg_fg, hl_bg, hl_fg, w - 8.0);
                        let galley = p.layout_job(job);
                        let gy = y + (row_h - galley.size().y) / 2.0;
                        p.galley(pos2(cx + 4.0, gy), galley, cfg_fg);
                        if idx == view.cursor && vi == view.cursor_col {
                            p.rect_stroke(cell.shrink(1.0), 0.0, Stroke::new(1.5, accent), StrokeKind::Inside);
                        }
                    }
                }
                // Line + tag cells (always pinned)
                let lr = Rect::from_min_size(pos2(inner.left(), y), vec2(line_w, row_h));
                painter.rect_filled(lr, 0.0, header_bg);
                painter.text(pos2(lr.right() - 6.0, y + row_h / 2.0), Align2::RIGHT_CENTER, r + 1, cfg.font.clone(), weak);
                let tr = Rect::from_min_size(pos2(inner.left() + line_w, y), vec2(TAG_W, row_h));
                painter.rect_filled(tr, 0.0, if selected { sel_bg } else { bg });
                draw_checkbox(&painter, tr.center(), view.tags[r], accent, weak);
                if idx == view.cursor {
                    painter.rect_stroke(row_rect.shrink(0.5), 0.0, Stroke::new(1.0, accent.gamma_multiply(0.6)), StrokeKind::Inside);
                }
            }
        }
        painter.line_segment([pos2(inner.left(), y + row_h), pos2(inner.right(), y + row_h)], Stroke::new(0.5, line_color.gamma_multiply(0.5)));
    }
    // Vertical grid lines
    for pass_frozen in [false, true] {
        let p = painter.with_clip_rect(lay.clip_for(pass_frozen));
        for &(_, cx, w, frozen) in &lay.cols {
            if frozen == pass_frozen {
                p.line_segment([pos2(cx + w, inner.top()), pos2(cx + w, body.bottom())], Stroke::new(0.5, line_color));
            }
        }
    }
    painter.line_segment(
        [pos2(inner.left() + fixed_w, inner.top()), pos2(inner.left() + fixed_w, body.bottom())],
        Stroke::new(if frozen_n > 0 { 2.0 } else { 1.0 }, line_color),
    );
    if len == 0 {
        painter.text(body.center_top() + vec2(0.0, 30.0), Align2::CENTER_CENTER, "No rows match the current filter", cfg.font.clone(), weak);
    }

    // Header
    painter.rect_filled(lay.header, 0.0, header_bg);
    for pass_frozen in [false, true] {
        let clip = lay.clip_for(pass_frozen).intersect(lay.header);
        let p = painter.with_clip_rect(clip);
        for &(c, cx, w, frozen) in &lay.cols {
            if frozen != pass_frozen || cx + w < clip.left() || cx > clip.right() {
                continue;
            }
            let cell = Rect::from_min_size(pos2(cx, inner.top()), vec2(w, header_h));
            p.rect_filled(cell, 0.0, header_bg);
            if hover.is_some_and(|h| cell.contains(h)) {
                p.rect_filled(cell, 0.0, visuals.widgets.hovered.weak_bg_fill);
            }
            let mut label = t.headers[c].clone();
            if let Some(pos) = view.sort.iter().position(|s| s.0 == c) {
                let arrow = if view.sort[pos].1 { "▼" } else { "▲" };
                label = if view.sort.len() > 1 { format!("{label} {arrow}{}", pos + 1) } else { format!("{label} {arrow}") };
            }
            let job = cell_job(&label, &[], &cfg.font, text_color, hl_bg, hl_fg, w - 26.0);
            let g = p.layout_job(job);
            p.galley(pos2(cx + 4.0, inner.top() + (header_h - g.size().y) / 2.0), g, text_color);
            let active = view.col_filters.get(&c).is_some_and(ColumnFilter::is_active);
            let funnel_color = if active { accent } else { weak.gamma_multiply(0.7) };
            draw_funnel(&p, pos2(cx + w - 11.0, inner.top() + header_h / 2.0), funnel_color, active);
            p.line_segment([pos2(cx + w, inner.top()), pos2(cx + w, inner.top() + header_h)], Stroke::new(1.0, line_color));
        }
    }
    let lh = Rect::from_min_size(inner.min, vec2(line_w, header_h));
    painter.text(lh.center(), Align2::CENTER_CENTER, "Line", cfg.font.clone(), text_color);
    let th = Rect::from_min_size(pos2(inner.left() + line_w, inner.top()), vec2(TAG_W, header_h));
    painter.text(th.center(), Align2::CENTER_CENTER, "Tag", FontId::proportional(cfg.font.size * 0.8), text_color);
    painter.line_segment([lay.header.left_bottom(), lay.header.right_bottom()], Stroke::new(1.0, line_color));

    if let (Drag::MoveCol { col }, Some(target)) = (view.ui.drag, view.ui.move_target) {
        let x = insertion_x(&lay, target);
        painter.line_segment([pos2(x, inner.top()), pos2(x, body.bottom())], Stroke::new(2.0, accent));
        if let Some(p) = hover {
            let r = Rect::from_min_size(p + vec2(8.0, 4.0), vec2(lay.cols.iter().find(|c| c.0 == col).map_or(100.0, |c| c.2.min(220.0)), header_h));
            painter.rect_filled(r, 3.0, header_bg.gamma_multiply(0.9));
            painter.text(r.left_center() + vec2(4.0, 0.0), Align2::LEFT_CENTER, &t.headers[col], cfg.font.clone(), text_color);
        }
    }

    // Auto-filter row
    if cfg.show_filter_row {
        painter.rect_filled(lay.filter_row, 0.0, header_bg.gamma_multiply(0.85));
        let tr = Rect::from_min_size(pos2(inner.left() + line_w, lay.filter_row.top()), vec2(TAG_W, filter_h));
        let tagged_btn = ui.interact(tr, ui.scope_id().with("tag_only"), Sense::click()).on_hover_text("Show only tagged rows (Ctrl+Shift+T)");
        if tagged_btn.clicked() {
            view.tagged_only = !view.tagged_only;
            view.mark_dirty();
        }
        draw_checkbox(&painter, tr.center(), view.tagged_only, accent, weak);
        for pass_frozen in [false, true] {
            let clip = lay.clip_for(pass_frozen).intersect(lay.filter_row);
            for &(c, cx, w, frozen) in &lay.cols {
                if frozen != pass_frozen || cx + w < clip.left() || cx > clip.right() {
                    continue;
                }
                let cell = Rect::from_min_size(pos2(cx, lay.filter_row.top()), vec2(w, filter_h)).shrink2(vec2(2.0, 2.0));
                let mut child = ui.new_child(UiBuilder::new().max_rect(cell).id_salt(("autofilter", c)));
                child.set_clip_rect(clip);
                let mut text = view.col_filters.get(&c).map(|f| f.text.clone()).unwrap_or_default();
                let r = child.add(
                    TextEdit::singleline(&mut text)
                        .font(cfg.font.clone())
                        .desired_width(cell.width())
                        .margin(vec2(3.0, 1.0))
                        .hint_text("…"),
                );
                if r.changed() {
                    let e = view.col_filters.entry(c).or_default();
                    e.text = text;
                    if !e.is_active() {
                        view.col_filters.remove(&c);
                    }
                    view.ui.filter_edit_at = Some(ui.input(|i| i.time));
                }
                r.on_hover_text(
                    "Auto-filter: text = contains · =x equals · !x not contains · ^x begins · x$ ends · >x <x >=x <=x · a*b wildcard · /regex/",
                );
            }
        }
        painter.line_segment([lay.filter_row.left_bottom(), lay.filter_row.right_bottom()], Stroke::new(1.0, line_color));
    }

    // Scrollbars
    let sb_bg = visuals.extreme_bg_color.gamma_multiply(0.9);
    let thumb_c = visuals.widgets.inactive.bg_fill;
    painter.rect_filled(lay.vtrack, 0.0, sb_bg);
    painter.rect_filled(Rect::from_min_max(pos2(inner.left(), inner.bottom()), rect.max), 0.0, sb_bg);
    if let Some(th) = vthumb {
        painter.rect_filled(th.shrink2(vec2(2.0, 0.0)), 4.0, thumb_c);
    }
    if let Some(th) = hthumb {
        painter.rect_filled(th.shrink2(vec2(0.0, 2.0)), 4.0, thumb_c);
    }

    // ---------------------------------------------------------- context menu
    resp.context_menu(|ui| {
        context_menu(ui, view, &mut actions, cfg);
    });
    actions
}

fn insertion_index(lay: &Layout, x: f32) -> usize {
    for (i, &(_, cx, w, _)) in lay.cols.iter().enumerate() {
        if x < cx + w / 2.0 {
            return i;
        }
    }
    lay.cols.len()
}

fn insertion_x(lay: &Layout, idx: usize) -> f32 {
    match lay.cols.get(idx) {
        Some(c) => c.1,
        None => lay.cols.last().map_or(lay.inner.left(), |c| c.1 + c.2),
    }
}

fn move_column(view: &mut TabView, col: usize, target_vis: usize, lay: &Layout) {
    let Some(from) = view.columns.iter().position(|c| c.col == col) else { return };
    let target_col = lay.cols.get(target_vis).map(|c| c.0);
    if target_col == Some(col) {
        return;
    }
    let cs = view.columns.remove(from);
    let to = match target_col {
        Some(tc) => view.columns.iter().position(|c| c.col == tc).unwrap_or(view.columns.len()),
        None => view.columns.len(),
    };
    view.columns.insert(to, cs);
}

fn vthumb_rect(lay: &Layout, top: usize, len: usize) -> Option<Rect> {
    if lay.max_top == 0 {
        return None;
    }
    let h = lay.vtrack.height();
    let th = (h * lay.visible_rows as f32 / len as f32).max(24.0).min(h);
    let y = lay.vtrack.top() + (h - th) * (top as f64 / lay.max_top as f64) as f32;
    Some(Rect::from_min_size(pos2(lay.vtrack.left(), y), vec2(lay.vtrack.width(), th)))
}

fn hthumb_rect(lay: &Layout, off: f32) -> Option<Rect> {
    let max_h = lay.scroll_total - lay.scroll_view_w;
    if max_h <= 0.0 {
        return None;
    }
    let w = lay.htrack.width();
    let tw = (w * lay.scroll_view_w / lay.scroll_total).max(24.0).min(w);
    let x = lay.htrack.left() + (w - tw) * off / max_h;
    Some(Rect::from_min_size(pos2(x, lay.htrack.top()), vec2(tw, lay.htrack.height())))
}

fn draw_checkbox(p: &egui::Painter, c: Pos2, on: bool, accent: Color32, weak: Color32) {
    let r = Rect::from_center_size(c, vec2(12.0, 12.0));
    if on {
        p.rect_filled(r, 2.0, accent);
        p.line_segment([r.left_center() + vec2(2.5, 0.0), r.center_bottom() + vec2(-1.0, -2.5)], Stroke::new(2.0, Color32::WHITE));
        p.line_segment([r.center_bottom() + vec2(-1.0, -2.5), r.right_top() + vec2(-2.0, 2.5)], Stroke::new(2.0, Color32::WHITE));
    } else {
        p.rect_stroke(r, 2.0, Stroke::new(1.0, weak), StrokeKind::Inside);
    }
}

fn draw_funnel(p: &egui::Painter, c: Pos2, color: Color32, filled: bool) {
    let pts = vec![c + vec2(-5.0, -4.0), c + vec2(5.0, -4.0), c + vec2(1.0, 1.0), c + vec2(1.0, 5.0), c + vec2(-1.0, 5.0), c + vec2(-1.0, 1.0)];
    let shape = if filled {
        egui::Shape::convex_polygon(pts, color, Stroke::NONE)
    } else {
        egui::Shape::closed_line(pts, Stroke::new(1.0, color))
    };
    p.add(shape);
}

fn context_menu(ui: &mut Ui, view: &mut TabView, actions: &mut Vec<GridAction>, cfg: &GridConfig) {
    let Some(target) = view.ui.ctx_target else {
        ui.close();
        return;
    };
    let t = view.table.clone();
    match target {
        CtxTarget::Header(c) => {
            let name = t.headers[c].clone();
            ui.label(egui::RichText::new(&name).strong());
            ui.separator();
            if ui.button("▲ Sort ascending").clicked() {
                view.sort = vec![(c, false)];
                view.mark_dirty();
                ui.close();
            }
            if ui.button("▼ Sort descending").clicked() {
                view.sort = vec![(c, true)];
                view.mark_dirty();
                ui.close();
            }
            if !view.sort.is_empty() && ui.button("Clear sorting").clicked() {
                view.sort.clear();
                view.mark_dirty();
                ui.close();
            }
            ui.separator();
            if view.group_by.contains(&c) {
                if ui.button("Ungroup").clicked() {
                    view.group_by.retain(|&g| g != c);
                    view.mark_dirty();
                    ui.close();
                }
            } else if ui.button("Group by this column").clicked() {
                view.group_by.push(c);
                view.mark_dirty();
                ui.close();
            }
            if !view.group_by.is_empty() && ui.button("Clear grouping").clicked() {
                view.group_by.clear();
                view.mark_dirty();
                ui.close();
            }
            ui.separator();
            if ui.button("Hide this column").clicked() {
                if let Some(cs) = view.column_state(c) {
                    cs.visible = false;
                }
                view.mark_dirty();
                ui.close();
            }
            if ui.button("Column chooser…").clicked() {
                actions.push(GridAction::ColumnChooser);
                ui.close();
            }
            if ui.button("Best fit").clicked() {
                autofit(ui, view, Some(c), cfg);
                ui.close();
            }
            if ui.button("Best fit (all columns)").clicked() {
                autofit(ui, view, None, cfg);
                ui.close();
            }
            let vis = view.visible_cols();
            if let Some(pos) = vis.iter().position(|&x| x == c) {
                if ui.button("Set as first scrollable column").on_hover_text("Pin every column to the left of this one").clicked() {
                    view.frozen = pos;
                    ui.close();
                }
                if ui.button("Freeze up to this column").clicked() {
                    view.frozen = pos + 1;
                    ui.close();
                }
            }
            if view.frozen > 0 && ui.button("Unfreeze columns").clicked() {
                view.frozen = 0;
                ui.close();
            }
            ui.separator();
            if ui.button("Filter values…").clicked() {
                open_filter_popup(view, c, ui.ctx().input(|i| i.pointer.hover_pos()).unwrap_or_default(), false);
                ui.close();
            }
            if view.col_filters.get(&c).is_some_and(ColumnFilter::is_active) && ui.button("Clear filter on this column").clicked() {
                view.col_filters.remove(&c);
                view.mark_dirty();
                ui.close();
            }
            if ui.button("Filter editor…").clicked() {
                actions.push(GridAction::FilterEditor);
                ui.close();
            }
            ui.separator();
            ui.menu_button("Conditional formatting", |ui| {
                ui.menu_button("Highlight cell rules", |ui| {
                    for (label, op) in [
                        ("Text that contains…", Op::Contains),
                        ("Equal to…", Op::Equals),
                        ("Greater than…", Op::Greater),
                        ("Less than…", Op::Less),
                        ("Between…", Op::Between),
                        ("Matches regex…", Op::Regex),
                    ] {
                        if ui.button(label).clicked() {
                            let mut r = CondRule::new(Some(name.clone()), op, "", Preset::LightRed);
                            r.scope = RuleScope::Cell;
                            actions.push(GridAction::RuleDialog(r));
                            ui.close();
                        }
                    }
                });
                if ui.button("Manage rules…").clicked() {
                    actions.push(GridAction::ManageRules);
                    ui.close();
                }
            });
        }
        CtxTarget::Cell { col } => {
            let row = view.cursor_row();
            if let (Some(c), Some(r)) = (col, row) {
                let name = t.headers[c].clone();
                let val = t.cell(r, c).to_string();
                let short: String = sanitize(&val).chars().take(40).collect();
                if ui.button("Copy cell value").clicked() {
                    ui.ctx().copy_text(val.clone());
                    ui.close();
                }
                if ui.button("Copy selected rows (with header)").clicked() {
                    ui.ctx().copy_text(view.copy_rows(true));
                    ui.close();
                }
                if ui.button("Copy selected rows (no header)").clicked() {
                    ui.ctx().copy_text(view.copy_rows(false));
                    ui.close();
                }
                ui.separator();
                if ui.button("Tag / untag selected rows").clicked() {
                    view.toggle_tags();
                    ui.close();
                }
                if ui.button("Show row details").clicked() {
                    actions.push(GridAction::ShowDetails);
                    ui.close();
                }
                ui.separator();
                ui.menu_button("Filter", |ui| {
                    if ui.button(format!("Show only [{name}] = '{short}'")).clicked() {
                        view.filter_tree.push_and(Condition::new(&name, Op::Equals, &val), false);
                        view.mark_dirty();
                        ui.close();
                    }
                    if ui.button(format!("Exclude [{name}] = '{short}'")).clicked() {
                        view.filter_tree.push_and(Condition::new(&name, Op::NotEquals, &val), false);
                        view.mark_dirty();
                        ui.close();
                    }
                    if t.col_types[c] != ColType::Text {
                        if ui.button(format!("[{name}] ≥ '{short}'")).clicked() {
                            view.filter_tree.push_and(Condition::new(&name, Op::GreaterOrEqual, &val), false);
                            view.mark_dirty();
                            ui.close();
                        }
                        if ui.button(format!("[{name}] ≤ '{short}'")).clicked() {
                            view.filter_tree.push_and(Condition::new(&name, Op::LessOrEqual, &val), false);
                            view.mark_dirty();
                            ui.close();
                        }
                    }
                    if ui.button("Search for this value").clicked() {
                        actions.push(GridAction::SetSearch(format!("\"{}\"", val.replace('"', ""))));
                        ui.close();
                    }
                });
                ui.menu_button("Conditional formatting", |ui| {
                    ui.menu_button(format!("Highlight rows where [{name}] = '{short}'"), |ui| {
                        for p in Preset::ALL {
                            let (bg, fg) = p.colors();
                            let txt = egui::RichText::new(p.label()).background_color(bg).color(fg);
                            if ui.button(txt).clicked() {
                                view.rules.push(CondRule::new(Some(name.clone()), Op::Equals, &val, p));
                                view.rules_dirty = true;
                                ui.close();
                            }
                        }
                    });
                    ui.menu_button("Highlight cell rules", |ui| {
                        for (label, op) in [
                            ("Text that contains…", Op::Contains),
                            ("Equal to…", Op::Equals),
                            ("Greater than…", Op::Greater),
                            ("Less than…", Op::Less),
                            ("Matches regex…", Op::Regex),
                        ] {
                            if ui.button(label).clicked() {
                                let mut r = CondRule::new(Some(name.clone()), op, &val, Preset::LightRed);
                                r.scope = RuleScope::Cell;
                                actions.push(GridAction::RuleDialog(r));
                                ui.close();
                            }
                        }
                    });
                    if ui.button("Manage rules…").clicked() {
                        actions.push(GridAction::ManageRules);
                        ui.close();
                    }
                });
            } else {
                if ui.button("Tag / untag selected rows").clicked() {
                    view.toggle_tags();
                    ui.close();
                }
                if ui.button("Copy selected rows").clicked() {
                    ui.ctx().copy_text(view.copy_rows(true));
                    ui.close();
                }
            }
        }
    }
}
