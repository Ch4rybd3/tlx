# TLX — Timeline eXplorer

[![CI](https://github.com/Ch4rybd3/tlx/actions/workflows/ci.yml/badge.svg)](https://github.com/Ch4rybd3/tlx/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Cross-platform (Linux / Windows) forensic timeline & CSV viewer written in Rust,
modelled on Eric Zimmerman's **Timeline Explorer**. Single native binary (egui/eframe, OpenGL).

## Build

Requires a recent stable Rust toolchain (edition 2024, Rust 1.85+).

```bash
git clone https://github.com/Ch4rybd3/tlx.git && cd tlx
cargo build --release            # Linux: target/release/tlx
cargo build --release            # Windows (MSVC toolchain): target\release\tlx.exe
tlx file1.csv file2.xlsx session.tlxs   # files can also be dropped on the window
```

## Features (Timeline Explorer parity)

| Area | What you get |
|---|---|
| Formats | CSV / TSV / TXT (delimiter auto-detected `,` `;` `TAB` `|`, UTF-8/UTF-8 BOM/UTF-16), XLSX/XLS/XLSB/ODS (first sheet only, like TLE). KAPE / EZ Tools output opens directly. |
| EVTX | Native parser (pure Rust, `evtx` crate, multithreaded): drop `.evtx` files or a folder (recursive). Several logs dropped together are merged into one tab sorted by time. EvtxECmd-like columns: RecordNumber, TimeCreated (System/@SystemTime, 100 ns), EventId, Level, Provider, Channel, Computer, MapDescription (≈150 well-known IDs: Security, Sysmon, SCM, PowerShell, TaskScheduler, RDP, Defender, WMI…), UserName, RemoteHost, ExecutableInfo, UserId, ProcessId, ThreadId, Task, Opcode, Keywords (audit success/failure), EventData (all fields flattened), SourceFile. 278 files / 37k events load in ~1 s. |
| Big files | Cells live in one byte arena (≈1.35× file size in RAM), virtualised grid, row-based scrolling. 1M rows / 300 MB loads in < 2 s. Background loading with progress + cancel. |
| Tabs | Multiple files, tab filter box, Ctrl+W / Ctrl+Q, middle-click close, tag counter per tab. |
| Line & Tag columns | Original line number; Tag checkbox (click, Ctrl+T or Space on the selection), "tagged only" toggle, F2 / Shift+F2 jump between tags. |
| Instant filtering | Header funnel → value list with counts, search, (Select All), invert, double-click = only this value, Shift/Ctrl-select then **Space** toggles the range, option to list only values of visible rows. |
| Auto-filter row | Per column: `text` contains · `=x` · `!x` · `<>x` · `^x` · `x$` · `>` `>=` `<` `<=` (typed: numbers, timestamps) · `a*b?` · `/regex/`. |
| Filter editor | Nested AND / OR / NOT groups, 15 operators (contains, equals, like, regex, between, blank…), case sensitivity. |
| Filter bar | Readable description of the whole filter, enable/disable checkbox (Ctrl+Shift+E), Edit, Clear (Ctrl+E). |
| Context-aware filtering | The current row keeps its screen position when filters change or are removed. |
| Power search | Top-right box: `a b`, `"phrase"`, `+must`, `-exclude`, `Column:value`, `"Col name":"value"`; options Filter/Highlight, Contains/Starts/Equals/Like/Regex, Any/All/Exact, case; F3 / Shift+F3; matches highlighted in cells. |
| Sorting | Click / Shift+click (multi-column) / Ctrl+click (remove). Typed sort (numbers, timestamps, case-insensitive text). |
| Grouping | Drag headers to the group panel (multi-level), collapsible groups with counts, expand/collapse all, clear grouping. |
| Columns | Column chooser (show/hide, reorder, hide empty columns), drag headers to reorder, resize, double-click edge = best fit, Ctrl+R = best fit all, "Set as first scrollable column" / freeze. |
| Conditional formatting | Highlight cell rules (contains / equal / greater / less / between / regex), whole-row or cell scope, Excel-like presets, rule manager with ordering. |
| Plaso | l2tcsv and psort "dynamic" output detected; rows colour-coded by `source`, Help > Plaso colour legend. |
| Details pane | Ctrl+D or double-click: every field (hidden ones too), field filter, copy value / copy all; resizable panel **and** draggable field/value divider (double-click = fit names, width remembered), wrap toggle, EventData shown one field per line. |
| Clipboard | Ctrl+C rows with headers (TSV, pastes into Excel), Ctrl+Shift+C current cell, context-menu copies. |
| Timestamps | Display format (.NET `yyyy-MM-dd HH:mm:ss.fff` or strftime), display time-zone offset, original values untouched. |
| Sessions | Save/load `.tlxs` (JSON): files, layout, sort, groups, filters, search, tags, rules, cursor. |
| Export | Visible rows or tagged rows to CSV (refuses to overwrite the source). |
| Skins | Dark / Light / System, font size. |
| Forensic soundness | Source files are opened read-only and never written. |

Press **Help > Quick help** in the app for the full shortcut list.

## Layout

```
src/evtx.rs      EVTX parsing, flattening, event ID descriptions
src/data.rs      arena storage, CSV/Excel loaders, type detection, Plaso detection
src/filter.rs    column filters, filter tree, power-search parser & compiled predicates
src/view.rs      per-tab state: filter → sort → group pipeline, selection, tags
src/grid.rs      custom virtualised grid widget, header filter popup, context menus
src/format.rs    conditional formatting, Plaso legend
src/dialogs.rs   column chooser, filter editor, rules, settings, help
src/session.rs   sessions + settings (~/.config/tlx/settings.json, %APPDATA%\tlx on Windows)
examples/make_fixtures.py   sample EvtxECmd / Plaso / 1M-row timelines
```

`cargo test` covers parsing, filters, power search, sort, grouping, tags and context-aware filtering.
One test also parses the [EVTX-ATTACK-SAMPLES](https://github.com/sbousseaden/EVTX-ATTACK-SAMPLES)
when present: point `TLX_EVTX_SAMPLES` at their folder (skipped otherwise).

## License

[MIT](LICENSE).

TLX is an independent project. It is not affiliated with or endorsed by Eric Zimmerman;
Timeline Explorer is his tool and the name is used only to describe what TLX is modelled on.
