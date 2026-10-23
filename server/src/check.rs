//! Offline lint mode (`mqsc-lsp --check`): run the same analyzer the LSP
//! uses over files (or stdin with `-`) and print machine-readable lines
//!
//! ```text
//! file:line:col: severity[code]: message
//! ```
//!
//! with 1-based line/col. Exit code: 0 = clean or below-error, 1 = at least
//! one error-severity diagnostic, 2 = usage/IO problem. Severity settings are
//! honored exactly like in the editor (`--diag CODE=SEV` / `--settings file`).

use std::io::Read;
use std::path::Path;

use lsp_types::{DiagnosticSeverity, NumberOrString, Url};
use serde_json::{json, Value};

use crate::analysis;
use crate::model::Model;
use crate::settings::{apply_diagnostic_settings, diagnostic_settings};

fn sev_name(s: Option<DiagnosticSeverity>) -> &'static str {
    match s {
        Some(s) if s == DiagnosticSeverity::ERROR => "error",
        Some(s) if s == DiagnosticSeverity::INFORMATION => "information",
        Some(s) if s == DiagnosticSeverity::HINT => "hint",
        _ => "warning",
    }
}

fn code_str(code: &Option<NumberOrString>) -> String {
    match code {
        Some(NumberOrString::Number(n)) => n.to_string(),
        Some(NumberOrString::String(s)) => s.clone(),
        None => String::new(),
    }
}

fn check_one(display: &str, text: &str, settings: Option<&Value>, out: &mut String) -> bool {
    let m = Model::load();
    let uri = Url::from_directory_path(if display == "-" { "<stdin>" } else { display })
        .unwrap_or_else(|_| Url::parse("file:///-").unwrap());
    let tree = analysis::parse(text);
    let mut diags = analysis::analyze(text, &tree, m, &uri, &analysis::Definitions::default());
    if let Some(root) = settings {
        if let Some(map) = diagnostic_settings(root) {
            apply_diagnostic_settings(&mut diags, map);
        }
    }
    diags.sort_by_key(|d| {
        (
            d.range.start.line,
            d.range.start.character,
            code_str(&d.code),
        )
    });
    let mut any_error = false;
    for d in &diags {
        let code = code_str(&d.code);
        let sev = sev_name(d.severity);
        if sev == "error" {
            any_error = true;
        }
        // single-line messages keep the linter output grep-friendly
        let msg = d.message.replace('\n', " ");
        out.push_str(&format!(
            "{}:{}:{}: {}[{}]: {}\n",
            display,
            d.range.start.line + 1,
            d.range.start.character + 1,
            sev,
            code,
            msg
        ));
    }
    any_error
}

/// Lint each path (`-` = stdin); returns the process exit code.
pub fn run(files: &[String], settings: Option<&Value>) -> i32 {
    let mut any_error = false;
    let mut failed_io = false;
    for f in files {
        let (text, display) = if f == "-" {
            let mut buf = String::new();
            match std::io::stdin().read_to_string(&mut buf) {
                Ok(_) => (buf, "-"),
                Err(e) => {
                    eprintln!("mqsc-lsp: cannot read stdin: {e}");
                    failed_io = true;
                    continue;
                }
            }
        } else {
            match std::fs::read_to_string(f) {
                Ok(t) => (t, f.as_str()),
                Err(e) => {
                    eprintln!("mqsc-lsp: cannot read {}: {e}", Path::new(f).display());
                    failed_io = true;
                    continue;
                }
            }
        };
        let mut out = String::new();
        if check_one(display, &text, settings, &mut out) {
            any_error = true;
        }
        print!("{out}");
    }
    if failed_io {
        2
    } else if any_error {
        1
    } else {
        0
    }
}

/// Build a settings value from repeated `KEY=SEV` overrides (keys are code
/// numbers or `diag_name`), matching the editor configuration shape.
pub fn settings_from_overrides(overrides: &[(String, String)]) -> Option<Value> {
    if overrides.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for (k, v) in overrides {
        map.insert(k.clone(), json!(v));
    }
    Some(json!({ "diagnostics": Value::Object(map) }))
}
