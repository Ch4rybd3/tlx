//! Filter model: column filters (value lists + auto-filter row), the
//! filter-editor expression tree and the global "power search".

use std::collections::HashSet;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::data::{ColType, Table, cmp_ci};
use crate::timefmt;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    StartsWith,
    EndsWith,
    Like,
    Regex,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    Between,
    IsEmpty,
    IsNotEmpty,
}

impl Op {
    pub const ALL: [Op; 15] = [
        Op::Contains,
        Op::NotContains,
        Op::Equals,
        Op::NotEquals,
        Op::StartsWith,
        Op::EndsWith,
        Op::Like,
        Op::Regex,
        Op::Greater,
        Op::GreaterOrEqual,
        Op::Less,
        Op::LessOrEqual,
        Op::Between,
        Op::IsEmpty,
        Op::IsNotEmpty,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Op::Contains => "contains",
            Op::NotContains => "does not contain",
            Op::Equals => "equals",
            Op::NotEquals => "does not equal",
            Op::StartsWith => "begins with",
            Op::EndsWith => "ends with",
            Op::Like => "is like (* ?)",
            Op::Regex => "matches regex",
            Op::Greater => "is greater than",
            Op::GreaterOrEqual => "is greater or equal to",
            Op::Less => "is less than",
            Op::LessOrEqual => "is less or equal to",
            Op::Between => "is between",
            Op::IsEmpty => "is blank",
            Op::IsNotEmpty => "is not blank",
        }
    }

    pub fn arity(self) -> usize {
        match self {
            Op::IsEmpty | Op::IsNotEmpty => 0,
            Op::Between => 2,
            _ => 1,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Condition {
    pub column: String,
    pub op: Op,
    pub value: String,
    #[serde(default)]
    pub value2: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

impl Condition {
    pub fn new(column: &str, op: Op, value: &str) -> Self {
        Self { column: column.to_string(), op, value: value.to_string(), value2: String::new(), case_sensitive: false }
    }

    pub fn describe(&self) -> String {
        match self.op.arity() {
            0 => format!("[{}] {}", self.column, self.op.label()),
            2 => format!("[{}] {} '{}' and '{}'", self.column, self.op.label(), self.value, self.value2),
            _ => format!("[{}] {} '{}'", self.column, self.op.label(), self.value),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logic {
    And,
    Or,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FilterNode {
    Group { logic: Logic, negate: bool, children: Vec<FilterNode> },
    Cond { cond: Condition, negate: bool },
}

impl Default for FilterNode {
    fn default() -> Self {
        FilterNode::Group { logic: Logic::And, negate: false, children: vec![] }
    }
}

impl FilterNode {
    pub fn is_empty(&self) -> bool {
        match self {
            FilterNode::Group { children, .. } => children.iter().all(FilterNode::is_empty),
            FilterNode::Cond { .. } => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            FilterNode::Cond { cond, negate } => {
                if *negate { format!("NOT {}", cond.describe()) } else { cond.describe() }
            }
            FilterNode::Group { logic, negate, children } => {
                let parts: Vec<String> = children.iter().filter(|c| !c.is_empty()).map(|c| c.describe()).collect();
                let sep = if *logic == Logic::And { " AND " } else { " OR " };
                let inner = parts.join(sep);
                match (negate, parts.len()) {
                    (true, _) => format!("NOT ({inner})"),
                    (false, 0 | 1) => inner,
                    _ => format!("({inner})"),
                }
            }
        }
    }

    /// Adds a condition to the root group (AND-ing it with existing criteria).
    pub fn push_and(&mut self, cond: Condition, negate: bool) {
        let node = FilterNode::Cond { cond, negate };
        match self {
            FilterNode::Group { logic: Logic::And, negate: false, children } => children.push(node),
            _ => {
                let old = std::mem::take(self);
                *self = FilterNode::Group { logic: Logic::And, negate: false, children: vec![old, node] };
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ValueFilter {
    /// `true`: keep only values in `set`; `false`: drop values in `set`.
    pub include: bool,
    pub set: HashSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ColumnFilter {
    pub values: Option<ValueFilter>,
    /// Auto-filter row text (quick syntax, see [`parse_quick`]).
    pub text: String,
}

impl ColumnFilter {
    pub fn is_active(&self) -> bool {
        self.values.is_some() || !self.text.trim().is_empty()
    }

    pub fn describe(&self, column: &str) -> Vec<String> {
        let mut out = vec![];
        if let Some(v) = &self.values {
            let mut items: Vec<&String> = v.set.iter().collect();
            items.sort();
            let shown: Vec<String> =
                items.iter().take(5).map(|s| if s.is_empty() { "(Blanks)".to_string() } else { format!("'{s}'") }).collect();
            let more = if items.len() > 5 { format!(", … +{}", items.len() - 5) } else { String::new() };
            let kw = if v.include { "In" } else { "Not In" };
            out.push(format!("[{column}] {kw} ({}{more})", shown.join(", ")));
        }
        if !self.text.trim().is_empty() {
            let (op, val) = parse_quick(&self.text);
            out.push(Condition::new(column, op, &val).describe());
        }
        out
    }
}

/// Auto-filter row quick syntax:
/// `text` contains · `=x` equals · `!x` does not contain · `<>x` not equal ·
/// `^x` begins with · `x$` ends with · `>x` `>=x` `<x` `<=x` comparisons ·
/// `/re/` regex · `a*b?` wildcards.
pub fn parse_quick(text: &str) -> (Op, String) {
    let t = text.trim();
    let rest = |n: usize| t[n..].trim().to_string();
    if t.len() >= 2 && t.starts_with('/') && t.ends_with('/') {
        return (Op::Regex, t[1..t.len() - 1].to_string());
    }
    if let Some(p) = [">=", "<=", "<>", "!="].iter().find(|p| t.starts_with(*p)) {
        let op = match *p {
            ">=" => Op::GreaterOrEqual,
            "<=" => Op::LessOrEqual,
            _ => Op::NotEquals,
        };
        return (op, rest(2));
    }
    match t.chars().next() {
        Some('>') => return (Op::Greater, rest(1)),
        Some('<') => return (Op::Less, rest(1)),
        Some('=') => return (Op::Equals, rest(1)),
        Some('!') => return (Op::NotContains, rest(1)),
        Some('^') => return (Op::StartsWith, rest(1)),
        _ => {}
    }
    if t.len() > 1 && t.ends_with('$') {
        return (Op::EndsWith, t[..t.len() - 1].to_string());
    }
    if t.contains(['*', '?']) {
        return (Op::Like, t.to_string());
    }
    (Op::Contains, t.to_string())
}

fn like_to_regex(p: &str) -> String {
    let mut out = String::from("^");
    for c in p.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(50 << 20)
        .build()
        .map_err(|e| format!("Invalid regex '{pattern}': {e}"))
}

// ---------------------------------------------------------------------------
// Compiled predicates

#[derive(Clone, Copy, Debug)]
pub enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Between,
}

impl Cmp {
    fn test<T: PartialOrd>(self, v: T, a: T, b: T) -> bool {
        match self {
            Cmp::Gt => v > a,
            Cmp::Ge => v >= a,
            Cmp::Lt => v < a,
            Cmp::Le => v <= a,
            Cmp::Between => v >= a && v <= b,
        }
    }
}

pub enum Pred {
    True,
    Text { col: usize, re: Regex },
    Num { col: usize, cmp: Cmp, a: f64, b: f64 },
    Time { col: usize, cmp: Cmp, a: i64, b: i64 },
    Str { col: usize, cmp: Cmp, a: String, b: String },
    Empty { col: usize },
    Values { col: usize, include: bool, set: HashSet<String> },
    Not(Box<Pred>),
    All(Vec<Pred>),
    Any(Vec<Pred>),
}

impl Pred {
    pub fn eval(&self, t: &Table, r: usize) -> bool {
        match self {
            Pred::True => true,
            Pred::Text { col, re } => re.is_match(t.cell(r, *col)),
            Pred::Num { col, cmp, a, b } => {
                let v = t.num_keys(*col)[r];
                v.is_finite() && cmp.test(v, *a, *b)
            }
            Pred::Time { col, cmp, a, b } => {
                let v = t.time_keys(*col)[r];
                v != i64::MIN && cmp.test(v, *a, *b)
            }
            Pred::Str { col, cmp, a, b } => {
                let v = t.cell(r, *col);
                let o1 = cmp_ci(v, a);
                let o2 = cmp_ci(v, b);
                use std::cmp::Ordering::*;
                match cmp {
                    Cmp::Gt => o1 == Greater,
                    Cmp::Ge => o1 != Less,
                    Cmp::Lt => o1 == Less,
                    Cmp::Le => o1 != Greater,
                    Cmp::Between => o1 != Less && o2 != Greater,
                }
            }
            Pred::Empty { col } => t.cell(r, *col).trim().is_empty(),
            Pred::Values { col, include, set } => set.contains(t.cell(r, *col)) == *include,
            Pred::Not(p) => !p.eval(t, r),
            Pred::All(ps) => ps.iter().all(|p| p.eval(t, r)),
            Pred::Any(ps) => ps.iter().any(|p| p.eval(t, r)),
        }
    }

    /// Warms the lazily computed key caches used by this predicate (so that
    /// the parallel scan does not contend on initialisation).
    pub fn warm(&self, t: &Table) {
        match self {
            Pred::Num { col, .. } => {
                t.num_keys(*col);
            }
            Pred::Time { col, .. } => {
                t.time_keys(*col);
            }
            Pred::Not(p) => p.warm(t),
            Pred::All(ps) | Pred::Any(ps) => ps.iter().for_each(|p| p.warm(t)),
            _ => {}
        }
    }
}

pub fn compile_condition(t: &Table, c: &Condition) -> Result<Pred, String> {
    let col = t.find_column(&c.column).ok_or_else(|| format!("Unknown column '{}'", c.column))?;
    let esc = regex::escape(&c.value);
    let text = |p: String| -> Result<Pred, String> { Ok(Pred::Text { col, re: build_regex(&p, c.case_sensitive)? }) };
    let cmp = match c.op {
        Op::Contains => return text(esc),
        Op::NotContains => return Ok(Pred::Not(Box::new(text(esc)?))),
        Op::Equals => return text(format!("^{esc}$")),
        Op::NotEquals => return Ok(Pred::Not(Box::new(text(format!("^{esc}$"))?))),
        Op::StartsWith => return text(format!("^{esc}")),
        Op::EndsWith => return text(format!("{esc}$")),
        Op::Like => return text(like_to_regex(&c.value)),
        Op::Regex => return text(c.value.clone()),
        Op::IsEmpty => return Ok(Pred::Empty { col }),
        Op::IsNotEmpty => return Ok(Pred::Not(Box::new(Pred::Empty { col }))),
        Op::Greater => Cmp::Gt,
        Op::GreaterOrEqual => Cmp::Ge,
        Op::Less => Cmp::Lt,
        Op::LessOrEqual => Cmp::Le,
        Op::Between => Cmp::Between,
    };
    let v2 = if c.op == Op::Between { c.value2.as_str() } else { c.value.as_str() };
    match t.col_types[col] {
        ColType::Number => {
            if let (Some(a), Some(b)) = (timefmt::parse_number(&c.value), timefmt::parse_number(v2)) {
                return Ok(Pred::Num { col, cmp, a, b });
            }
        }
        ColType::DateTime => {
            if let (Some(a), Some(b)) = (timefmt::parse_datetime(&c.value), timefmt::parse_datetime(v2)) {
                let mut b = timefmt::to_key(b);
                // "between 2024-01-01 and 2024-01-02" should include the whole last day.
                if c.op == Op::Between && v2.trim().len() <= 10 {
                    b += 86_400_000_000 - 1;
                }
                return Ok(Pred::Time { col, cmp, a: timefmt::to_key(a), b });
            }
        }
        ColType::Text => {}
    }
    Ok(Pred::Str { col, cmp, a: c.value.clone(), b: v2.to_string() })
}

pub fn compile_node(t: &Table, n: &FilterNode) -> Result<Pred, String> {
    match n {
        FilterNode::Cond { cond, negate } => {
            let p = compile_condition(t, cond)?;
            Ok(if *negate { Pred::Not(Box::new(p)) } else { p })
        }
        FilterNode::Group { logic, negate, children } => {
            let ps: Vec<Pred> = children.iter().filter(|c| !c.is_empty()).map(|c| compile_node(t, c)).collect::<Result<_, _>>()?;
            let p = if ps.is_empty() {
                Pred::True
            } else if *logic == Logic::And {
                Pred::All(ps)
            } else {
                Pred::Any(ps)
            };
            Ok(if *negate { Pred::Not(Box::new(p)) } else { p })
        }
    }
}

pub fn compile_column_filter(t: &Table, col: usize, f: &ColumnFilter) -> Result<Vec<Pred>, String> {
    let mut out = vec![];
    if let Some(v) = &f.values {
        out.push(Pred::Values { col, include: v.include, set: v.set.clone() });
    }
    if !f.text.trim().is_empty() {
        let (op, value) = parse_quick(&f.text);
        let cond = Condition { column: t.headers[col].clone(), op, value, value2: String::new(), case_sensitive: false };
        out.push(compile_condition(t, &cond)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Global search ("Find panel" / power filtering)

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchBehavior {
    Filter,
    Highlight,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchCondition {
    Contains,
    StartsWith,
    Equals,
    Like,
    Regex,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchMode {
    /// Any of the (non +/-) words must match.
    Any,
    /// All of the words must match (anywhere in the row).
    All,
    /// The whole text is a single phrase.
    Exact,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct SearchOptions {
    pub behavior: SearchBehavior,
    pub condition: SearchCondition,
    pub match_mode: MatchMode,
    pub case_sensitive: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            behavior: SearchBehavior::Filter,
            condition: SearchCondition::Contains,
            match_mode: MatchMode::Any,
            case_sensitive: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TermKind {
    Optional,
    Required,
    Excluded,
}

struct SearchTerm {
    col: Option<usize>,
    kind: TermKind,
    re: Regex,
}

pub struct CompiledSearch {
    terms: Vec<SearchTerm>,
    any: bool,
    /// Columns searched by unqualified terms (the visible ones).
    cols: Vec<usize>,
}

#[derive(Debug, PartialEq)]
struct RawTerm {
    col: Option<String>,
    kind: TermKind,
    text: String,
}

fn tokenize(q: &str) -> Vec<RawTerm> {
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    let read_part = |i: &mut usize| -> (String, bool) {
        if *i < chars.len() && chars[*i] == '"' {
            *i += 1;
            let mut s = String::new();
            while *i < chars.len() && chars[*i] != '"' {
                s.push(chars[*i]);
                *i += 1;
            }
            *i += 1;
            (s, true)
        } else {
            let mut s = String::new();
            while *i < chars.len() && !chars[*i].is_whitespace() && chars[*i] != ':' {
                s.push(chars[*i]);
                *i += 1;
            }
            (s, false)
        }
    };
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let kind = match chars[i] {
            '+' => {
                i += 1;
                TermKind::Required
            }
            '-' => {
                i += 1;
                TermKind::Excluded
            }
            _ => TermKind::Optional,
        };
        let (first, _) = read_part(&mut i);
        if i < chars.len() && chars[i] == ':' && !first.is_empty() {
            i += 1;
            let (second, quoted) = read_part(&mut i);
            // Unquoted remainder may contain further ':' (e.g. URLs, times).
            let mut second = second;
            if !quoted {
                while i < chars.len() && !chars[i].is_whitespace() {
                    second.push(chars[i]);
                    i += 1;
                }
            }
            out.push(RawTerm { col: Some(first), kind, text: second });
        } else {
            let mut text = first;
            while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
                text.push(chars[i]);
                i += 1;
            }
            if !text.is_empty() {
                out.push(RawTerm { col: None, kind, text });
            }
        }
    }
    out
}

impl CompiledSearch {
    pub fn compile(t: &Table, query: &str, opts: &SearchOptions, visible_cols: Vec<usize>) -> Result<Option<Self>, String> {
        if query.trim().is_empty() {
            return Ok(None);
        }
        let mk = |text: &str| -> Result<Regex, String> {
            let esc = regex::escape(text);
            let p = match opts.condition {
                SearchCondition::Contains => esc,
                SearchCondition::StartsWith => format!("^{esc}"),
                SearchCondition::Equals => format!("^{esc}$"),
                SearchCondition::Like => like_to_regex(text),
                SearchCondition::Regex => text.to_string(),
            };
            build_regex(&p, opts.case_sensitive)
        };
        let raw = if opts.match_mode == MatchMode::Exact {
            vec![RawTerm { col: None, kind: TermKind::Required, text: query.trim().to_string() }]
        } else {
            tokenize(query)
        };
        let mut terms = vec![];
        for r in raw {
            let mut col = None;
            let mut text = r.text.clone();
            if let Some(cname) = &r.col {
                match t.find_column(cname) {
                    Some(c) => col = Some(c),
                    // Not a column: treat "a:b" literally.
                    None => text = format!("{cname}:{}", r.text),
                }
            }
            if text.is_empty() {
                continue;
            }
            let kind = if opts.match_mode == MatchMode::All && r.kind == TermKind::Optional { TermKind::Required } else { r.kind };
            terms.push(SearchTerm { col, kind, re: mk(&text)? });
        }
        if terms.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { any: terms.iter().any(|t| t.kind == TermKind::Optional), terms, cols: visible_cols }))
    }

    fn term_matches(&self, term: &SearchTerm, t: &Table, r: usize) -> bool {
        match term.col {
            Some(c) => term.re.is_match(t.cell(r, c)),
            None => self.cols.iter().any(|&c| term.re.is_match(t.cell(r, c))),
        }
    }

    pub fn row_matches(&self, t: &Table, r: usize) -> bool {
        let mut any_hit = !self.any;
        for term in &self.terms {
            match term.kind {
                TermKind::Required => {
                    if !self.term_matches(term, t, r) {
                        return false;
                    }
                }
                TermKind::Excluded => {
                    if self.term_matches(term, t, r) {
                        return false;
                    }
                }
                TermKind::Optional => {
                    if !any_hit && self.term_matches(term, t, r) {
                        any_hit = true;
                    }
                }
            }
        }
        any_hit
    }

    /// Byte ranges to highlight inside a cell of column `col`.
    pub fn highlight_ranges(&self, col: usize, text: &str) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = vec![];
        for term in &self.terms {
            if term.kind == TermKind::Excluded || term.col.is_some_and(|c| c != col) {
                continue;
            }
            for m in term.re.find_iter(text).take(64) {
                if m.end() > m.start() {
                    v.push((m.start(), m.end()));
                }
            }
        }
        v.sort_unstable();
        let mut merged: Vec<(usize, usize)> = vec![];
        for (s, e) in v {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::table_from_rows;

    fn sample() -> Table {
        table_from_rows(
            "t",
            &["Time", "EventId", "Message", "Event Source"],
            &[
                vec!["2024-01-01 10:00:00", "4624", "Logon success user=bob", "Security"],
                vec!["2024-01-02 11:00:00", "4625", "Logon failure user=alice", "Security"],
                vec!["2024-01-03 12:00:00", "7045", "Service installed http://evil:8080/x", "System"],
                vec!["2024-01-04 13:00:00", "", "", "System"],
            ],
        )
    }

    fn rows(t: &Table, p: &Pred) -> Vec<usize> {
        (0..t.nrows).filter(|&r| p.eval(t, r)).collect()
    }

    #[test]
    fn quick_syntax() {
        assert_eq!(parse_quick(">=10"), (Op::GreaterOrEqual, "10".into()));
        assert_eq!(parse_quick("=abc"), (Op::Equals, "abc".into()));
        assert_eq!(parse_quick("/a.c/"), (Op::Regex, "a.c".into()));
        assert_eq!(parse_quick("a*c"), (Op::Like, "a*c".into()));
        assert_eq!(parse_quick("logon"), (Op::Contains, "logon".into()));
        assert_eq!(parse_quick("exe$"), (Op::EndsWith, "exe".into()));
    }

    #[test]
    fn conditions() {
        let t = sample();
        let p = compile_condition(&t, &Condition::new("eventid", Op::Greater, "4624")).unwrap();
        assert_eq!(rows(&t, &p), vec![1, 2]);
        let mut c = Condition::new("Time", Op::Between, "2024-01-02");
        c.value2 = "2024-01-03".into();
        let p = compile_condition(&t, &c).unwrap();
        assert_eq!(rows(&t, &p), vec![1, 2]);
        let p = compile_condition(&t, &Condition::new("Message", Op::IsEmpty, "")).unwrap();
        assert_eq!(rows(&t, &p), vec![3]);
        let mut tree = FilterNode::default();
        tree.push_and(Condition::new("Event Source", Op::Equals, "security"), false);
        tree.push_and(Condition::new("Message", Op::Contains, "alice"), true);
        assert_eq!(rows(&t, &compile_node(&t, &tree).unwrap()), vec![0]);
    }

    #[test]
    fn power_search() {
        let t = sample();
        let cols = vec![0, 1, 2, 3];
        let s = |q: &str, o: SearchOptions| {
            let cs = CompiledSearch::compile(&t, q, &o, cols.clone()).unwrap().unwrap();
            (0..t.nrows).filter(|&r| cs.row_matches(&t, r)).collect::<Vec<_>>()
        };
        let o = SearchOptions::default();
        assert_eq!(s("bob alice", o), vec![0, 1]);
        assert_eq!(s("logon -alice", o), vec![0]);
        assert_eq!(s("+security +failure", o), vec![1]);
        assert_eq!(s("eventid:4625", o), vec![1]);
        assert_eq!(s("\"event source\":system", o), vec![2, 3]);
        assert_eq!(s("http://evil:8080", o), vec![2]);
        assert_eq!(s("logon user", SearchOptions { match_mode: MatchMode::All, ..o }), vec![0, 1]);
        assert_eq!(s("success user", SearchOptions { match_mode: MatchMode::Exact, ..o }), vec![0]);
        let cs = CompiledSearch::compile(&t, "on", &o, cols.clone()).unwrap().unwrap();
        assert_eq!(cs.highlight_ranges(2, "Logon on"), vec![(3, 5), (6, 8)]);
    }
}
