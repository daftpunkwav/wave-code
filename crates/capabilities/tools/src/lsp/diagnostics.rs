//! Bounded store of server-pushed `textDocument/publishDiagnostics`
//! diagnostics: the shared sink clients record into and the
//! `lsp_diagnostics` tool reads back.

use std::collections::HashMap;

use serde_json::Value;

/// Cap on diagnostics kept per file: entries beyond the cap are dropped on
/// each push (a chatty server cannot grow memory without bound; each push
/// still fully replaces the file's previous list per LSP semantics).
pub(super) const MAX_DIAGNOSTICS_PER_FILE: usize = 200;
/// Cap on tracked files: the oldest-inserted file is evicted FIFO when a new
/// file would push the count past the cap.
pub(super) const MAX_DIAGNOSTIC_FILES: usize = 32;

/// One stored diagnostic: the flattened fields the text renderer needs
/// (positions kept 0-based exactly as LSP addresses them).
pub(super) struct DiagnosticEntry {
    line: u32,
    character: u32,
    severity: u8,
    message: String,
}

impl DiagnosticEntry {
    /// Flatten one LSP `Diagnostic` object; entries without a usable range
    /// start or message are skipped (defensive: the schema is advisory).
    fn parse(d: &Value) -> Option<Self> {
        let start = d.get("range")?.get("start")?;
        Some(Self {
            line: start.get("line")?.as_u64()? as u32,
            character: start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
            severity: d
                .get("severity")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(u8::MAX as u64) as u8,
            message: d.get("message")?.as_str()?.to_owned(),
        })
    }
}

/// Bounded store of server-pushed diagnostics, keyed by the document URI
/// exactly as the server addressed it. Each `textDocument/publishDiagnostics`
/// notification fully replaces its file's list (LSP push semantics); the
/// per-file list is capped at [`MAX_DIAGNOSTICS_PER_FILE`] and the number of
/// tracked files at [`MAX_DIAGNOSTIC_FILES`] (oldest-inserted file evicted
/// FIFO).
#[derive(Default)]
pub struct DiagnosticsStore {
    pub(super) files: HashMap<String, Vec<DiagnosticEntry>>,
    /// Insertion order of tracked URIs (drives the FIFO eviction at the file
    /// cap; never holds duplicates — a URI is pushed while untracked exactly
    /// once, and eviction removes it from both sides).
    pub(super) order: Vec<String>,
}

impl DiagnosticsStore {
    /// Record one push: `diagnostics` is the notification's array and
    /// replaces any previous list for `uri`. Malformed entries are dropped;
    /// the per-file cap keeps the first N entries.
    pub fn record(&mut self, uri: &str, diagnostics: &[Value]) {
        let entries: Vec<DiagnosticEntry> = diagnostics
            .iter()
            .take(MAX_DIAGNOSTICS_PER_FILE)
            .filter_map(DiagnosticEntry::parse)
            .collect();
        if !self.files.contains_key(uri) {
            while self.order.len() >= MAX_DIAGNOSTIC_FILES {
                let Some(oldest) = self.order.first() else {
                    break;
                };
                let oldest = oldest.clone();
                self.files.remove(&oldest);
                self.order.remove(0);
            }
            self.order.push(uri.to_owned());
        }
        self.files.insert(uri.to_owned(), entries);
    }

    /// Render stored diagnostics as compact text: one `line:col: severity:
    /// message` entry per diagnostic (1-based, matching common compiler
    /// output), grouped under each file URI. `uri_filter` restricts the
    /// output to one file; `None` renders every tracked file. Files whose
    /// latest push cleared their list are skipped.
    pub fn render(&self, uri_filter: Option<&str>) -> String {
        let mut out = String::new();
        match uri_filter {
            Some(uri) => {
                if let Some(entries) = self.files.get(uri) {
                    render_file(&mut out, uri, entries);
                }
            }
            None => {
                let mut uris: Vec<&String> = self.files.keys().collect();
                uris.sort(); // deterministic order
                for uri in uris {
                    render_file(&mut out, uri, &self.files[uri]);
                }
            }
        }
        if out.is_empty() {
            match uri_filter {
                Some(uri) => format!("no diagnostics recorded for {uri}"),
                None => "no diagnostics recorded".to_owned(),
            }
        } else {
            out.trim_end().to_owned()
        }
    }
}

/// Append one file's block: the URI, then one indented line per diagnostic
/// (messages are kept on one physical line by escaping embedded newlines).
fn render_file(out: &mut String, uri: &str, entries: &[DiagnosticEntry]) {
    if entries.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(uri);
    out.push('\n');
    for e in entries {
        out.push_str(&format!(
            "  {}:{}: {}: {}\n",
            e.line + 1,
            e.character + 1,
            severity_label(e.severity),
            e.message.replace('\n', "\\n")
        ));
    }
}

/// LSP severity number to label (1 error / 2 warning / 3 information /
/// 4 hint; anything else renders as the generic "diagnostic").
fn severity_label(severity: u8) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "diagnostic",
    }
}
