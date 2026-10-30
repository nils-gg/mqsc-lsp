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
use crate::model::AvailRule;

// ------------------------------------------------- target version/platform (T-19)

/// MQ release used for availability comparisons, `"major.minor"` granularity
/// (a patch level in the input is read as its minor release).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MqVersion {
    pub major: u16,
    pub minor: u16,
}

impl MqVersion {
    /// Accepts `"9"`, `"9.4"`, `"9.4.1"` (patch ignored); anything else is
    /// not a version.
    pub fn parse(s: &str) -> Option<MqVersion> {
        let s = s.trim();
        let mut it = s.split('.');
        let major = it.next()?.trim().parse().ok()?;
        let minor = match it.next() {
            None => 0,
            Some(m) => m.trim().parse().ok()?,
        };
        // a third component may only be a patch number (9.4.1.0 style)
        for extra in it {
            if !extra.trim().chars().all(|c| c.is_ascii_digit()) || extra.trim().is_empty() {
                return None;
            }
        }
        Some(MqVersion { major, minor })
    }
}

impl std::fmt::Display for MqVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Platform the scripts are aimed at (config `mqsc.platform`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Distributed,
    Zos,
}

impl Platform {
    /// Accepts `"distributed"`, `"zos"`, `"z/os"`, `"z-os"` (case-insensitive).
    pub fn parse(s: &str) -> Option<Platform> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['/', '-'], "")
            .as_str()
        {
            "distributed" => Some(Platform::Distributed),
            "zos" => Some(Platform::Zos),
            _ => None,
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Distributed => write!(f, "distributed"),
            Platform::Zos => write!(f, "z/OS"),
        }
    }
}

/// What the user says the scripts target: `mqsc.targetVersion` and
/// `mqsc.platform`, both optional. With nothing configured NO availability
/// rule applies — the analyzer behaves exactly as before the feature.
#[derive(Clone, Copy, Default, Debug)]
pub struct Target {
    pub version: Option<MqVersion>,
    pub platform: Option<Platform>,
}

/// How the workspace scan of closed `*.mqsc` files behaves (Phase 3 /
/// PR 7). Read from `mqsc.workspaceIndex.*` (or the same keys at the top
/// level of a settings root, like every other shape this server accepts).
/// Defaults: scanning on, up to 2000 files, the usual generated/VCS
/// directories excluded. Bad values fall back per key — a broken setting
/// must never disable the feature entirely nor make it walk everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceScan {
    pub enabled: bool,
    pub max_files: usize,
    /// directory NAMES skipped at any depth (".git", "target", ...)
    pub exclude: Vec<String>,
}

/// Hard ceiling per file; MQSC scripts are configuration, not data
/// dumps, and one runaway generated file must not stall the scan.
pub const SCAN_MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

impl Default for WorkspaceScan {
    fn default() -> Self {
        WorkspaceScan {
            enabled: true,
            max_files: 2000,
            exclude: [".git", "target", "node_modules", "dist", "build"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl WorkspaceScan {
    /// Same lookup convention as `Target::from_settings`: nested under
    /// `mqsc` or flat at the root; unreadable keys keep the default.
    pub fn from_settings(root: &Value) -> WorkspaceScan {
        let section = root
            .get("mqsc")
            .and_then(|m| m.get("workspaceIndex"))
            .or_else(|| root.get("workspaceIndex"));
        let mut cfg = WorkspaceScan::default();
        let Some(sec) = section else {
            return cfg;
        };
        if let Some(b) = sec.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = b;
        }
        if let Some(n) = sec.get("maxFiles").and_then(|v| v.as_u64()) {
            cfg.max_files = n as usize;
        }
        if let Some(list) = sec.get("exclude").and_then(|v| v.as_array()) {
            let names: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            // an explicitly empty list means "exclude nothing", which is
            // different from not configuring the key at all
            cfg.exclude = names;
        }
        cfg
    }
}

/// Which files `definition` and `rename` may touch, read from
/// `mqsc.crossFileNavigation` (nested or flat, like every other shape).
/// **Default false**: navigation resolves a name inside the file being
/// edited only — go-to-definition jumps to DEFINEs in that document and a
/// rename edits that document alone. Set true to restore the workspace-
/// wide behavior (both reach every indexed document, scanned closed files
/// included). References, hover, completion and the cross-file code-12
/// hint are deliberately NOT affected: they answer "what exists in this
/// project", not "what should this edit touch". Read live per request
/// (like the severity settings), no restart contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Navigation {
    pub cross_file: bool,
}

impl Navigation {
    /// Same lookup convention as `WorkspaceScan::from_settings`: nested
    /// under `mqsc` or flat at the root; unreadable keys keep the default.
    pub fn from_settings(root: &Value) -> Navigation {
        Navigation {
            cross_file: root
                .get("mqsc")
                .and_then(|m| m.get("crossFileNavigation"))
                .or_else(|| root.get("crossFileNavigation"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }
}

/// How a parameter use falls outside the configured target.
#[derive(Debug, PartialEq, Eq)]
pub enum AvailViolation {
    /// target version is older than the rule's `since`
    TooOld(MqVersion),
    /// target version is newer than the rule's `until`
    TooNew(MqVersion),
    /// rule only exists on the other platform
    WrongPlatform(Platform),
}

impl Target {
    /// Is any part of the target configured? Unset targets never violate.
    pub fn configured(&self) -> bool {
        self.version.is_some() || self.platform.is_some()
    }

    /// Read `targetVersion`/`platform` from a settings root (either at the
    /// top level or nested under `mqsc`, like the diagnostics map). Values
    /// that do not parse are ignored, same as unrecognized severities.
    pub fn from_settings(root: &Value) -> Target {
        let get = |key: &str| {
            root.get("mqsc")
                .and_then(|m| m.get(key))
                .or_else(|| root.get(key))
                .and_then(|v| v.as_str())
        };
        Target {
            version: get("targetVersion").and_then(MqVersion::parse),
            platform: get("platform").and_then(Platform::parse),
        }
    }

    /// Does `rule` conflict with this target? Only an explicit mismatch is
    /// reported; unset target sides pass, and malformed rule data (a
    /// non-version `since`, an unknown platform word) is treated as unknown
    /// rather than flagged.
    pub fn gate(&self, rule: &AvailRule) -> Option<AvailViolation> {
        if !self.configured() {
            return None;
        }
        if let (Some(v), Some(since)) = (self.version, rule.since.as_deref()) {
            if let Some(since) = MqVersion::parse(since) {
                if v < since {
                    return Some(AvailViolation::TooOld(since));
                }
            }
        }
        if let (Some(v), Some(until)) = (self.version, rule.until.as_deref()) {
            if let Some(until) = MqVersion::parse(until) {
                if v > until {
                    return Some(AvailViolation::TooNew(until));
                }
            }
        }
        if let (Some(p), Some(rp)) = (self.platform, rule.platform.as_deref()) {
            if let Some(rp) = Platform::parse(rp) {
                if p != rp {
                    return Some(AvailViolation::WrongPlatform(rp));
                }
            }
        }
        None
    }
}

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
        let val = match map
            .get(diag_name(code))
            .or_else(|| map.get(code.to_string()))
        {
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

/// Validate a diagnostics settings map: returns human-readable problems —
/// keys that resolve to no known code, or values that are no severity. The
/// LSP has no stderr, so it keeps ignoring typos silently; the `--check` CLI
/// warns with these instead (REVIEW.md minor).
pub fn settings_warnings(map: &Value) -> Vec<String> {
    let Some(obj) = map.as_object() else {
        return Vec::new();
    };
    // The 15 CODE_* constants in analysis (1..=15); diag_name maps unknown
    // numbers to "OTHER".
    let names: Vec<&str> = (1..=15).map(diag_name).collect();
    let mut out = Vec::new();
    for (k, v) in obj {
        let known = match k.parse::<i32>() {
            Ok(n) => diag_name(n) != "OTHER",
            Err(_) => names.contains(&k.as_str()),
        };
        if !known {
            out.push(format!(
                "unknown diagnostic code '{k}' (not a number 1-15 or a diag name)"
            ));
        }
        match v.as_str() {
            Some(s) if severity_of(s).is_some() => {}
            Some(s) => out.push(format!(
                "'{k}': '{s}' is not a severity (error|warning|information|hint|off)"
            )),
            None => out.push(format!("'{k}': value must be a severity string")),
        }
    }
    out
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
        assert_eq!(
            d[0].severity,
            Some(DiagnosticSeverity::HINT),
            "bad severity must not mutate"
        );
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

    fn rule(
        param: &str,
        obj: &[&str],
        since: Option<&str>,
        until: Option<&str>,
        platform: Option<&str>,
    ) -> AvailRule {
        AvailRule {
            param: param.into(),
            obj: obj.iter().map(|s| s.to_string()).collect(),
            since: since.map(|s| s.to_string()),
            until: until.map(|s| s.to_string()),
            platform: platform.map(|s| s.to_string()),
            note: String::new(),
        }
    }

    #[test]
    fn version_parses_compares_and_displays() {
        let v = |s: &str| MqVersion::parse(s);
        assert_eq!(v("9.4"), Some(MqVersion { major: 9, minor: 4 }));
        assert_eq!(v("9"), Some(MqVersion { major: 9, minor: 0 }));
        assert_eq!(v(" 9.4.1 "), Some(MqVersion { major: 9, minor: 4 }));
        assert_eq!(v("9.x"), None);
        assert_eq!(v(""), None);
        assert!(v("9.0") < v("9.1"));
        assert!(v("8.9") < v("9.0"));
        assert_eq!(MqVersion { major: 9, minor: 1 }.to_string(), "9.1");
    }

    #[test]
    fn platform_accepts_spellings_and_displays_canonically() {
        assert_eq!(Platform::parse("distributed"), Some(Platform::Distributed));
        assert_eq!(Platform::parse("Z/OS"), Some(Platform::Zos));
        assert_eq!(Platform::parse("z-os"), Some(Platform::Zos));
        assert_eq!(Platform::parse("mainframe"), None);
        assert_eq!(Platform::Zos.to_string(), "z/OS");
        assert_eq!(Platform::Distributed.to_string(), "distributed");
    }

    #[test]
    fn target_read_from_both_levels_and_garbage_ignored() {
        let flat = json!({"targetVersion": "9.4", "platform": "zos"});
        let t = Target::from_settings(&flat);
        assert_eq!(
            (t.version, t.platform),
            (MqVersion::parse("9.4"), Platform::parse("zos"))
        );
        let nested = json!({"mqsc": {"targetVersion": "9.0", "diagnostics": {}}});
        let t = Target::from_settings(&nested);
        assert_eq!(t.version, MqVersion::parse("9.0"));
        assert_eq!(t.platform, None);
        let junk = json!({"mqsc": {"targetVersion": "banana", "platform": 7}});
        assert!(!Target::from_settings(&junk).configured());
        assert!(!Target::from_settings(&json!({})).configured());
    }

    #[test]
    fn gate_needs_a_target_and_reports_the_first_conflict() {
        let unset = Target::default();
        // purely synthetic rule data: this test pins the gate decision
        // table, not any real parameter's availability
        let r = rule("NEWP", &["QLOCAL"], Some("9.1"), None, Some("distributed"));
        assert_eq!(unset.gate(&r), None, "no target: rules dormant");

        let old = Target {
            version: MqVersion::parse("9.0"),
            platform: Platform::parse("distributed"),
        };
        assert_eq!(
            old.gate(&r),
            Some(AvailViolation::TooOld(MqVersion { major: 9, minor: 1 }))
        );

        let main = Target {
            version: MqVersion::parse("9.4"),
            platform: Platform::parse("zos"),
        };
        assert_eq!(
            main.gate(&r),
            Some(AvailViolation::WrongPlatform(Platform::Distributed))
        );

        let ok = Target {
            version: MqVersion::parse("9.4"),
            platform: Platform::parse("distributed"),
        };
        assert_eq!(ok.gate(&r), None);

        let gone = Target {
            version: MqVersion::parse("9.4"),
            platform: None,
        };
        let gone_r = rule("OLD", &[], None, Some("9.2"), None);
        assert_eq!(
            gone.gate(&gone_r),
            Some(AvailViolation::TooNew(MqVersion { major: 9, minor: 2 }))
        );

        // malformed rule data is unknown data, not a violation
        let junk_r = rule("X", &[], Some("nonsense"), None, Some("atari"));
        assert_eq!(old.gate(&junk_r), None);
    }

    #[test]
    fn workspace_scan_defaults_and_partial_overrides() {
        let d = WorkspaceScan::default();
        assert!(d.enabled && d.max_files == 2000 && d.exclude.contains(&".git".to_string()));
        // no section at all: defaults
        assert_eq!(WorkspaceScan::from_settings(&json!({})), d);
        // nested and flat shapes both work
        let nested = json!({"mqsc": {"workspaceIndex": {"enabled": false}}});
        assert!(!WorkspaceScan::from_settings(&nested).enabled);
        let flat = json!({"workspaceIndex": {"maxFiles": 50, "exclude": []}});
        let cfg = WorkspaceScan::from_settings(&flat);
        assert_eq!(cfg.max_files, 50);
        assert!(cfg.exclude.is_empty(), "explicit [] excludes nothing");
        // per-key fallback: junk values leave that key at its default
        let junk = json!({"workspaceIndex": {"enabled": "yes", "maxFiles": "many", "exclude": 7}});
        assert_eq!(WorkspaceScan::from_settings(&junk), d);
        // non-string array entries are dropped, the rest survive
        let mixed = json!({"workspaceIndex": {"exclude": [".svn", 1, "out"]}});
        assert_eq!(
            WorkspaceScan::from_settings(&mixed).exclude,
            vec![".svn".to_string(), "out".to_string()]
        );
    }

    #[test]
    fn navigation_scope_defaults_to_same_file() {
        // the default IS the feature: definition/rename stay in the edited file
        assert!(!Navigation::from_settings(&json!({})).cross_file);
        assert!(!Navigation::from_settings(&json!({"mqsc": {}})).cross_file);
        // an unreadable value keeps the default, like every other key here
        assert!(
            !Navigation::from_settings(&json!({"mqsc": {"crossFileNavigation": "yes"}})).cross_file
        );
        // true restores the old workspace-wide behavior, nested or flat
        assert!(
            Navigation::from_settings(&json!({"mqsc": {"crossFileNavigation": true}})).cross_file
        );
        assert!(Navigation::from_settings(&json!({"crossFileNavigation": true})).cross_file);
    }

    #[test]
    fn warnings_flag_typos_and_clean_maps_stay_quiet() {
        assert!(settings_warnings(&json!(
            {"UNDEFINED_OBJECT": "off", "12": "hint", "NAME_TOO_LONG": "information", "13": "error"}
        ))
        .is_empty());
        let w = settings_warnings(&json!({"BOGUS": "off", "12": "purple", "99": "off"}));
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w.iter().any(|x| x.contains("BOGUS")));
        assert!(w.iter().any(|x| x.contains("purple")));
        assert!(w.iter().any(|x| x.contains("99")));
    }
}
