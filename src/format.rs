//! Conditional formatting rules and Plaso colour coding.

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::data::Table;
use crate::filter::{Condition, Op, Pred, compile_condition};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleScope {
    Row,
    Cell,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CondRule {
    pub enabled: bool,
    /// `None` = any visible column.
    pub column: Option<String>,
    pub op: Op,
    pub value: String,
    #[serde(default)]
    pub value2: String,
    pub case_sensitive: bool,
    pub scope: RuleScope,
    pub bg: [u8; 4],
    pub fg: Option<[u8; 4]>,
}

impl CondRule {
    pub fn new(column: Option<String>, op: Op, value: &str, preset: Preset) -> Self {
        let (bg, fg) = preset.colors();
        Self {
            enabled: true,
            column,
            op,
            value: value.to_string(),
            value2: String::new(),
            case_sensitive: false,
            scope: RuleScope::Row,
            bg: bg.to_array(),
            fg: Some(fg.to_array()),
        }
    }

    pub fn describe(&self) -> String {
        let col = self.column.clone().unwrap_or_else(|| "Any column".into());
        let mut c = Condition::new(&col, self.op, &self.value);
        c.value2 = self.value2.clone();
        c.describe()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    LightRed,
    Yellow,
    Green,
    LightBlue,
    Orange,
    Purple,
}

impl Preset {
    pub const ALL: [Preset; 6] = [Preset::LightRed, Preset::Yellow, Preset::Green, Preset::LightBlue, Preset::Orange, Preset::Purple];

    pub fn label(self) -> &'static str {
        match self {
            Preset::LightRed => "Light red fill with dark red text",
            Preset::Yellow => "Yellow fill with dark yellow text",
            Preset::Green => "Green fill with dark green text",
            Preset::LightBlue => "Light blue fill with dark blue text",
            Preset::Orange => "Orange fill with dark text",
            Preset::Purple => "Purple fill with dark text",
        }
    }

    pub fn colors(self) -> (Color32, Color32) {
        match self {
            Preset::LightRed => (Color32::from_rgb(255, 199, 206), Color32::from_rgb(156, 0, 6)),
            Preset::Yellow => (Color32::from_rgb(255, 235, 156), Color32::from_rgb(156, 87, 0)),
            Preset::Green => (Color32::from_rgb(198, 239, 206), Color32::from_rgb(0, 97, 0)),
            Preset::LightBlue => (Color32::from_rgb(189, 215, 238), Color32::from_rgb(31, 56, 100)),
            Preset::Orange => (Color32::from_rgb(252, 213, 180), Color32::from_rgb(90, 40, 0)),
            Preset::Purple => (Color32::from_rgb(222, 205, 240), Color32::from_rgb(60, 20, 90)),
        }
    }
}

pub struct CompiledRule {
    pub scope: RuleScope,
    /// `None` column = test every visible column.
    pub col: Option<usize>,
    /// For "any column" rules we compile one predicate per column lazily: keep
    /// the template condition and per-column predicates.
    preds: Vec<(usize, Pred)>,
    pub bg: Color32,
    pub fg: Option<Color32>,
}

pub fn compile_rules(t: &Table, rules: &[CondRule]) -> (Vec<CompiledRule>, Vec<String>) {
    let mut out = vec![];
    let mut errors = vec![];
    for r in rules.iter().filter(|r| r.enabled) {
        let cols: Vec<usize> = match &r.column {
            Some(name) => match t.find_column(name) {
                Some(c) => vec![c],
                None => {
                    errors.push(format!("Formatting rule: unknown column '{name}'"));
                    continue;
                }
            },
            None => (0..t.ncols()).collect(),
        };
        let mut preds = vec![];
        let mut failed = false;
        for &c in &cols {
            let cond = Condition {
                column: t.headers[c].clone(),
                op: r.op,
                value: r.value.clone(),
                value2: r.value2.clone(),
                case_sensitive: r.case_sensitive,
            };
            match compile_condition(t, &cond) {
                Ok(p) => preds.push((c, p)),
                Err(e) => {
                    errors.push(format!("Formatting rule: {e}"));
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        let rgba = |a: [u8; 4]| Color32::from_rgba_unmultiplied(a[0], a[1], a[2], a[3]);
        out.push(CompiledRule {
            scope: r.scope,
            col: if r.column.is_some() { Some(cols[0]) } else { None },
            preds,
            bg: rgba(r.bg),
            fg: r.fg.map(rgba),
        });
    }
    (out, errors)
}

impl CompiledRule {
    /// Does the rule match the row (any of its target columns among `visible`)?
    pub fn matches_row(&self, t: &Table, row: usize, visible: &[usize]) -> bool {
        self.preds.iter().any(|(c, p)| (self.col.is_some() || visible.contains(c)) && p.eval(t, row))
    }

    pub fn matches_cell(&self, t: &Table, row: usize, col: usize) -> bool {
        self.preds.iter().any(|(c, p)| *c == col && p.eval(t, row))
    }
}

// ---------------------------------------------------------------------------
// Plaso legend (mirrors Timeline Explorer's "Plaso colour legend").

pub const PLASO_LEGEND: &[(&str, &str, [u8; 3])] = &[
    ("FILE", "File system (MFT, $J, file stat)", [214, 234, 248]),
    ("REG", "Registry", [252, 228, 214]),
    ("EVT", "Windows event logs", [226, 239, 218]),
    ("EVTX", "Windows event logs (EVTX)", [226, 239, 218]),
    ("LOG", "Text / system logs", [237, 237, 237]),
    ("WEBHIST", "Web history, cookies, cache", [255, 242, 204]),
    ("LNK", "Shortcut files / jump lists", [237, 226, 246]),
    ("RECBIN", "Recycle bin", [248, 203, 173]),
    ("PE", "Executable (PE) metadata", [255, 199, 206]),
    ("META", "Document metadata", [221, 235, 247]),
    ("OLECF", "OLE compound files", [217, 225, 242]),
    ("PLIST", "Property lists", [226, 239, 218]),
    ("SQLITE", "SQLite databases", [255, 230, 153]),
    ("AV", "Antivirus logs", [244, 176, 132]),
    ("PREFETCH", "Prefetch", [198, 224, 180]),
    ("JOB", "Scheduled tasks", [180, 198, 231]),
    ("EMAIL", "E-mail", [255, 217, 102]),
];

pub fn plaso_color(source: &str, dark: bool) -> Option<Color32> {
    let s = source.trim();
    let (_, _, rgb) = PLASO_LEGEND.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(s))?;
    Some(if dark {
        // Darkened variant so text stays readable on dark skins.
        Color32::from_rgb(rgb[0] / 4 + 10, rgb[1] / 4 + 10, rgb[2] / 4 + 10)
    } else {
        Color32::from_rgb(rgb[0], rgb[1], rgb[2])
    })
}
