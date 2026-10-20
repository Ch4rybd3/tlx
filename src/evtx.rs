//! Windows Event Log (.evtx) ingestion: records are flattened into an
//! EvtxECmd-like table (one row per event) that the grid can filter/sort.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use evtx::{EvtxParser, ParserSettings};
use serde_json::Value;

use crate::data::{Progress, Table, table_from_owned};

pub const HEADERS: &[&str] = &[
    "RecordNumber",
    "TimeCreated",
    "EventId",
    "Level",
    "Provider",
    "Channel",
    "Computer",
    "MapDescription",
    "UserName",
    "RemoteHost",
    "ExecutableInfo",
    "UserId",
    "ProcessId",
    "ThreadId",
    "Task",
    "Opcode",
    "Keywords",
    "EventData",
    "SourceFile",
];

/// Collects every `.evtx` file below `dir` (recursively).
pub fn find_evtx(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("evtx")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

pub fn load_dir(dir: &Path, progress: &Progress) -> Result<Table, String> {
    let files = find_evtx(dir);
    if files.is_empty() {
        return Err(format!("No .evtx file found in {}", dir.display()));
    }
    load_files(dir, &files, progress)
}

/// Parses one or several EVTX files into a single table. Several files are
/// merged and ordered by time (a mini super-timeline of the event logs).
pub fn load_files(label: &Path, files: &[PathBuf], progress: &Progress) -> Result<Table, String> {
    let total: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    progress.total.store(total.max(1), Ordering::Relaxed);
    let mut rows: Vec<(i64, Vec<String>)> = Vec::new();
    let mut notes = vec![];
    let mut done_bytes = 0u64;
    for f in files {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err("Loading cancelled".into());
        }
        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        match parse_file(f, &mut rows, progress) {
            Ok(0) => {}
            Ok(errors) => notes.push(format!("{}: {errors} corrupted record(s) skipped", file_name(f))),
            Err(e) if files.len() == 1 => return Err(e),
            Err(e) => notes.push(format!("{}: {e}", file_name(f))),
        }
        done_bytes += size;
        progress.bytes.store(done_bytes, Ordering::Relaxed);
    }
    if files.len() > 1 {
        // Stable: records of the same instant keep their file/record order.
        rows.sort_by_key(|r| r.0);
    }
    let headers = HEADERS.iter().map(|s| s.to_string()).collect();
    let mut t = table_from_owned(label.to_path_buf(), headers, rows.into_iter().map(|r| r.1), total);
    if files.len() > 1 {
        notes.insert(0, format!("{} EVTX files merged", files.len()));
    }
    t.notes = notes;
    Ok(t)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Returns the number of records that could not be parsed.
fn parse_file(path: &Path, rows: &mut Vec<(i64, Vec<String>)>, progress: &Progress) -> Result<usize, String> {
    let settings = ParserSettings::new().num_threads(0).separate_json_attributes(false);
    let mut parser = EvtxParser::from_path(path).map_err(|e| format!("{}: {e}", path.display()))?.with_configuration(settings);
    let source = path.display().to_string();
    let mut errors = 0;
    for rec in parser.records_json_value() {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err("Loading cancelled".into());
        }
        match rec {
            Ok(r) => {
                // Prefer System/TimeCreated/@SystemTime (what EvtxECmd shows); the
                // record header timestamp is a fallback (it can be zeroed).
                let (secs, nanos) = system_time(&r.data)
                    .unwrap_or((r.timestamp.as_second(), r.timestamp.subsec_nanosecond().max(0) as u32));
                let key = secs * 1_000_000 + (nanos / 1000) as i64;
                let time = chrono::DateTime::from_timestamp(secs, nanos)
                    .map(|d| format!("{}.{:07}", d.format("%Y-%m-%d %H:%M:%S"), nanos / 100))
                    .unwrap_or_default();
                rows.push((key, record_row(r.event_record_id, time, &r.data, &source)));
                progress.records.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => errors += 1,
        }
    }
    Ok(errors)
}

// ---------------------------------------------------------------------------
// JSON → row

fn system_time(data: &Value) -> Option<(i64, u32)> {
    let ev = data.get("Event").unwrap_or(data);
    let st = ev.get("System")?.get("TimeCreated")?.get("#attributes")?.get("SystemTime")?.as_str()?;
    let t = chrono::DateTime::parse_from_rfc3339(st).ok()?;
    Some((t.timestamp(), t.timestamp_subsec_nanos()))
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Object(m) => m.get("#text").and_then(scalar),
        Value::Array(a) => Some(a.iter().filter_map(scalar).collect::<Vec<_>>().join(", ")),
    }
}

fn attr<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    v.get("#attributes")?.get(name)
}

/// Flattens EventData / UserData into `(name, value)` pairs.
fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    let join = |k: &str| if prefix.is_empty() { k.to_string() } else { format!("{prefix}.{k}") };
    match v {
        Value::Null => {}
        Value::Object(m) => {
            for (k, val) in m {
                match k.as_str() {
                    "#attributes" => {
                        if let Value::Object(attrs) = val {
                            for (ak, av) in attrs {
                                if ak != "xmlns" && !ak.starts_with("xmlns") {
                                    flatten(&join(ak), av, out);
                                }
                            }
                        }
                    }
                    "#text" => flatten(prefix, val, out),
                    _ => flatten(&join(k), val, out),
                }
            }
        }
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            let s: Vec<String> = a.iter().filter_map(scalar).collect();
            out.push((prefix.to_string(), s.join(", ")));
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                flatten(&format!("{prefix}[{i}]"), x, out);
            }
        }
        other => {
            if let Some(s) = scalar(other) {
                out.push((if prefix.is_empty() { "Data".into() } else { prefix.to_string() }, s));
            }
        }
    }
}

fn find<'a>(fields: &'a [(String, String)], names: &[&str]) -> Option<&'a str> {
    for n in names {
        if let Some((_, v)) = fields.iter().find(|(k, v)| {
            let leaf = k.rsplit('.').next().unwrap_or(k);
            leaf.eq_ignore_ascii_case(n) && !v.trim().is_empty() && v != "-"
        }) {
            return Some(v);
        }
    }
    None
}

fn level_name(l: &str) -> String {
    match l {
        "0" => "LogAlways",
        "1" => "Critical",
        "2" => "Error",
        "3" => "Warning",
        "4" => "Info",
        "5" => "Verbose",
        other => other,
    }
    .to_string()
}

fn keywords_name(k: &str) -> String {
    let lk = k.to_ascii_lowercase();
    if lk.starts_with("0x8020") {
        format!("Audit success ({k})")
    } else if lk.starts_with("0x8010") {
        format!("Audit failure ({k})")
    } else {
        k.to_string()
    }
}

fn record_row(record_id: u64, time: String, data: &Value, source: &str) -> Vec<String> {
    let ev = data.get("Event").unwrap_or(data);
    let sys = ev.get("System").cloned().unwrap_or(Value::Null);
    let s = |name: &str| sys.get(name).and_then(scalar).unwrap_or_default();
    let provider = sys.get("Provider").and_then(|p| attr(p, "Name")).and_then(scalar).unwrap_or_default();
    let event_id = s("EventID");
    let exec = sys.get("Execution");
    let exec_attr = |n: &str| exec.and_then(|e| attr(e, n)).and_then(scalar).unwrap_or_default();
    let user_id = sys.get("Security").and_then(|e| attr(e, "UserID")).and_then(scalar).unwrap_or_default();

    let mut fields = vec![];
    for section in ["EventData", "UserData"] {
        if let Some(v) = ev.get(section) {
            flatten("", v, &mut fields);
        }
    }
    let user = match (find(&fields, &["TargetUserName"]), find(&fields, &["TargetDomainName"])) {
        (Some(u), Some(d)) => format!("{d}\\{u}"),
        (Some(u), None) => u.to_string(),
        _ => match (find(&fields, &["SubjectUserName"]), find(&fields, &["SubjectDomainName"])) {
            (Some(u), Some(d)) => format!("{d}\\{u}"),
            (Some(u), None) => u.to_string(),
            _ => find(&fields, &["User", "UserName", "AccountName", "SamAccountName", "Param1"]).unwrap_or_default().to_string(),
        },
    };
    let remote = find(
        &fields,
        &["IpAddress", "SourceAddress", "SourceIp", "ClientAddress", "Address", "WorkstationName", "Workstation", "DestinationIp"],
    )
    .unwrap_or_default()
    .to_string();
    let executable = find(
        &fields,
        &[
            "CommandLine",
            "NewProcessName",
            "Image",
            "ProcessName",
            "ImagePath",
            "ServiceFileName",
            "Application",
            "TaskName",
            "ScriptBlockText",
            "HostApplication",
        ],
    )
    .unwrap_or_default()
    .to_string();
    let event_data = fields.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(" | ");
    let channel = s("Channel");
    vec![
        record_id.to_string(),
        time,
        event_id.clone(),
        level_name(&s("Level")),
        provider.clone(),
        channel.clone(),
        s("Computer"),
        describe(&provider, &channel, &event_id).to_string(),
        user,
        remote,
        executable,
        user_id,
        exec_attr("ProcessID"),
        exec_attr("ThreadID"),
        s("Task"),
        s("Opcode"),
        keywords_name(&s("Keywords")),
        event_data,
        source.to_string(),
    ]
}

/// Short descriptions of well-known event IDs (subset of EvtxECmd maps).
pub fn describe(provider: &str, channel: &str, id: &str) -> &'static str {
    let p = provider.to_ascii_lowercase();
    let c = channel.to_ascii_lowercase();
    let id: u32 = id.trim().parse().unwrap_or(u32::MAX);
    if p.contains("sysmon") {
        return match id {
            1 => "Process creation",
            2 => "File creation time changed",
            3 => "Network connection",
            4 => "Sysmon service state changed",
            5 => "Process terminated",
            6 => "Driver loaded",
            7 => "Image loaded",
            8 => "CreateRemoteThread",
            9 => "RawAccessRead",
            10 => "Process accessed",
            11 => "File created",
            12 => "Registry object added or deleted",
            13 => "Registry value set",
            14 => "Registry object renamed",
            15 => "File stream created",
            16 => "Sysmon config changed",
            17 => "Pipe created",
            18 => "Pipe connected",
            19..=21 => "WMI event",
            22 => "DNS query",
            23 => "File delete archived",
            24 => "Clipboard changed",
            25 => "Process tampering",
            26 => "File delete logged",
            27 => "File block executable",
            28 => "File block shredding",
            29 => "File executable detected",
            _ => "",
        };
    }
    if p.contains("security-auditing") || c == "security" {
        return match id {
            1102 => "The audit log was cleared",
            4624 => "Successful logon",
            4625 => "Failed logon",
            4634 => "Logoff",
            4647 => "User initiated logoff",
            4648 => "Logon with explicit credentials",
            4672 => "Special privileges assigned to new logon",
            4688 => "Process created",
            4689 => "Process exited",
            4697 => "Service installed",
            4698 => "Scheduled task created",
            4699 => "Scheduled task deleted",
            4700 => "Scheduled task enabled",
            4701 => "Scheduled task disabled",
            4702 => "Scheduled task updated",
            4719 => "System audit policy changed",
            4720 => "User account created",
            4722 => "User account enabled",
            4723 => "Password change attempt",
            4724 => "Password reset attempt",
            4725 => "User account disabled",
            4726 => "User account deleted",
            4728 => "Member added to global security group",
            4732 => "Member added to local security group",
            4738 => "User account changed",
            4740 => "User account locked out",
            4756 => "Member added to universal security group",
            4768 => "Kerberos TGT requested",
            4769 => "Kerberos service ticket requested",
            4771 => "Kerberos pre-authentication failed",
            4776 => "NTLM credential validation",
            4778 => "Session reconnected (RDP)",
            4779 => "Session disconnected (RDP)",
            4798 => "Local group membership enumerated",
            4799 => "Security group membership enumerated",
            5140 => "Network share accessed",
            5145 => "Network share object checked",
            5156 => "WFP allowed connection",
            5157 => "WFP blocked connection",
            _ => "",
        };
    }
    if p.contains("service control manager") {
        return match id {
            7034 => "Service crashed unexpectedly",
            7035 => "Service control request sent",
            7036 => "Service state changed",
            7040 => "Service start type changed",
            7045 => "Service installed",
            _ => "",
        };
    }
    if p.contains("powershell") {
        return match id {
            400 => "PowerShell engine started",
            403 => "PowerShell engine stopped",
            600 => "PowerShell provider started",
            800 => "Pipeline execution details",
            4100 => "PowerShell error",
            4103 => "Module logging",
            4104 => "Script block logging",
            _ => "",
        };
    }
    if p.contains("taskscheduler") {
        return match id {
            106 => "Scheduled task registered",
            140 => "Scheduled task updated",
            141 => "Scheduled task deleted",
            200 => "Scheduled task action started",
            201 => "Scheduled task action completed",
            _ => "",
        };
    }
    if p.contains("terminalservices-localsessionmanager") {
        return match id {
            21 => "RDP session logon",
            22 => "RDP shell start",
            23 => "RDP session logoff",
            24 => "RDP session disconnected",
            25 => "RDP session reconnected",
            _ => "",
        };
    }
    if p.contains("terminalservices-remoteconnectionmanager") && id == 1149 {
        return "RDP user authentication succeeded";
    }
    if p.contains("windows defender") {
        return match id {
            1116 => "Malware detected",
            1117 => "Malware action taken",
            5001 => "Real-time protection disabled",
            5007 => "Defender configuration changed",
            _ => "",
        };
    }
    if p.contains("wmi-activity") {
        return match id {
            5857 => "WMI provider loaded",
            5860 => "WMI temporary event consumer",
            5861 => "WMI permanent event consumer",
            _ => "",
        };
    }
    if p.contains("eventlog") {
        return match id {
            104 => "Event log cleared",
            1102 => "The audit log was cleared",
            6005 => "Event log service started",
            6006 => "Event log service stopped",
            _ => "",
        };
    }
    if p.contains("kernel-general") && id == 1 {
        return "System time changed";
    }
    if p.contains("user32") && id == 1074 {
        return "Shutdown / restart initiated";
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_event_data() {
        let v: Value = serde_json::json!({
            "Event": {
                "System": {
                    "Provider": {"#attributes": {"Name": "Microsoft-Windows-Security-Auditing"}},
                    "EventID": 4624,
                    "Level": 0,
                    "Channel": "Security",
                    "Computer": "WKS-01",
                    "Keywords": "0x8020000000000000",
                    "Execution": {"#attributes": {"ProcessID": 600, "ThreadID": 700}},
                    "Security": null
                },
                "EventData": {"TargetUserName": "bob", "TargetDomainName": "CORP", "IpAddress": "10.0.0.5", "LogonType": 3}
            }
        });
        let row = record_row(42, "2024-01-01 00:00:00.0000000".into(), &v, "Security.evtx");
        let get = |h: &str| row[HEADERS.iter().position(|x| *x == h).unwrap()].clone();
        assert_eq!(get("EventId"), "4624");
        assert_eq!(get("Level"), "LogAlways");
        assert_eq!(get("MapDescription"), "Successful logon");
        assert_eq!(get("UserName"), "CORP\\bob");
        assert_eq!(get("RemoteHost"), "10.0.0.5");
        assert_eq!(get("ProcessId"), "600");
        assert!(get("Keywords").starts_with("Audit success"));
        assert!(get("EventData").contains("LogonType: 3"));
    }

    #[test]
    fn eventid_with_qualifiers() {
        let v: Value = serde_json::json!({"Event": {"System": {"EventID": {"#attributes": {"Qualifiers": 16384}, "#text": 7045},
            "Provider": {"#attributes": {"Name": "Service Control Manager"}}},
            "EventData": {"Data": {"#text": ["a", "b"]}}}});
        let row = record_row(1, String::new(), &v, "System.evtx");
        assert_eq!(row[2], "7045");
        assert_eq!(row[7], "Service installed");
        assert!(row[17].contains("a, b"));
    }

    /// Parses the real attack samples (EVTX-ATTACK-SAMPLES) when present on the machine.
    /// Set TLX_EVTX_SAMPLES to their folder; defaults to ~/Downloads/EVTX-ATTACK-SAMPLES-master.
    #[test]
    fn real_samples_if_available() {
        let dir = std::env::var_os("TLX_EVTX_SAMPLES")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap().join("Downloads/EVTX-ATTACK-SAMPLES-master"));
        if !dir.exists() {
            return;
        }
        let files: Vec<PathBuf> = find_evtx(&dir).into_iter().take(400).collect();
        let t = load_files(&dir, &files, &Progress::default()).unwrap();
        assert!(t.nrows > 100, "{} rows", t.nrows);
        assert_eq!(t.col_types[1], crate::data::ColType::DateTime);
        assert_eq!(t.col_types[2], crate::data::ColType::Number);
        let early = (0..t.nrows).filter(|&r| t.cell(r, 1).starts_with("1601")).count();
        assert_eq!(early, 0, "records with a zero timestamp");
    }
}
