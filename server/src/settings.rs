//! Shared per-code diagnostic severity settings, used by both the LSP
//! server (`workspace/didChangeConfiguration` / `initializationOptions`) and
//! the offline `--check` CLI so the two agree on configuration shape.
//!
//! Accepted shape (see README "Configuration"); either nesting level works:
//!
//! ```json
//! {"diagnostics": {"UNKNOWN_PARAM": "warning", "12": "off"}}
//! {"mqsc": {"diagnostics": {"UNDEFINED_OBJECT": "off"}}}
//! ```
//!
//! Keys are a diagnostic code number or its `diag_name`; values are
//! `error|warning|information|hint|off`.

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
use serde_json::Value;

use crate::analysis::diag_name;

/// Extract the `diagnostics` map from a settings value (either root or nested
/// under an `mqsc` section). Returns `None` when absent or not an object.
pub fn diagnostic_settings(root: &Value) -> Option<&Value> {
    let map = root
        .get("mqsc")
        .and_then(|m| m.get("diagnostics"))
        .or_else(|| root.get("diagnostics"))?;
    if map.is_object() {
        Some(map)
    } else {
        None
    }
}

/// Parse a severity keyword. Returns `None` for unrecognized strings so the
/// caller keeps the diagnostic's natural severity.
fn severity_of(s: &str) -> Option<Option<DiagnosticSeverity>> {
    Some(Some(match s.to_ascii_lowercase().as_str() {
        "off" | "none" | "false" => return Some(None), // drop
        "error" => DiagnosticSeverity::ERROR,
        "warning" | "warn" => DiagnosticSeverity::WARNING,
        "information" | "info" => DiagnosticSeverity::INFORMATION,
        "hint" => DiagnosticSeverity::HINT,
        _ => return None,
    }))
}

/// Apply `map` (`{"CODE_OR_NAME": "severity"}`) to diagnostics in place:
/// a recognized severity string replaces the severity; `"off"` drops it.
pub fn apply_diagnostic_settings(diags: &mut Vec<Diagnostic>, map: &Value) {
    diags.retain_mut(|d| {
        let code = match d.code {
            Some(NumberOrString::Number(c)) => c,
            _ => return true,
        };
        let val = match map.get(diag_name(code)).or_else(|| map.get(&code.to_string())) {
            Some(v) => v,
            None => return true,
        };
        match val.as_str().and_then(severity_of) {
            Some(sev) => {
                d.severity = sev;
                sev.is_some()
            }
            None => true, // unrecognized value: leave severity untouched
        }
    });
}
