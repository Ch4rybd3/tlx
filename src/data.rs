//! In-memory table storage and file loaders (CSV/TSV/XLSX).
//!
//! Cells are stored in a single byte arena to keep the memory footprint close
//! to the size of the source file, even for multi-million row timelines.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use rayon::prelude::*;

use crate::timefmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Text,
    Number,
    DateTime,
}

/// Special kinds of input that get extra treatment (colour coding, legend...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Generic,
    /// Plaso / log2timeline output (l2tcsv or psort "dynamic"); value is the `source` column.
    Plaso { source_col: usize },
}

pub struct Table {
    pub path: PathBuf,
    pub headers: Vec<String>,
    pub col_types: Vec<ColType>,
    pub kind: TableKind,
    pub nrows: usize,
    data: Vec<u8>,
    row_start: Vec<u64>,
    /// End offset of each cell relative to its row start (`nrows * ncols` entries).
    cell_end: Vec<u32>,
    /// Lazily computed sort keys for numeric / datetime columns.
    num_keys: Vec<OnceLock<Vec<f64>>>,
    time_keys: Vec<OnceLock<Vec<i64>>>,
    pub file_size: u64,
    /// Warnings produced while loading (skipped records, …).
    pub notes: Vec<String>,
    /// Files merged into this table (empty = just `path`).
    pub sources: Vec<PathBuf>,
}

impl Table {
    pub fn ncols(&self) -> usize {
        self.headers.len()
    }

    #[inline]
    pub fn cell(&self, row: usize, col: usize) -> &str {
        let n = self.headers.len();
        let base = self.row_start[row] as usize;
        let idx = row * n + col;
        let start = if col == 0 { 0 } else { self.cell_end[idx - 1] as usize };
        let end = self.cell_end[idx] as usize;
        // SAFETY: the arena is only ever filled with complete `&str` values
        // (see `TableBuilder::push_row`) and offsets are taken at str boundaries.
        unsafe { std::str::from_utf8_unchecked(&self.data[base + start..base + end]) }
    }

    pub fn row_values(&self, row: usize) -> impl Iterator<Item = &str> {
        (0..self.ncols()).map(move |c| self.cell(row, c))
    }

    pub fn find_column(&self, name: &str) -> Option<usize> {
        let lname = name.to_lowercase();
        if let Some(i) = self.headers.iter().position(|h| h.to_lowercase() == lname) {
            return Some(i);
        }
        // Unique prefix match
        let cands: Vec<usize> = self
            .headers
            .iter()
            .enumerate()
            .filter(|(_, h)| h.to_lowercase().starts_with(&lname))
            .map(|(i, _)| i)
            .collect();
        if cands.len() == 1 { Some(cands[0]) } else { None }
    }

    pub fn num_keys(&self, col: usize) -> &[f64] {
        self.num_keys[col].get_or_init(|| {
            (0..self.nrows)
                .into_par_iter()
                .map(|r| timefmt::parse_number(self.cell(r, col)).unwrap_or(f64::NEG_INFINITY))
                .collect()
        })
    }

    pub fn time_keys(&self, col: usize) -> &[i64] {
        self.time_keys[col].get_or_init(|| {
            (0..self.nrows)
                .into_par_iter()
                .map(|r| timefmt::parse_datetime(self.cell(r, col)).map(timefmt::to_key).unwrap_or(i64::MIN))
                .collect()
        })
    }

    /// Distinct values of a column (restricted to `rows` if given) with their counts.
    pub fn distinct<'t>(&'t self, col: usize, rows: Option<&[u32]>) -> Vec<(String, u32)> {
        let fold = |m: HashMap<&'t str, u32>, r: usize| count_into(self, col, m, r);
        let map = match rows {
            Some(rows) => rows.par_iter().map(|&r| r as usize).fold(HashMap::new, fold).reduce(HashMap::new, merge_maps),
            None => (0..self.nrows).into_par_iter().fold(HashMap::new, fold).reduce(HashMap::new, merge_maps),
        };
        let mut v: Vec<(String, u32)> = map.into_iter().map(|(k, c)| (k.to_string(), c)).collect();
        match self.col_types[col] {
            ColType::Number => v.par_sort_by(|a, b| {
                let ka = timefmt::parse_number(&a.0).unwrap_or(f64::NEG_INFINITY);
                let kb = timefmt::parse_number(&b.0).unwrap_or(f64::NEG_INFINITY);
                ka.total_cmp(&kb)
            }),
            ColType::DateTime => v.par_sort_by_key(|a| timefmt::parse_datetime(&a.0).map(timefmt::to_key).unwrap_or(i64::MIN)),
            ColType::Text => v.par_sort_by(|a, b| cmp_ci(&a.0, &b.0)),
        }
        v
    }

    /// True if every cell of the column is empty.
    pub fn column_is_empty(&self, col: usize) -> bool {
        (0..self.nrows).into_par_iter().all(|r| self.cell(r, col).trim().is_empty())
    }
}

fn count_into<'a>(t: &'a Table, col: usize, mut m: HashMap<&'a str, u32>, r: usize) -> HashMap<&'a str, u32> {
    *m.entry(t.cell(r, col)).or_insert(0) += 1;
    m
}

fn merge_maps<'a>(mut a: HashMap<&'a str, u32>, mut b: HashMap<&'a str, u32>) -> HashMap<&'a str, u32> {
    if a.len() < b.len() {
        std::mem::swap(&mut a, &mut b);
    }
    for (k, v) in b {
        *a.entry(k).or_insert(0) += v;
    }
    a
}

/// Case-insensitive comparison (ASCII fast path, Unicode-aware fallback).
pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        let ord = a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()));
        return ord.then_with(|| a.cmp(b));
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
        .then_with(|| a.cmp(b))
}

// ---------------------------------------------------------------------------

struct TableBuilder {
    ncols: usize,
    data: Vec<u8>,
    row_start: Vec<u64>,
    cell_end: Vec<u32>,
}

impl TableBuilder {
    fn new(ncols: usize, size_hint: usize) -> Self {
        Self {
            ncols,
            data: Vec::with_capacity(size_hint),
            row_start: Vec::new(),
            cell_end: Vec::new(),
        }
    }

    fn push_row<'a>(&mut self, fields: impl IntoIterator<Item = &'a str>) {
        let base = self.data.len();
        self.row_start.push(base as u64);
        let mut n = 0;
        for f in fields {
            if n < self.ncols {
                self.data.extend_from_slice(f.as_bytes());
                self.cell_end.push(rel(self.data.len() - base));
                n += 1;
            } else {
                // Extra fields beyond the header: append them to the last column.
                self.data.push(b',');
                self.data.extend_from_slice(f.as_bytes());
                *self.cell_end.last_mut().unwrap() = rel(self.data.len() - base);
            }
        }
        while n < self.ncols {
            self.cell_end.push(rel(self.data.len() - base));
            n += 1;
        }
    }

    fn finish(mut self, path: PathBuf, headers: Vec<String>, file_size: u64) -> Table {
        self.data.shrink_to_fit();
        self.row_start.shrink_to_fit();
        self.cell_end.shrink_to_fit();
        let ncols = headers.len();
        let nrows = self.row_start.len();
        let mut t = Table {
            path,
            col_types: vec![ColType::Text; ncols],
            kind: TableKind::Generic,
            nrows,
            data: self.data,
            row_start: self.row_start,
            cell_end: self.cell_end,
            num_keys: (0..ncols).map(|_| OnceLock::new()).collect(),
            time_keys: (0..ncols).map(|_| OnceLock::new()).collect(),
            headers,
            file_size,
            notes: Vec::new(),
            sources: Vec::new(),
        };
        t.col_types = (0..ncols).into_par_iter().map(|c| detect_type(&t, c)).collect();
        t.kind = detect_kind(&t.headers);
        t
    }
}

#[inline]
fn rel(v: usize) -> u32 {
    u32::try_from(v).expect("a single row exceeds 4 GiB")
}

fn detect_type(t: &Table, col: usize) -> ColType {
    let n = t.nrows;
    if n == 0 {
        return ColType::Text;
    }
    let step = (n / 2000).max(1);
    let (mut seen, mut nums, mut dates) = (0usize, 0usize, 0usize);
    let mut r = 0;
    while r < n && seen < 2000 {
        let v = t.cell(r, col).trim();
        if !v.is_empty() {
            seen += 1;
            if timefmt::parse_number(v).is_some() {
                nums += 1;
            } else if timefmt::parse_datetime(v).is_some() {
                dates += 1;
            }
        }
        r += step;
    }
    if seen == 0 {
        ColType::Text
    } else if nums == seen {
        ColType::Number
    } else if dates == seen {
        ColType::DateTime
    } else {
        ColType::Text
    }
}

fn detect_kind(headers: &[String]) -> TableKind {
    let has = |n: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(n));
    let src = has("source");
    let l2t = has("MACB").is_some() && has("sourcetype").is_some();
    let dynamic = has("timestamp_desc").is_some() && (has("source_long").is_some() || has("parser").is_some());
    match src {
        Some(source_col) if l2t || dynamic => TableKind::Plaso { source_col },
        _ => TableKind::Generic,
    }
}

fn make_headers(raw: Vec<String>) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    raw.into_iter()
        .enumerate()
        .map(|(i, h)| {
            let h = h.trim().trim_start_matches('\u{feff}').to_string();
            let base = if h.is_empty() { format!("Column{}", i + 1) } else { h };
            let n = seen.entry(base.to_lowercase()).or_insert(0);
            *n += 1;
            if *n == 1 { base } else { format!("{base} ({n})") }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Loading

#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// Records parsed so far (EVTX), shown while loading.
    pub records: AtomicU64,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { (self.bytes.load(Ordering::Relaxed) as f64 / t as f64).min(1.0) as f32 }
    }
}

struct CountingReader<R> {
    inner: R,
    progress: Arc<Progress>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.progress.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.progress.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Loads one source, or merges several EVTX files into one table.
pub fn load_sources(label: &Path, sources: &[PathBuf], progress: Arc<Progress>) -> Result<Table, String> {
    if sources.len() <= 1 {
        return load_file(sources.first().map(PathBuf::as_path).unwrap_or(label), progress);
    }
    let mut t = crate::evtx::load_files(label, sources, &progress)?;
    t.sources = sources.to_vec();
    Ok(t)
}

pub fn is_evtx(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("evtx"))
}

pub fn load_file(path: &Path, progress: Arc<Progress>) -> Result<Table, String> {
    if path.is_dir() {
        return crate::evtx::load_dir(path, &progress);
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    progress.total.store(meta.len(), Ordering::Relaxed);
    match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => load_excel(path, meta.len(), &progress),
        "evtx" => crate::evtx::load_files(path, &[path.to_path_buf()], &progress),
        _ => load_delimited(path, &ext, meta.len(), progress),
    }
}

fn sniff_delimiter(sample: &[u8], ext: &str) -> u8 {
    if ext == "tsv" || ext == "tab" {
        return b'\t';
    }
    let text = String::from_utf8_lossy(sample);
    let first = text.lines().next().unwrap_or("");
    let mut best = (b',', 0usize);
    for d in *b",\t;|" {
        let mut in_q = false;
        let mut count = 0;
        for c in first.bytes() {
            if c == b'"' {
                in_q = !in_q;
            } else if c == d && !in_q {
                count += 1;
            }
        }
        if count > best.1 {
            best = (d, count);
        }
    }
    best.0
}

fn load_delimited(path: &Path, ext: &str, size: u64, progress: Arc<Progress>) -> Result<Table, String> {
    let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut head = vec![0u8; 64 * 1024];
    let n = read_up_to(&mut f, &mut head).map_err(|e| e.to_string())?;
    head.truncate(n);

    // UTF-16 input (some Windows tools emit it): decode fully up front.
    let utf16 = if head.starts_with(&[0xFF, 0xFE]) {
        Some(true)
    } else if head.starts_with(&[0xFE, 0xFF]) {
        Some(false)
    } else {
        None
    };
    if let Some(le) = utf16 {
        let mut all = head;
        f.read_to_end(&mut all).map_err(|e| e.to_string())?;
        progress.bytes.store(size, Ordering::Relaxed);
        let units: Vec<u16> = all[2..]
            .as_chunks::<2>().0.iter()
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        let text = String::from_utf16_lossy(&units);
        let delim = sniff_delimiter(text.as_bytes(), ext);
        let reader = csv_builder(delim).from_reader(text.as_bytes());
        return parse_csv(reader, path, size, &progress);
    }

    let delim = sniff_delimiter(&head, ext);
    progress.bytes.store(n as u64, Ordering::Relaxed);
    let chained = std::io::Cursor::new(head).chain(CountingReader { inner: f, progress: progress.clone() });
    let reader = csv_builder(delim).from_reader(BufReader::with_capacity(1 << 20, chained));
    parse_csv(reader, path, size, &progress)
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

fn csv_builder(delim: u8) -> csv::ReaderBuilder {
    let mut b = csv::ReaderBuilder::new();
    b.delimiter(delim).has_headers(false).flexible(true).double_quote(true);
    b
}

fn parse_csv<R: Read>(mut reader: csv::Reader<R>, path: &Path, size: u64, progress: &Progress) -> Result<Table, String> {
    let mut rec = csv::ByteRecord::new();
    let mut headers: Option<Vec<String>> = None;
    let mut builder: Option<TableBuilder> = None;
    let mut scratch: Vec<String> = Vec::new();
    loop {
        match reader.read_byte_record(&mut rec) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => {
                if progress.cancel.load(Ordering::Relaxed) {
                    return Err("Loading cancelled".into());
                }
                return Err(format!("CSV parse error: {e}"));
            }
        }
        match &mut builder {
            None => {
                let raw: Vec<String> = rec.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect();
                let h = make_headers(raw);
                builder = Some(TableBuilder::new(h.len(), size as usize));
                headers = Some(h);
            }
            Some(b) => {
                if rec.iter().all(|f| f.is_empty()) && rec.len() <= 1 {
                    continue; // skip blank lines
                }
                if rec.iter().all(|f| std::str::from_utf8(f).is_ok()) {
                    b.push_row(rec.iter().map(|f| unsafe { std::str::from_utf8_unchecked(f) }));
                } else {
                    scratch.clear();
                    scratch.extend(rec.iter().map(|f| String::from_utf8_lossy(f).into_owned()));
                    b.push_row(scratch.iter().map(String::as_str));
                }
            }
        }
    }
    let headers = headers.ok_or("The file is empty")?;
    progress.bytes.store(size, Ordering::Relaxed);
    Ok(builder.unwrap().finish(path.to_path_buf(), headers, size))
}

fn load_excel(path: &Path, size: u64, progress: &Progress) -> Result<Table, String> {
    use calamine::{Data, Reader, open_workbook_auto};
    let mut wb = open_workbook_auto(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = wb.sheet_names().first().cloned().ok_or("Workbook has no sheet")?;
    let range = wb.worksheet_range(&name).map_err(|e| e.to_string())?;
    progress.bytes.store(size / 2, Ordering::Relaxed);
    let to_s = |d: &Data| -> String {
        match d {
            Data::Empty => String::new(),
            Data::String(s) => s.clone(),
            Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
            Data::Float(f) => f.to_string(),
            Data::Int(i) => i.to_string(),
            Data::Bool(b) => b.to_string(),
            Data::DateTime(dt) => match dt.as_datetime() {
                Some(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
                None => dt.to_string(),
            },
            Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
            Data::Error(e) => format!("#{e:?}"),
            #[allow(unreachable_patterns)]
            other => other.to_string(),
        }
    };
    let mut rows = range.rows();
    let headers = make_headers(rows.next().map(|r| r.iter().map(to_s).collect()).unwrap_or_default());
    if headers.is_empty() {
        return Err("The first sheet is empty".into());
    }
    let mut b = TableBuilder::new(headers.len(), size as usize);
    let mut buf: Vec<String> = Vec::new();
    for r in rows {
        buf.clear();
        buf.extend(r.iter().map(to_s));
        if buf.iter().all(String::is_empty) {
            continue;
        }
        b.push_row(buf.iter().map(String::as_str));
    }
    progress.bytes.store(size, Ordering::Relaxed);
    Ok(b.finish(path.to_path_buf(), headers, size))
}

/// Builds a table from owned rows produced by a parser (EVTX, …).
pub fn table_from_owned(path: PathBuf, headers: Vec<String>, rows: impl IntoIterator<Item = Vec<String>>, file_size: u64) -> Table {
    let h = make_headers(headers);
    let mut b = TableBuilder::new(h.len(), file_size as usize / 2);
    for r in rows {
        b.push_row(r.iter().map(String::as_str));
    }
    b.finish(path, h, file_size)
}

/// Builds a table from in-memory rows (used by tests and merged views).
#[allow(dead_code)]
pub fn table_from_rows(path: &str, headers: &[&str], rows: &[Vec<&str>]) -> Table {
    let h = make_headers(headers.iter().map(|s| s.to_string()).collect());
    let mut b = TableBuilder::new(h.len(), 0);
    for r in rows {
        b.push_row(r.iter().copied());
    }
    b.finish(PathBuf::from(path), h, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_roundtrip() {
        let t = table_from_rows("x", &["a", "b", ""], &[vec!["1", "hello", "x"], vec!["2"], vec!["3", "é", "z", "extra"]]);
        assert_eq!(t.headers, vec!["a", "b", "Column3"]);
        assert_eq!(t.cell(0, 1), "hello");
        assert_eq!(t.cell(1, 1), "");
        assert_eq!(t.cell(1, 2), "");
        assert_eq!(t.cell(2, 1), "é");
        assert_eq!(t.cell(2, 2), "z,extra");
        assert_eq!(t.col_types[0], ColType::Number);
    }

    #[test]
    fn sniff() {
        assert_eq!(sniff_delimiter(b"a;b;c\n1;2;3", "csv"), b';');
        assert_eq!(sniff_delimiter(b"a\tb\n", "txt"), b'\t');
        assert_eq!(sniff_delimiter(b"\"a,b\";c;d\n", "csv"), b';');
    }
}
