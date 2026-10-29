//! MQSC command model, extracted from the IBM "Script (MQSC) Command
//! Reference" by tools/extract_model.py.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;

const MODEL_JSON: &str = include_str!("../../model/mqsc-model.json");

/// Curated modern (MQ 7.1+–9.x) additions merged over the 2011-reference
/// extraction at load time — see model/mqsc-overlay.json and README
/// "Model gaps". The base model JSON stays the pure, reproducible extractor
/// output; everything curated lives here.
const OVERLAY_JSON: &str = include_str!("../../model/mqsc-overlay.json");

#[derive(Debug, Deserialize)]
pub struct RawParam {
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub has_default: bool,
    #[serde(default)]
    pub footnotes: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct CommandEntry {
    pub verb: String,
    pub object: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub synonym: Vec<String>,
    #[serde(default)]
    pub params: Vec<RawParam>,
    #[serde(default)]
    pub attrs: Vec<String>,
    #[serde(default)]
    pub positional: Option<String>,
    #[serde(default)]
    pub footnotes: Vec<String>,
}

/// One `when` clause: the rule applies only when `param` was supplied with
/// one of `values` (any value if the list is empty), and not with one of
/// `unless_values`. When the parameter is absent, the rule applies iff
/// `values` is empty (a value-gated rule needs the value to fire).
#[derive(Debug, Clone, Deserialize)]
pub struct MandatoryWhen {
    pub param: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub unless_values: Vec<String>,
}

/// One mandatory-property rule for a `DEFINE <object>` command: at least one
/// parameter of `any` must be present (`why` and `example` word the
/// diagnostic). Curated data — the 2011 railroad diagrams bracket every
/// parameter, so the model's `optional` flag cannot express MQ's "this
/// parameter is required" prose. Drives diagnostic 14 MISSING_MANDATORY.
#[derive(Debug, Clone, Deserialize)]
pub struct MandatoryRule {
    pub any: Vec<String>,
    #[serde(default)]
    pub when: Option<MandatoryWhen>,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub example: String,
}

/// One curated availability rule for a parameter (overlay `availability`
/// section). Like `name_max`/`mandatory` this is a documented heuristic:
/// only parameters whose version/platform availability is known with
/// confidence are listed — everything else is never flagged. `obj` limits
/// the rule to certain canonical object types (empty = every command that
/// has the parameter); `platform` is `"zos"` or `"distributed"`; `since`/
/// `until` are `"major.minor"` MQ versions. Rules only ever apply when the
/// user configured a target (see `settings::Target`) — with no target set
/// they change nothing. Drives diagnostic 15 NOT_IN_TARGET.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailRule {
    pub param: String,
    #[serde(default)]
    pub obj: Vec<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    commands: HashMap<String, HashMap<String, CommandEntry>>,
    #[serde(default)]
    param_docs: HashMap<String, Vec<(String, String)>>,
    /// curated object type -> maximum positional name length in chars
    /// (overlay-only data; the base model JSON never carries this key)
    #[serde(default)]
    name_max: HashMap<String, usize>,
    /// curated required parameters per DEFINE object type (overlay-only data
    /// like name_max; see MandatoryRule)
    #[serde(default)]
    mandatory: HashMap<String, Vec<MandatoryRule>>,
    /// curated parameter availability (overlay-only data; see AvailRule)
    #[serde(default)]
    availability: Vec<AvailRule>,
}

/// One `"VERB OBJECT"` patch in the overlay; every operation optional.
#[derive(Debug, Deserialize, Default)]
struct CommandPatch {
    #[serde(default)]
    add_params: Vec<RawParam>,
    #[serde(default)]
    replace_params: Option<Vec<RawParam>>,
    /// parameter name -> values to union into its enum
    #[serde(default)]
    extend_enum: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Overlay {
    #[serde(default)]
    commands: HashMap<String, HashMap<String, CommandEntry>>,
    #[serde(default)]
    patch: HashMap<String, CommandPatch>,
    #[serde(default)]
    param_docs: HashMap<String, Vec<(String, String)>>,
    /// curated object type -> max positional name length (chars)
    #[serde(default)]
    name_max: HashMap<String, usize>,
    /// curated mandatory-property rules per DEFINE object type
    #[serde(default)]
    mandatory: HashMap<String, Vec<MandatoryRule>>,
    /// curated parameter availability rules (see AvailRule)
    #[serde(default)]
    availability: Vec<AvailRule>,
}

impl RawModel {
    /// Merge the curated modern overlay into the extracted 2011 model.
    /// New commands are inserted whole; patches adjust existing commands.
    fn apply_overlay(&mut self, ov: Overlay) {
        for (verb, objs) in ov.commands {
            for (obj, entry) in objs {
                self.commands
                    .entry(verb.clone())
                    .or_default()
                    .insert(obj, entry);
            }
        }
        for (key, p) in ov.patch {
            let Some((verb, obj)) = key.split_once(' ') else {
                eprintln!("mqsc-model overlay: patch key {key:?} is not 'VERB OBJECT', skipped");
                continue;
            };
            let Some(entry) = self.commands.get_mut(verb).and_then(|o| o.get_mut(obj)) else {
                eprintln!("mqsc-model overlay: patch target {key:?} not in base model, skipped");
                continue;
            };
            if let Some(list) = p.replace_params {
                entry.params = list;
            }
            for np in p.add_params {
                match entry.params.iter_mut().find(|e| e.name == np.name) {
                    Some(slot) => *slot = np,
                    None => entry.params.push(np),
                }
            }
            for (pname, vals) in p.extend_enum {
                let Some(slot) = entry.params.iter_mut().find(|e| e.name == pname) else {
                    continue;
                };
                for v in vals {
                    if !slot.values.contains(&v) {
                        slot.values.push(v);
                    }
                }
                if slot.kind != "enum" {
                    slot.kind = "enum".into();
                }
            }
        }
        for (name, docs) in ov.param_docs {
            self.param_docs.entry(name).or_default().extend(docs);
        }
        // curated name-length table: the overlay is the single source
        self.name_max.extend(ov.name_max);
        // curated mandatory-property rules: likewise overlay-only
        self.mandatory.extend(ov.mandatory);
        // curated availability rules: overlay-only, appended (base carries none)
        self.availability.extend(ov.availability);
    }
}

/// A value category for a parameter, after normalisation.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueKind {
    /// Bare keyword, no value group (REPLACE, NOEORMDB...)
    Flag,
    /// Enumerated keywords, stored uppercase
    Enum(Vec<String>),
    Integer,
    Str,
    /// An object/name token (q-local name, cluster name...)
    Name,
    /// '*' (and sometimes a name)
    Star,
    /// Anything goes
    Free,
}

#[derive(Debug)]
pub struct Param {
    pub name: String,
    pub kind: ValueKind,
    /// parameter also accepts integer even when primary kind is enum/star
    pub alt_integer: bool,
    pub default: Option<String>,
    pub optional: bool,
    pub has_default: bool,
    pub footnotes: Vec<String>,
    pub description: String,
}

#[derive(Debug)]
pub struct Command {
    pub verb: &'static str,
    pub object: &'static str,
    pub title: String,
    pub description: String,
    pub synonyms: Vec<String>,
    pub params: Vec<Param>,
    pub attrs: Vec<String>,
    pub positional: Option<String>,
    pub footnotes: Vec<String>,
    /// parameter name -> index into params
    pub by_name: HashMap<String, usize>,
    /// set of bare words valid positionally (attrs for DISPLAY/RESET)
    pub attr_set: HashSet<String>,
}

static VERBS: &[&str] = &[
    "ADD", "ALTER", "ARCHIVE", "BACKUP", "CLEAR", "DEFINE", "DELETE", "DISPLAY", "MODIFY", "MOVE",
    "PING", "PURGE", "RECOVER", "REFRESH", "RESET", "RESOLVE", "RESUME", "RVERIFY", "SET", "START",
    "STOP", "SUSPEND",
];

/// Global, loaded model.
pub struct Model {
    /// verb -> object -> command
    pub commands: HashMap<&'static str, HashMap<String, usize>>,
    pub entries: Vec<Box<Command>>,
    /// verb synonyms -> canonical verb (e.g. DEF -> DEFINE)
    verb_syn: HashMap<String, &'static str>,
    /// (verb, object synonym) -> canonical object
    obj_syn: HashMap<(&'static str, String), String>,
    /// object type synonyms valid for any verb that has that object
    global_obj_syn: HashMap<String, String>,
    /// all known parameter names across the model (for heuristics)
    pub known_params: HashSet<String>,
    /// all known attrs across the model
    pub known_attrs: HashSet<String>,
    pub verbs: &'static [&'static str],
    param_docs: HashMap<String, Vec<(String, String)>>,
    /// curated object type -> maximum positional name length in chars
    /// (from the overlay's `name_max` section; absent = limit unknown,
    /// never flagged). Drives diagnostic 13 NAME_TOO_LONG.
    name_max: HashMap<String, usize>,
    /// curated required parameters per DEFINE object type (from the
    /// overlay's `mandatory` section). Drives diagnostic 14
    /// MISSING_MANDATORY.
    mandatory: HashMap<String, Vec<MandatoryRule>>,
    /// curated parameter availability (from the overlay's `availability`
    /// section). Drives diagnostic 15 NOT_IN_TARGET; only consulted when a
    /// target version/platform is configured.
    availability: Vec<AvailRule>,
}

fn norm_syn(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

fn kind_of(p: &RawParam) -> (ValueKind, bool) {
    let mut alt_integer = false;
    let kind = match p.kind.as_str() {
        "integer" => ValueKind::Integer,
        "string" => ValueKind::Str,
        "name" => ValueKind::Name,
        "star" => ValueKind::Star,
        "flag" => {
            if !p.values.is_empty() {
                ValueKind::Enum(p.values.clone())
            } else {
                ValueKind::Flag
            }
        }
        "enum" => {
            let mut vals: Vec<String> = Vec::new();
            let mut saw_base = None;
            for v in &p.values {
                match v.as_str() {
                    "integer" => alt_integer = true,
                    "string" | "name" => saw_base = Some(ValueKind::Str),
                    _ => vals.push(norm_syn(v)),
                }
            }
            if vals.is_empty() {
                saw_base.unwrap_or(ValueKind::Str)
            } else {
                ValueKind::Enum(vals)
            }
        }
        _ => ValueKind::Free,
    };
    (kind, alt_integer)
}

impl Model {
    pub fn load() -> &'static Model {
        static MODEL: OnceLock<Model> = OnceLock::new();
        MODEL.get_or_init(|| {
            let mut raw: RawModel =
                serde_json::from_str(MODEL_JSON).expect("embedded mqsc-model.json must be valid");
            let overlay: Overlay = serde_json::from_str(OVERLAY_JSON)
                .expect("embedded mqsc-overlay.json must be valid");
            raw.apply_overlay(overlay);

            let mut entries: Vec<Box<Command>> = Vec::new();
            let mut commands: HashMap<&'static str, HashMap<String, usize>> = HashMap::new();
            let mut verb_syn: HashMap<String, &'static str> = HashMap::new();
            let mut obj_syn: HashMap<(&'static str, String), String> = HashMap::new();
            let mut global_obj_syn: HashMap<String, String> = HashMap::new();
            let mut known_params: HashSet<String> = HashSet::new();
            let mut known_attrs: HashSet<String> = HashSet::new();

            // stable verb ordering for completion
            for (verb_s, objs) in raw.commands {
                let verb: &'static str = match VERBS.iter().copied().find(|v| *v == verb_s.as_str())
                {
                    Some(v) => v,
                    None => {
                        // A command under a verb VERBS lacks would vanish
                        // without a trace (this is how PURGE nearly got
                        // lost): say so, like the patch-target warning.
                        eprintln!("mqsc-model: verb {verb_s:?} not in VERBS, command dropped");
                        continue;
                    }
                };
                let slot = commands.entry(verb).or_default();
                for (obj, e) in objs {
                    let mut params = Vec::with_capacity(e.params.len());
                    let mut by_name = HashMap::new();
                    for rp in &e.params {
                        let (kind, alt_integer) = kind_of(rp);
                        known_params.insert(rp.name.clone());
                        by_name.insert(rp.name.clone(), params.len());
                        params.push(Param {
                            name: rp.name.clone(),
                            kind,
                            alt_integer,
                            default: rp.default.clone(),
                            optional: rp.optional,
                            has_default: rp.has_default,
                            footnotes: rp.footnotes.clone(),
                            description: rp.description.clone(),
                        });
                    }
                    let attr_set: HashSet<String> = e.attrs.iter().cloned().collect();
                    known_attrs.extend(e.attrs.iter().cloned());

                    // synonyms: first token is the verb abbreviation, the
                    // rest the object abbreviation ("DEF QL" => DEF->DEFINE,
                    // (DEFINE,QL)->QLOCAL)
                    let mut syns: Vec<String> = e.synonym.iter().map(|s| norm_syn(s)).collect();
                    if let Some(vs) = syns.first() {
                        if vs != verb {
                            verb_syn.insert(vs.clone(), verb);
                        }
                    }
                    if syns.len() > 1 {
                        let os = &syns[1];
                        if os != &obj {
                            obj_syn.insert((verb, os.clone()), obj.clone());
                            global_obj_syn
                                .entry(os.clone())
                                .or_insert_with(|| obj.clone());
                        }
                    }

                    let idx = entries.len();
                    entries.push(Box::new(Command {
                        verb,
                        object: &*Box::leak(obj.clone().into_boxed_str()),
                        title: e.title.clone(),
                        description: e.description.clone(),
                        synonyms: std::mem::take(&mut syns),
                        params,
                        attrs: e.attrs.clone(),
                        positional: e.positional.clone(),
                        footnotes: e.footnotes.clone(),
                        by_name,
                        attr_set,
                    }));
                    slot.insert(obj, idx);
                }
            }

            // Manual MQSC object-type aliases that the reference documents
            // only in prose notes:
            //  DISPLAY QLOCAL/QMODEL/... == DISPLAY QUEUE TYPE(...)
            if let Some(q) = commands
                .get("DISPLAY")
                .and_then(|m| m.get("QUEUE"))
                .copied()
            {
                for alias in ["QLOCAL", "QMODEL", "QALIAS", "QREMOTE", "QCLUSTER", "Q"] {
                    commands
                        .get_mut("DISPLAY")
                        .unwrap()
                        .entry(alias.to_string())
                        .or_insert(q);
                    global_obj_syn
                        .entry(alias.to_string())
                        .or_insert_with(|| "QUEUE".to_string());
                }
            }

            Model {
                commands,
                entries,
                verb_syn,
                obj_syn,
                global_obj_syn,
                known_params,
                known_attrs,
                verbs: VERBS,
                param_docs: raw.param_docs,
                name_max: raw.name_max,
                mandatory: raw.mandatory,
                availability: raw.availability,
            }
        })
    }

    pub fn entry(&self, i: usize) -> &Command {
        &self.entries[i]
    }

    /// Resolve a (verb, object) pair the user typed (any case) to a command.
    pub fn resolve(&self, verb: &str, object: Option<&str>) -> Option<usize> {
        let canon = self.canon_verb(verb)?;
        let objs = self.commands.get(canon)?;
        let obj = object.map(norm_syn);
        match &obj {
            None => None,
            Some(o) => {
                if let Some(&i) = objs.get(o) {
                    return Some(i);
                }
                if let Some(i) = self
                    .obj_syn
                    .get(&(canon, o.clone()))
                    .and_then(|real| objs.get(real.as_str()).copied())
                {
                    return Some(i);
                }
                if let Some(real) = self.global_obj_syn.get(o) {
                    if let Some(&i) = objs.get(real) {
                        return Some(i);
                    }
                }
                None
            }
        }
    }

    pub fn canon_verb(&self, verb: &str) -> Option<&'static str> {
        let up = norm_syn(verb);
        if let Some(v) = VERBS.iter().copied().find(|v| *v == up.as_str()) {
            return Some(v);
        }
        self.verb_syn.get(&up).copied()
    }

    pub fn objects_for(&self, verb: &str) -> Vec<&str> {
        let Some(canon) = self.canon_verb(verb) else {
            return Vec::new();
        };
        let mut out: Vec<&str> = self
            .commands
            .get(canon)
            .map(|m| m.keys().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Curated maximum positional-name length (characters) for an object
    /// type; `None` = no limit known for that type (never flagged). The
    /// table is the overlay's `name_max` section — a curated heuristic,
    /// the 2011 reference records name limits only in prose the extractor
    /// cannot capture. Backs diagnostic 13 NAME_TOO_LONG.
    pub fn name_max_len(&self, object: &str) -> Option<usize> {
        self.name_max.get(&norm_syn(object)).copied()
    }

    /// Curated mandatory-property rules for `DEFINE <object>` (empty for
    /// object types with no curated requirements, and never applied to other
    /// verbs: ALTER/DELETE act on objects that already carry these
    /// properties). The table is the overlay's `mandatory` section — a
    /// documented heuristic like `name_max`: the 2011 railroad diagrams
    /// bracket every parameter, so MQ's "this parameter is required" prose
    /// never reaches the model's `optional` flag. Backs diagnostic 14
    /// MISSING_MANDATORY.
    pub fn mandatory_rules(&self, object: &str) -> &[MandatoryRule] {
        self.mandatory
            .get(&norm_syn(object))
            .map_or(&[], Vec::as_slice)
    }

    /// Curated availability rule for `param` as used on `object` (an
    /// object-restricted rule wins over a general one for the parameter).
    /// Overlay data only; absence means availability is unknown = never
    /// flagged.
    pub fn availability_for(&self, param: &str, object: &str) -> Option<&AvailRule> {
        let obj = norm_syn(object);
        let param = param.to_ascii_uppercase();
        self.availability
            .iter()
            .find(|r| r.param == param && r.obj.iter().any(|o| norm_syn(o) == obj))
            .or_else(|| {
                self.availability
                    .iter()
                    .find(|r| r.param == param && r.obj.is_empty())
            })
    }

    /// documentation text for a parameter name, preferring contexts that
    /// mention `object`.
    pub fn param_doc(&self, name: &str, object: &str) -> Option<&str> {
        for cand in [name.to_string(), strip_no(name)] {
            let Some(docs) = self.param_docs.get(&cand) else {
                continue;
            };
            let mut any = None;
            for (ctx, d) in docs {
                if d.is_empty() {
                    continue;
                }
                if ctx.contains(object) {
                    return Some(d.as_str());
                }
                if any.is_none() {
                    any = Some(d.as_str());
                }
            }
            if any.is_some() {
                return any;
            }
        }
        None
    }

    /// All commands whose verb resolves from `verb_syn`, for completion.
    pub fn all_verbs(&self) -> Vec<&'static str> {
        VERBS.to_vec()
    }
}

impl Clone for Param {
    fn clone(&self) -> Self {
        Param {
            name: self.name.clone(),
            kind: self.kind.clone(),
            alt_integer: self.alt_integer,
            default: self.default.clone(),
            optional: self.optional,
            has_default: self.has_default,
            footnotes: self.footnotes.clone(),
            description: self.description.clone(),
        }
    }
}

fn strip_no(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("NOT") {
        format!("T{rest}")
    } else if let Some(rest) = name.strip_prefix("NO") {
        rest.to_string()
    } else {
        name.to_string()
    }
}

/// small Levenshtein for suggestion lists (words are short)
pub fn distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub fn suggest<'a, I: IntoIterator<Item = &'a str>>(
    word: &str,
    pool: I,
    max: usize,
) -> Vec<&'a str> {
    let w = word.to_ascii_uppercase();
    let thr = 2.max(w.len() / 3);
    let mut hits: Vec<(usize, &str)> = pool
        .into_iter()
        // edit distance is at least the length difference: reject hopeless
        // candidates in O(1) so large candidate pools stay cheap
        .filter(|c| c.len().abs_diff(w.len()) <= thr)
        .map(|c| (distance(&w, c), c))
        .filter(|(d, _)| *d <= thr)
        .collect();
    hits.sort_by_key(|(d, c)| (*d, *c));
    hits.truncate(max);
    hits.into_iter().map(|(_, c)| c).collect()
}

// ---------------------------------------------------------------- tests (T-10)

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> &'static Model {
        Model::load()
    }

    fn param(verb: &str, object: &str, name: &str) -> &'static Param {
        let m = model();
        let idx = m
            .resolve(verb, Some(object))
            .unwrap_or_else(|| panic!("{verb} {object} not in model"));
        m.entry(idx)
            .params
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{verb} {object} has no param {name}"))
    }

    fn enum_values(p: &Param) -> Vec<String> {
        match &p.kind {
            ValueKind::Enum(v) => v.clone(),
            other => panic!("{} is {:?}, not Enum", p.name, other),
        }
    }

    // ------------------------------------------------------------- suggestions

    #[test]
    fn distance_is_levenshtein() {
        assert_eq!(distance("MAXMSGL", "MAXMSGS"), 1);
        assert_eq!(distance("A", "ABC"), 2);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "xy"), 2);
        assert_eq!(distance("SAME", "SAME"), 0);
    }

    #[test]
    fn suggest_picks_near_words_case_insensitively() {
        let pool = ["MAXMSGL", "MAXDEPTH", "DESCR"];
        assert_eq!(suggest("MAXMSGS", pool, 3), vec!["MAXMSGL"]);
        assert_eq!(suggest("maxmsgs", pool, 3), vec!["MAXMSGL"]);
        // far-away words are not suggested (threshold = max(2, len/3))
        assert!(suggest("ZZZZZZZ", pool, 3).is_empty());
        // near miss survives, other candidates are out of range
        assert_eq!(
            suggest("MAXDEPT", ["MAXDEPTH", "MAXMSGL", "MAXDATA"], 2),
            vec!["MAXDEPTH"]
        );
        // equal-distance hits come back alphabetical and respect the cap
        assert_eq!(
            suggest("MAXMSGX", ["MAXMSGL", "MAXMSGX1", "MAXDEPTH"], 2),
            vec!["MAXMSGL", "MAXMSGX1"]
        );
    }

    // ------------------------------------------------------------ model lookup

    #[test]
    fn verbs_and_objects_resolve() {
        let m = model();
        assert_eq!(m.canon_verb("define"), Some("DEFINE"));
        assert_eq!(m.canon_verb("Define"), Some("DEFINE"));
        assert_eq!(m.canon_verb("FROBNIC"), None);
        assert!(m.resolve("DEFINE", Some("QLOCAL")).is_some());
        assert!(m.resolve("DEFINE", Some("NOPE")).is_none());
        assert!(m.objects_for("DEFINE").contains(&"QLOCAL"));
    }

    // --------------------------------------- curated model invariants (T-1..T-9)

    #[test]
    fn overlay_custom_field_is_permissive_string() {
        // z/OS dumps store CUSTOM('DeployID(123)'); integer kind would warn
        assert_eq!(param("DEFINE", "QLOCAL", "CUSTOM").kind, ValueKind::Str);
    }

    #[test]
    fn overlay_set_authrec_objtype_has_modern_values() {
        let v = enum_values(param("SET", "AUTHREC", "OBJTYPE"));
        // QUEUE is from the base; CLNTCONN/COMMINFO/RQMNAME were added by the
        // overlay so SET AUTHREC OBJTYPE(...) (the corpus value set) validates
        for want in ["QUEUE", "CLNTCONN", "COMMINFO", "RQMNAME"] {
            assert!(v.iter().any(|x| x == want), "OBJTYPE missing {want}: {v:?}");
        }
    }

    #[test]
    fn overlay_setchlauth_has_warn_and_chckclnt() {
        let m = model();
        let idx = m.resolve("SET", Some("CHLAUTH")).unwrap();
        let names: Vec<_> = m
            .entry(idx)
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(names.contains(&"WARN"), "SET CHLAUTH params: {names:?}");
        assert!(names.contains(&"CHCKCLNT"));
    }

    #[test]
    fn every_overlay_verb_is_in_the_static_verbs_list() {
        // A command under a verb VERBS lacks is dropped at load (MODIFY
        // CHLAUTH lived silently dead that way once; PURGE nearly joined
        // it). Pin the contract the load-time warning can only report.
        let ov: serde_json::Value = serde_json::from_str(OVERLAY_JSON).expect("overlay must parse");
        let mut verbs: Vec<&str> = Vec::new();
        if let Some(objs) = ov["commands"].as_object() {
            verbs.extend(objs.keys().map(|k| k.as_str()));
        }
        if let Some(keys) = ov["patch"].as_object() {
            verbs.extend(
                keys.keys()
                    .filter_map(|k| k.split_once(' ').map(|(v, _)| v)),
            );
        }
        assert!(!verbs.is_empty());
        for v in verbs {
            assert!(VERBS.contains(&v), "overlay verb {v:?} missing from VERBS");
        }
    }

    #[test]
    fn oracle_added_commands_resolve() {
        // the campaign-B4 commands (live 10.0.1 railroads)
        for (verb, obj) in [
            ("DISPLAY", "TCLUSTER"),
            ("PURGE", "CHANNEL"),
            ("SET", "POLICY"),
            ("DELETE", "POLICY"),
            ("DISPLAY", "POLICY"),
            ("DELETE", "COMMINFO"),
            ("DELETE", "AUTHREC"),
            ("DISPLAY", "AUTHREC"),
            ("DISPLAY", "AUTHSERV"),
            ("DISPLAY", "ENTAUTH"),
            ("DISPLAY", "APSTATUS"),
            ("MODIFY", "CHLAUTH"),
        ] {
            assert!(model().resolve(verb, Some(obj)).is_some(), "{verb} {obj}");
        }
        // KEYREUSE( DISABLED | 1-9999999 | UNLIMITED ) keeps the model's
        // integer-alternative encoding for the range form
        let kr = param("SET", "POLICY", "KEYREUSE");
        assert!(kr.alt_integer, "KEYREUSE lost its integer range");
    }

    #[test]
    fn rebuilt_define_sub_has_modern_parameters() {
        assert!(param("DEFINE", "SUB", "TOPICSTR").kind == ValueKind::Str);
        let e = enum_values(param("DEFINE", "SUB", "EXPIRY"));
        assert!(e.contains(&"UNLIMITED".to_string()), "EXPIRY: {e:?}");
    }

    #[test]
    fn extracted_integer_alternatives_are_marked() {
        // T-3 (railroad) and T-7 (merge) fixes, directly from the base model
        let appltype = param("DEFINE", "PROCESS", "APPLTYPE");
        assert!(
            appltype.alt_integer,
            "APPLTYPE lost its integer alternative"
        );
        assert!(enum_values(appltype).contains(&"CICS".to_string()));
        assert!(
            param("DEFINE", "TOPIC", "DEFPRTY").alt_integer,
            "DEFPRTY lost its integer alternative"
        );
        assert!(param("ALTER", "TRACE", "TNO").alt_integer);
    }

    #[test]
    fn enum_extensions_present() {
        let propctl = enum_values(param("DEFINE", "QLOCAL", "PROPCTL"));
        assert!(
            propctl.contains(&"V6COMPAT".to_string()),
            "PROPCTL: {propctl:?}"
        );
        let acctq = enum_values(param("DEFINE", "QMODEL", "ACCTQ"));
        assert!(
            acctq.contains(&"OFF".to_string()) && acctq.contains(&"QMGR".to_string()),
            "ACCTQ: {acctq:?}"
        );
    }

    #[test]
    fn model_loaded_nontrivially() {
        let m = model();
        assert!(m.all_verbs().contains(&"DEFINE"));
        assert!(m.all_verbs().len() >= 15);
    }

    #[test]
    fn overlay_name_max_table_loads() {
        let m = model();
        assert_eq!(m.name_max_len("QLOCAL"), Some(48));
        assert_eq!(m.name_max_len("channel"), Some(1024)); // normalized
        assert_eq!(m.name_max_len("GROUP"), None); // deliberately absent
        assert_eq!(m.name_max_len(""), None);
    }

    #[test]
    fn overlay_mandatory_table_loads() {
        let m = model();
        let qalias = m.mandatory_rules("qalias"); // normalized lookup
        assert_eq!(qalias.len(), 1, "{qalias:?}");
        assert!(qalias[0].any.contains(&"TARGET".to_string()));
        assert!(qalias[0].any.contains(&"LIKE".to_string()));
        assert!(qalias[0].when.is_none());
        // object types with no curated requirement return nothing
        assert!(m.mandatory_rules("QLOCAL").is_empty());
        assert!(m.mandatory_rules("").is_empty());
        // CHANNEL: CHLTYPE plus one value-gated XMITQ rule
        let chan = m.mandatory_rules("CHANNEL");
        assert_eq!(chan.len(), 2, "{chan:?}");
        assert_eq!(chan[0].any, vec!["CHLTYPE".to_string()]);
        let w = chan[1]
            .when
            .as_ref()
            .expect("XMITQ rule must be CHLTYPE-gated");
        assert_eq!(w.param, "CHLTYPE");
        assert!(w.values.contains(&"SDR".to_string()));
        assert!(w.unless_values.is_empty());
        // SUB's DEST rule is exclusion-gated (DESTCLAS(MANAGED) exempts it)
        let sub = m.mandatory_rules("SUB");
        assert_eq!(sub.len(), 2, "{sub:?}");
        let w = sub[1].when.as_ref().expect("DEST rule gated on DESTCLAS");
        assert_eq!(w.param, "DESTCLAS");
        assert!(w.values.is_empty());
        assert!(w.unless_values.contains(&"MANAGED".to_string()));
    }
}
