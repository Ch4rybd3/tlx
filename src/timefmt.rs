//! Timestamp / number parsing and display formatting.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};

pub fn parse_number(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return None;
    }
    let mut i = 0;
    if b[0] == b'-' || b[0] == b'+' {
        i = 1;
    }
    let mut digits = 0;
    let mut dot = false;
    let mut exp = false;
    while i < b.len() {
        match b[i] {
            b'0'..=b'9' => digits += 1,
            b'.' if !dot && !exp => dot = true,
            b'e' | b'E' if !exp && digits > 0 => {
                exp = true;
                if i + 1 < b.len() && (b[i + 1] == b'-' || b[i + 1] == b'+') {
                    i += 1;
                }
            }
            _ => return None,
        }
        i += 1;
    }
    if digits == 0 {
        return None;
    }
    s.parse::<f64>().ok()
}

const NAIVE_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M",
    "%Y-%m-%dT%H:%M",
    "%Y/%m/%d %H:%M:%S%.f",
    "%m/%d/%Y %H:%M:%S%.f",
    "%m/%d/%Y %I:%M:%S %p",
    "%m/%d/%Y %I:%M:%S%.f %p",
    "%m/%d/%Y %H:%M",
    "%d.%m.%Y %H:%M:%S%.f",
    "%Y%m%d%H%M%S",
];

const OFFSET_FORMATS: &[&str] = &["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f %:z"];

const DATE_FORMATS: &[&str] = &["%Y-%m-%d", "%Y/%m/%d", "%m/%d/%Y", "%d.%m.%Y"];

/// Parses a timestamp. Values with an explicit offset are converted to UTC;
/// naive values are assumed to already be UTC (the norm for forensic tooling).
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 8 || b.len() > 40 || !b[0].is_ascii_digit() {
        return None;
    }
    // Cheap rejection: a timestamp has separators.
    if !s.contains(['-', '/', '.', ':']) && b.len() != 14 {
        return None;
    }
    let s = s.strip_suffix(" UTC").or_else(|| s.strip_suffix('Z')).or_else(|| s.strip_suffix(" Z")).unwrap_or(s);
    for f in NAIVE_FORMATS {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(t);
        }
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.naive_utc());
    }
    if s.len() > 19 {
        for f in OFFSET_FORMATS {
            if let Ok(t) = DateTime::parse_from_str(s, f) {
                return Some(t.naive_utc());
            }
        }
    }
    if s.len() <= 10 {
        for f in DATE_FORMATS {
            if let Ok(d) = NaiveDate::parse_from_str(s, f) {
                return d.and_hms_opt(0, 0, 0);
            }
        }
    }
    None
}

pub fn to_key(t: NaiveDateTime) -> i64 {
    t.and_utc().timestamp_micros()
}

/// Converts a .NET style format (`yyyy-MM-dd HH:mm:ss.fff`) to a chrono format.
/// Strings already containing `%` are treated as chrono/strftime formats.
pub fn to_chrono_format(fmt: &str) -> String {
    if fmt.contains('%') {
        return fmt.to_string();
    }
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut n = 1;
        while i + n < chars.len() && chars[i + n] == c {
            n += 1;
        }
        let tok = match (c, n) {
            ('y', 4..) => "%Y",
            ('y', _) => "%y",
            ('M', 4..) => "%B",
            ('M', 3) => "%b",
            ('M', 2) => "%m",
            ('M', _) => "%-m",
            ('d', 4..) => "%A",
            ('d', 3) => "%a",
            ('d', 2) => "%d",
            ('d', _) => "%-d",
            ('H', 2..) => "%H",
            ('H', _) => "%-H",
            ('h', 2..) => "%I",
            ('h', _) => "%-I",
            ('m', 2..) => "%M",
            ('m', _) => "%-M",
            ('s', 2..) => "%S",
            ('s', _) => "%-S",
            ('f' | 'F', 1..=3) => "%3f",
            ('f' | 'F', 4..=6) => "%6f",
            ('f' | 'F', _) => "%9f",
            ('t', _) => "%p",
            ('z' | 'K', _) => "%:z",
            ('\'', _) => {
                // quoted literal
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '\'' {
                    out.push(chars[j]);
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            ('%', _) => "%%",
            _ => {
                for _ in 0..n {
                    out.push(c);
                }
                i += n;
                continue;
            }
        };
        out.push_str(tok);
        i += n;
    }
    out
}

pub struct DisplayTime {
    chrono_fmt: String,
    offset: FixedOffset,
    valid: bool,
}

impl DisplayTime {
    pub fn new(fmt: &str, offset_minutes: i32) -> Self {
        let chrono_fmt = to_chrono_format(fmt);
        // Validate the format once so a typo never panics the renderer.
        let valid = chrono::format::StrftimeItems::new(&chrono_fmt).all(|i| !matches!(i, chrono::format::Item::Error));
        let offset = FixedOffset::east_opt(offset_minutes.clamp(-1439, 1439) * 60).unwrap();
        Self { chrono_fmt, offset, valid }
    }

    pub fn format(&self, raw: &str) -> Option<String> {
        if !self.valid || !raw.contains(':') {
            return None;
        }
        use std::fmt::Write;
        let t = self.offset.from_utc_datetime(&parse_datetime(raw)?);
        let mut out = String::new();
        write!(out, "{}", t.format(&self.chrono_fmt)).ok()?;
        Some(out)
    }
}

pub fn format_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let m = minutes.abs();
    format!("UTC{sign}{:02}:{:02}", m / 60, m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        for s in [
            "2024-01-02 03:04:05",
            "2024-01-02 03:04:05.1234567",
            "2024-01-02T03:04:05Z",
            "2024-01-02T03:04:05.123+02:00",
            "01/02/2024 03:04:05 PM",
            "2024-01-02",
            "2024-01-02 03:04:05 UTC",
        ] {
            assert!(parse_datetime(s).is_some(), "{s}");
        }
        assert!(parse_datetime("hello").is_none());
        assert!(parse_datetime("12345").is_none());
        let t = parse_datetime("2024-01-02T03:04:05+02:00").unwrap();
        assert_eq!(t.to_string(), "2024-01-02 01:04:05");
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_number("42"), Some(42.0));
        assert_eq!(parse_number("-1.5e3"), Some(-1500.0));
        assert!(parse_number("1.2.3").is_none());
        assert!(parse_number("0x10").is_none());
        assert!(parse_number("inf").is_none());
        assert!(parse_number("2024-01-01").is_none());
    }

    #[test]
    fn dotnet_format() {
        assert_eq!(to_chrono_format("yyyy-MM-dd HH:mm:ss.fff"), "%Y-%m-%d %H:%M:%S.%3f");
        assert_eq!(to_chrono_format("yyyy-MM-dd'T'HH:mm:ssK"), "%Y-%m-%dT%H:%M:%S%:z");
        let d = DisplayTime::new("yyyy-MM-dd HH:mm:ss", 60);
        assert_eq!(d.format("2024-01-02 23:30:00").unwrap(), "2024-01-03 00:30:00");
    }
}
