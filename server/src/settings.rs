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

// ---------------------------------------------------------------- tests (T-10)

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// mirrors analysis::diag_name(12) — the code constants are private to
    /// analysis; the settings layer addresses codes by number or by name
    const UNDEFINED_OBJECT: i32 = 12;

    fn diag(code: i32, sev: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            code: Some(NumberOrString::Number(code)),
            severity: Some(sev),
            ..Default::default()
        }
    }

    #[test]
    fn settings_map_extracted_at_both_levels() {
        let flat = json!({"diagnostics": {"12": "off"}});
        let nested = json!({"mqsc": {"diagnostics": {"12": "off"}}});
        assert!(diagnostic_settings(&flat).is_some());
        assert!(diagnostic_settings(&nested).is_some());
        assert!(diagnostic_settings(&json!({})).is_none());
        assert!(diagnostic_settings(&json!({"diagnostics": "nonsense"})).is_none());
    }

    #[test]
    fn off_by_number_and_by_name() {
        let mut d = vec![diag(UNDEFINED_OBJECT, DiagnosticSeverity::HINT)];
        apply_diagnostic_settings(&mut d, &json!({"12": "off"}));
        assert!(d.is_empty(), "numeric key off failed");

        let mut d = vec![diag(UNDEFINED_OBJECT, DiagnosticSeverity::HINT)];
        apply_diagnostic_settings(&mut d, &json!({"UNDEFINED_OBJECT": "off"}));
        assert!(d.is_empty(), "name key off failed");
    }

    #[test]
    fn severity_remap_and_aliases() {
        let mut d = vec![diag(UNDEFINED_OBJECT, DiagnosticSeverity::HINT)];
        apply_diagnostic_settings(&mut d, &json!({"UNDEFINED_OBJECT": "error"}));
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::ERROR));

        let mut d = vec![diag(UNDEFINED_OBJECT, DiagnosticSeverity::HINT)];
        apply_diagnostic_settings(&mut d, &json!({"UNDEFINED_OBJECT": "WARN"}));
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn unknown_keys_and_values_are_no_ops() {
        let mut d = vec![diag(UNDEFINED_OBJECT, DiagnosticSeverity::HINT)];
        apply_diagnostic_settings(&mut d, &json!({"NOT_A_CODE": "off"}));
        assert_eq!(d.len(), 1);
        apply_diagnostic_settings(&mut d, &json!({"12": "purple"}));
        assert_eq!(d[0].severity, Some(DiagnosticSeverity::HINT), "bad severity must not mutate");
    }

    #[test]
    fn non_numeric_code_diagnostics_are_left_alone() {
        let mut d = vec![Diagnostic {
            code: Some(NumberOrString::String("other".into())),
            ..Default::default()
        }];
        apply_diagnostic_settings(&mut d, &json!({"12": "off"}));
        assert_eq!(d.len(), 1);
    }
}
