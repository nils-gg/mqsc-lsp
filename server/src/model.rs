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

/// One curated availability rule for a whole command (overlay
/// `command_availability` section). Same conservative convention as
/// `AvailRule`: only commands whose platform/version history is known
/// with confidence are listed — the live-oracle differential
/// (`tools/mq_oracle_check.py`) is the evidence source for what the
/// distributed parser does not accept — everything else is never
/// flagged. Rules only apply when a target is configured (drives
/// diagnostic 15 NOT_IN_TARGET on the `VERB OBJECT` span).
#[derive(Debug, Clone, Deserialize)]
pub struct CmdAvailRule {
    /// `"VERB OBJECT"` as the model spells it after resolution
    pub command: String,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub note: String,
}

/// One curated value-meaning set for a parameter (overlay `value_docs`
/// section). Like the other curated tables only parameters whose value
/// meanings are documented with confidence are listed. `obj` limits the
/// rule to certain canonical object types and object-restricted rules
/// win over general ones — that scoping is what separates ACTION on
/// CHLAUTH from ACTION on CHANNEL, or TYPE on QUEUE from TYPE on
/// CHLAUTH. Purely informational: hover appends the meaning of the
/// hovered value; nothing here ever flags a diagnostic.
#[derive(Debug, Clone, Deserialize)]
pub struct ValueDoc {
    pub param: String,
    #[serde(default)]
    pub obj: Vec<String>,
    /// value spelling (uppercase) -> meaning shown in the hover card
    pub values: HashMap<String, String>,
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
    /// curated command availability (overlay-only data; see CmdAvailRule)
    #[serde(default)]
    command_availability: Vec<CmdAvailRule>,
    /// curated value meanings (overlay-only data; see ValueDoc)
    #[serde(default)]
    value_docs: Vec<ValueDoc>,
    /// curated meanings for shared value words (overlay-only data)
    #[serde(default)]
    generic_values: HashMap<String, String>,
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
    /// parameters the extractor pooled garbage into: the 2011 pages render
    /// a command's attribute dump as trailing "( NEXT ATTRS )" prose, and
    /// the extractor read that run-on list as the parameter's enum values.
    /// The live railroads show these words are bare attribute selectors, so
    /// the operation keeps the (real) description but forces kind=flag and
    /// drops the invented value list.
    #[serde(default)]
    flag_params: Vec<String>,
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
    /// curated command availability rules (see CmdAvailRule)
    #[serde(default)]
    command_availability: Vec<CmdAvailRule>,
    /// curated parameter value meanings (see ValueDoc)
    #[serde(default)]
    value_docs: Vec<ValueDoc>,
    /// curated meanings for value words shared across many parameters
    /// (see Model::generic_values)
    #[serde(default)]
    generic_values: HashMap<String, String>,
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
            // applied last: a demoted parameter must end kind=flag with the
            // pooled values gone (kind_of only honors "flag" when values are empty)
            for pname in p.flag_params {
                let Some(slot) = entry.params.iter_mut().find(|e| e.name == pname) else {
                    eprintln!("mqsc-model overlay: flag_params target {key} {pname} not in model, skipped");
                    continue;
                };
                slot.kind = "flag".into();
                slot.values.clear();
                slot.default = None;
                slot.has_default = false;
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
        // command-level rules likewise; a key that is not 'VERB OBJECT'
        // shape can never match, so report it instead of dying silently
        for r in ov.command_availability {
            match r.command.split_once(' ') {
                Some((v, o)) if !v.trim().is_empty() && !o.trim().is_empty() => {
                    self.command_availability.push(r)
                }
                _ => eprintln!(
                    "mqsc-model overlay: command availability key {:?} is not \
                     'VERB OBJECT', skipped",
                    r.command
                ),
            }
        }
        // curated value meanings: overlay-only, like the tables above
        self.value_docs.extend(ov.value_docs);
        self.generic_values.extend(ov.generic_values);
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
    /// curated command availability (from the overlay's
    /// `command_availability` section). Drives the same diagnostic 15 on
    /// the VERB OBJECT span; same settings gate.
    command_availability: Vec<CmdAvailRule>,
    /// curated parameter value meanings (from the overlay's `value_docs`
    /// section). Hover annotation only; never flags anything.
    value_docs: Vec<ValueDoc>,
    /// curated meanings for value words shared across many parameters
    /// (from the overlay's `generic_values` section). Hover fallback of the
    /// `value_docs` lookup; never flags anything.
    generic_values: HashMap<String, String>,
}

fn norm_syn(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

/// The bare keyword of a modelled display attribute. The reference spells
/// some attrs with their argument in the attrs list (`PORT ( port-number )`),
/// but every consumer — hover match, completion label, the "looks like a
/// continuation word" checks — sees only the bare word, so the loaded model
/// stores just that. Page-footer debris that the extractor picked up into
/// an attrs list (multi-word junk) normalises to nothing and is dropped.
fn norm_attr(a: &str) -> Option<&str> {
    let w = a.split('(').next().unwrap_or("").trim();
    (!w.is_empty() && !w.chars().any(|c| c.is_whitespace())).then_some(w)
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
                    // attrs normalise to bare words (see norm_attr); the set
                    // is built from the same pass, deduping what the
                    // normalisation makes identical
                    let mut attr_set: HashSet<String> = HashSet::new();
                    let attrs: Vec<String> = e
                        .attrs
                        .iter()
                        .filter_map(|a| norm_attr(a))
                        .map(|w| w.to_owned())
                        .filter(|w| attr_set.insert(w.clone()))
                        .collect();
                    known_attrs.extend(attrs.iter().cloned());

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
                        attrs,
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
                command_availability: raw.command_availability,
                value_docs: raw.value_docs,
                generic_values: raw.generic_values,
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

    /// Curated availability rule for the command `verb object` (spelled
    /// as the model resolves them). Overlay data only; absence means
    /// availability is unknown = never flagged.
    pub fn command_availability_for(&self, verb: &str, object: &str) -> Option<&CmdAvailRule> {
        let (verb, obj) = (norm_syn(verb), norm_syn(object));
        self.command_availability.iter().find(|r| {
            r.command
                .split_once(' ')
                .is_some_and(|(v, o)| norm_syn(v) == verb && norm_syn(o) == obj)
        })
    }

    /// Every curated command availability rule (coverage tests; the
    /// shipped table is expected to resolve in full).
    pub fn all_command_availability(&self) -> &[CmdAvailRule] {
        &self.command_availability
    }

    /// Curated meaning of `value` for `param` used on `object` (both
    /// spelled as the model resolves them; `value` must already be
    /// uppercased and quote-free). Object-restricted rules win over
    /// general ones; absence means the meaning is not curated.
    pub fn value_doc(&self, param: &str, object: &str, value: &str) -> Option<&str> {
        let obj = norm_syn(object);
        let param = param.to_ascii_uppercase();
        let value = value.to_ascii_uppercase();
        let find = |scoped: bool| {
            self.value_docs.iter().find(|r| {
                r.param == param
                    && if scoped {
                        r.obj.iter().any(|o| norm_syn(o) == obj)
                    } else {
                        r.obj.is_empty()
                    }
            })
        };
        find(true)
            .and_then(|r| r.values.get(&value))
            .or_else(|| find(false).and_then(|r| r.values.get(&value)))
            .map(|s| s.as_str())
    }

    /// Curated meaning of a GENERIC value word (overlay `generic_values`):
    /// the fallback for hover when no parameter-scoped `value_docs` rule
    /// explains the value. Purely informational, like the scoped table.
    pub fn generic_value_doc(&self, value: &str) -> Option<&str> {
        self.generic_values
            .get(&value.to_ascii_uppercase())
            .map(|s| s.as_str())
    }

    /// Every `(object, param)` pair the merged model carries (the
    /// value_docs coverage guard iterates the whole table through this).
    #[cfg(test)]
    fn scoped_params(&self, param: &str, obj: &[String]) -> Vec<(String, Param)> {
        let param = param.to_ascii_uppercase();
        let mut out = Vec::new();
        for objs in self.commands.values() {
            for (object, &i) in objs {
                if !obj.is_empty() && !obj.iter().any(|o| norm_syn(o) == *object) {
                    continue;
                }
                let e = self.entry(i);
                for p in &e.params {
                    if p.name == param {
                        out.push((object.clone(), p.clone()));
                    }
                }
            }
        }
        out
    }

    /// The curated `value_docs` table itself (coverage guard test input).
    pub fn all_value_docs(&self) -> &[ValueDoc] {
        &self.value_docs
    }

    /// The curated `generic_values` table itself (guard test input).
    #[cfg(test)]
    fn all_generic_values(&self) -> &HashMap<String, String> {
        &self.generic_values
    }

    /// Every enum value word the merged model accepts anywhere (the
    /// generic_values existence guard checks its words against this).
    #[cfg(test)]
    fn all_enum_words(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for objs in self.commands.values() {
            for &i in objs.values() {
                for p in &self.entry(i).params {
                    if let ValueKind::Enum(vs) = &p.kind {
                        out.extend(vs.iter().cloned());
                    }
                }
            }
        }
        out
    }

    /// Every `(object, attribute)` pair the merged model carries (the
    /// attribute-description coverage guard iterates through this).
    #[cfg(test)]
    fn all_scoped_attrs(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for objs in self.commands.values() {
            for (object, &i) in objs {
                for a in &self.entry(i).attrs {
                    out.push((object.clone(), a.clone()));
                }
            }
        }
        out
    }

    /// Every `(object, param)` pair the merged model carries (the
    /// parameter-description coverage guard iterates through this).
    #[cfg(test)]
    fn all_params(&self) -> Vec<(String, Param)> {
        let mut out = Vec::new();
        for objs in self.commands.values() {
            for (object, &i) in objs {
                for p in &self.entry(i).params {
                    out.push((object.clone(), p.clone()));
                }
            }
        }
        out
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
    fn oracle_enum_and_kind_fixes_are_merged() {
        // extraction artifacts corrected and oracle-proven values present
        let defpres = enum_values(param("ALTER", "QLOCAL", "DEFPRESP"));
        assert!(defpres.contains(&"SYNC".to_string()), "{defpres:?}");
        assert!(!defpres.iter().any(|v| v.starts_with('!')), "{defpres:?}");
        let dur = enum_values(param("DISPLAY", "SBSTATUS", "DURABLE"));
        assert!(dur.contains(&"ALL".to_string()), "{dur:?}");
        // 2011 special-value enums that are really cluster-name strings
        assert!(param("ALTER", "QALIAS", "CLUSNL").kind == ValueKind::Str);
        assert!(param("ALTER", "QLOCAL", "CLUSTER").kind == ValueKind::Str);
        // the '* ,' enum the extractor built for user-exit data is gone
        assert!(param("DEFINE", "CHANNEL", "MSGDATA").kind == ValueKind::Str);
        // oracle values added, z/OS extras kept
        assert!(enum_values(param("DEFINE", "QLOCAL", "DEFBIND")).contains(&"GROUP".to_string()));
        let pubprty = param("DEFINE", "SUB", "PUBPRTY");
        assert!(pubprty.alt_integer, "PUBPRTY lost its integer form");
        let pp = enum_values(pubprty);
        assert!(
            pp.contains(&"SYSTEM".to_string()) && pp.contains(&"ASPUB".to_string()),
            "{pp:?}"
        );
        // tightenings the 10.0.1 parser proves
        let aff = enum_values(param("ALTER", "CHANNEL", "AFFINITY"));
        assert!(
            aff.contains(&"NONE".to_string()) && aff.contains(&"PREFERRED".to_string()),
            "{aff:?}"
        );
        // SET CHLAUTH ACTION: the fabricated DELETE is gone, oracle values in
        let act = enum_values(param("SET", "CHLAUTH", "ACTION"));
        assert!(!act.contains(&"DELETE".to_string()), "{act:?}");
        assert!(act.contains(&"REMOVEALL".to_string()), "{act:?}");
    }

    #[test]
    fn ccsid_hover_text_and_upsert_keep_the_parameter_shape() {
        // the CCSID hover enrichment upserts ALTER QMGR CCSID; an upsert
        // that forgot kind or footnotes would silently downgrade it
        let p = param("ALTER", "QMGR", "CCSID");
        assert_eq!(p.kind, ValueKind::Integer, "CCSID lost its integer kind");
        assert!(
            p.footnotes.iter().any(|f| f.contains("AIX")),
            "CCSID lost its reference footnotes: {:?}",
            p.footnotes
        );
        assert!(
            p.description.contains("1208 = UTF-8"),
            "CCSID lost the code list"
        );
        // the 2011 reference text survives underneath it
        assert!(p.description.contains("Coded character set identifier"));
        assert!(param("DEFINE", "COMMINFO", "CCSID")
            .description
            .contains("1208 = UTF-8"));
        assert!(param("ALTER", "COMMINFO", "CCSID")
            .description
            .contains("1208 = UTF-8"));
    }

    #[test]
    fn command_availability_rules_load_and_look_up() {
        // the curated command table (overlay `command_availability`) loads
        // and answers case-insensitively on the canonical pair spelling
        let m = model();
        let r = m
            .command_availability_for("define", "CFSTRUCT")
            .expect("DEFINE CFSTRUCT rule");
        assert_eq!(r.platform.as_deref(), Some("zos"));
        assert!(m.command_availability_for("MOVE", "QLOCAL").is_some());
        // version-floor rules carry since and NO platform (the command
        // exists on both platforms from MQ 9.0 up)
        let ch = m
            .command_availability_for("SET", "CHLAUTH")
            .expect("SET CHLAUTH rule");
        assert_eq!(ch.since.as_deref(), Some("9.0"));
        assert!(ch.platform.is_none());
        // the extractor artifacts (MAXSMSGS) are artifacts on every
        // platform — no rule, never flagged; and ordinary commands have none
        assert!(m.command_availability_for("DEFINE", "MAXSMSGS").is_none());
        assert!(m.command_availability_for("ALTER", "QLOCAL").is_none());
    }

    #[test]
    fn value_docs_load_and_look_up_scoped() {
        let m = model();
        // general rule: any object carrying CCSID resolves numeric values
        assert_eq!(
            m.value_doc("ccsid", "COMMINFO", "1208"),
            Some("UTF-8 (Unicode)")
        );
        assert!(m
            .value_doc("CCSID", "QMGR", "65535")
            .unwrap()
            .contains("automatic"));
        // object scoping separates same-spelled values on different objects
        assert!(m.value_doc("ACTION", "CHLAUTH", "REMOVEALL").is_some());
        assert!(m.value_doc("ACTION", "CHANNEL", "REMOVEALL").is_none());
        assert!(m.value_doc("ACTION", "CHANNEL", "COMMIT").is_some());
        assert!(m.value_doc("ACTION", "TPIPE", "BACKOUT").is_some());
        assert!(m
            .value_doc("TYPE", "CHLAUTH", "BLOCKUSER")
            .unwrap()
            .contains("blocked user"));
        assert!(m
            .value_doc("TYPE", "QUEUE", "QCLUSTER")
            .unwrap()
            .contains("cluster"));
        assert!(m.value_doc("TYPE", "QMSTATUS", "HANDLE").is_none());
        // CHCKCLNT on CHLAUTH additionally knows ASQMGR; AUTHINFO does not
        assert!(m.value_doc("CHCKCLNT", "CHLAUTH", "ASQMGR").is_some());
        assert!(m.value_doc("CHCKCLNT", "AUTHINFO", "ASQMGR").is_none());
        assert!(m.value_doc("CHCKCLNT", "AUTHINFO", "OPTIONAL").is_some());
        // unknown values, unknown params, and empty spelling stay silent
        assert!(m.value_doc("CHLTYPE", "CHANNEL", "NOPE").is_none());
        assert!(m.value_doc("NOPE", "CHANNEL", "SDR").is_none());
        assert!(m.value_doc("", "CHANNEL", "SDR").is_none());
    }

    #[test]
    fn every_documented_value_exists_in_the_model() {
        // the anti-fabrication guard for `value_docs`: every curated
        // meaning must key on a value the merged model actually accepts
        // on its scoped commands — an enum value in the union of the
        // scoped enums, a number for a numeric parameter, or a value on
        // a definition the model does not constrain at all (the 2011
        // extraction left some parameter kinds as free strings)
        let m = model();
        let table = m.all_value_docs();
        assert_eq!(table.len(), 35, "value_docs rule count moved");
        for r in table {
            let hits = m.scoped_params(&r.param, &r.obj);
            assert!(!hits.is_empty(), "{}: no scoped command", r.param);
            let enum_of = |p: &Param| match &p.kind {
                ValueKind::Enum(vs) => vs.clone(),
                _ => Vec::new(),
            };
            let union: std::collections::HashSet<String> =
                hits.iter().flat_map(|(_, p)| enum_of(p)).collect();
            let numeric = hits.iter().any(|(_, p)| {
                matches!(p.kind, ValueKind::Integer)
                    || enum_of(p).iter().any(|x| x.eq_ignore_ascii_case("integer"))
            });
            let unconstrained = hits
                .iter()
                .any(|(_, p)| matches!(p.kind, ValueKind::Str | ValueKind::Name | ValueKind::Free));
            for v in r.values.keys() {
                assert!(
                    union.contains(v)
                        || (numeric && v.chars().all(|c| c.is_ascii_digit()))
                        || unconstrained,
                    "value_docs {}/{}: {v} is not a model value ({union:?})",
                    r.param,
                    r.obj.join(",")
                );
            }
        }
    }

    #[test]
    fn attrs_load_as_bare_words() {
        // the reference spells some attrs with their argument
        // (`PORT ( port-number )`); the loaded model must keep the bare
        // word only, or hover/completion matching cannot see them — and
        // the page-footer debris the extractor left in ALTER QLOCAL's
        // attrs list must not be "known" as a word
        let m = model();
        assert!(
            m.known_attrs.contains("PORT"),
            "PORT lost its bare-word form"
        );
        assert!(m.known_attrs.contains("QMNAME"));
        assert!(m.known_attrs.contains("IPADDR"));
        assert!(
            !m.known_attrs.iter().any(|a| a.contains(' ')),
            "debris survived"
        );
        let i = m.commands["DISPLAY"]["THREAD"];
        assert!(m.entry(i).attr_set.contains("QMNAME"));
        // and the raw junk string never reaches the command-card attrs list
        let i = m.commands["ALTER"]["QLOCAL"];
        assert!(m.entry(i).attrs.iter().all(|a| !a.contains("Page")));
    }

    #[test]
    fn every_attribute_has_a_description() {
        // the user-visible contract for attribute hover: no DISPLAY
        // attribute anywhere in the merged model may render as a bare
        // label — attr hover text comes through param_doc, exactly like
        // the hover card's attribute branch does
        let m = model();
        let mut missing = Vec::new();
        for (obj, attr) in m.all_scoped_attrs() {
            if m.param_doc(&attr, &obj).unwrap_or("").trim().is_empty() {
                missing.push(format!("{obj} {attr}"));
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "attributes with no hover text: {missing:?}"
        );
    }

    #[test]
    fn every_parameter_has_a_description() {
        // same contract for parameters: a param card falls back from
        // p.description to param_doc, so the pair must not BOTH be empty
        let m = model();
        let mut missing = Vec::new();
        for (obj, p) in m.all_params() {
            let has = !p.description.trim().is_empty()
                || !m.param_doc(&p.name, &obj).unwrap_or("").trim().is_empty();
            if !has {
                missing.push(format!("{obj} {}", p.name));
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "parameters with no hover text: {missing:?}"
        );
    }

    #[test]
    fn every_generic_value_word_exists_in_the_model() {
        // anti-fabrication guard for `generic_values`: a word that is not
        // an enum value SOMEWHERE in the model has no business in the
        // generic vocabulary (hover would annotate spellings MQ rejects)
        let m = model();
        let words = m.all_enum_words();
        for w in m.all_generic_values().keys() {
            assert!(
                words.contains(w),
                "generic_values word {w:?} is no model enum value"
            );
        }
    }

    #[test]
    fn no_parameter_enumerates_its_own_siblings() {
        // fingerprint of the extractor's pooling artifact: it read a page's
        // attribute dump ("the following attributes are displayable: …") as
        // the trailing value list of whatever parameter preceded it, so the
        // invented enums enumerate OTHER parameters of the same command.
        // No genuine MQSC enum is mostly sibling parameter names — the
        // overlay's flag_params table demotes every occurrence found against
        // the live railroads; this guard makes regressions (or new pages
        // added to the base model with the same artifact) fail loudly.
        let m = model();
        let mut pooled = Vec::new();
        for objs in m.commands.values() {
            for (object, &i) in objs {
                let e = m.entry(i);
                let names: std::collections::HashSet<String> =
                    e.params.iter().map(|p| p.name.clone()).collect();
                for p in &e.params {
                    let vals = match &p.kind {
                        ValueKind::Enum(v) => v,
                        _ => continue,
                    };
                    // the extractor's pooled windows are runs of the page's
                    // attribute dump and run 10-15 long; genuine short enums
                    // routinely collide with same-named flag parameters
                    // (TYPE(ALL|LOCAL|CLUSTER), PUTAUT(DEF|CTX|ONLYMCA|ALTMCA),
                    // OPENTYPE(ALL|INPUT|OUTPUT)) and must stay untouched.
                    if vals.len() < 6 {
                        continue;
                    }
                    let sib = vals
                        .iter()
                        .filter(|v| **v != p.name && names.contains(*v))
                        .count();
                    if sib * 2 >= vals.len() {
                        pooled.push(format!("{} {} {}", e.verb, object, p.name));
                    }
                }
            }
        }
        pooled.sort();
        assert!(
            pooled.is_empty(),
            "pooled-enum artifacts survived: {pooled:?}"
        );
    }

    #[test]
    fn pooled_attribute_words_load_as_flags() {
        // the demoted parameters keep their (real) reference description but
        // must end the merge as bare attribute selectors, per the MP 10.0.1
        // railroads: DISPLAY QMGR SSLEV / CONN(*) PID / CHSTATUS(*) COMPMSG
        // / QSTATUS(q) IPPROCS / CHANNEL(x) PROPCTL are all bare [ WORD ]
        for (verb, obj, name) in [
            ("DISPLAY", "QMGR", "SSLEV"),
            ("DISPLAY", "QMGR", "ADOPTCHK"),
            ("DISPLAY", "QMGR", "RCVTTYPE"),
            ("DISPLAY", "CONN", "PID"),
            ("DISPLAY", "CONN", "APPLTYPE"),
            ("DISPLAY", "CHSTATUS", "COMPMSG"),
            ("DISPLAY", "QSTATUS", "IPPROCS"),
            ("DISPLAY", "CHANNEL", "PROPCTL"),
        ] {
            let p = param(verb, obj, name);
            assert!(
                matches!(p.kind, ValueKind::Flag),
                "{verb} {obj} {name}: {:?}",
                p.kind
            );
            assert!(
                !p.description.trim().is_empty(),
                "{verb} {obj} {name} lost its description in the demotion"
            );
        }
        // the real enums on the same pages are untouched (same-name siblings
        // on ALTER QMGR still carry their genuine values)
        let alter_sslev = enum_values(param("ALTER", "QMGR", "SSLEV"));
        assert!(alter_sslev.contains(&"DISABLED".to_string()));
        let alter_rcvt = enum_values(param("ALTER", "QMGR", "RCVTTYPE"));
        assert!(
            alter_rcvt.contains(&"MULTIPLY".to_string())
                && alter_rcvt.contains(&"EQUAL".to_string()),
            "ALTER QMGR RCVTTYPE: {alter_rcvt:?}"
        );
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

    #[test]
    fn hover_text_carries_no_platform_table_residue() {
        // the 2011 pages interleave their platform-support tables
        // ("i5/OS® UNIX systems Windows z/OS®" / "n m l k j n m l k j")
        // INTO the prose flow; the extractor drops those lines
        // (extract_model.py::_is_table_row / _is_cell_heading) before they
        // can reach a card. These strings reappearing in a description or
        // footnote means the cull regressed — the pre-fix model had 24 of
        // these spills.
        let m = model();
        let junk = ["n m l k j", "UNIX systems Windows"];
        let mut offenders = Vec::new();
        let mut check = |what: String, text: &str| {
            if junk.iter().any(|j| text.contains(j)) {
                offenders.push(what);
            }
        };
        for (obj, p) in m.all_params() {
            check(format!("{obj} {}", p.name), &p.description);
            for (n, f) in p.footnotes.iter().enumerate() {
                check(format!("{obj} {} footnote{n}", p.name), f);
            }
        }
        for objs in m.commands.values() {
            for (object, &i) in objs {
                for (n, f) in m.entry(i).footnotes.iter().enumerate() {
                    check(format!("{object} entry footnote{n}"), f);
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "hover text with table residue: {offenders:?}"
        );
    }

    #[test]
    fn clamped_descriptions_end_at_a_sentence() {
        // extract_model.py caps a parameter description at DOC_LIMIT and
        // rewinds to the last sentence end inside the cap; the old hard
        // [:500] cut produced hover cards ending mid-word ("…the lis").
        // Anything at/near the cap is clamped by definition, so it must
        // close on punctuation.
        let m = model();
        let mut offenders = Vec::new();
        for (obj, p) in m.all_params() {
            if p.description.chars().count() >= 990
                && !p.description.ends_with(['.', '?', '!', ':'])
            {
                offenders.push(format!("{obj} {}", p.name));
            }
        }
        assert!(
            offenders.is_empty(),
            "clamped descriptions cut mid-word: {offenders:?}"
        );
    }

    #[test]
    fn wrapped_footnote_continuations_are_joined() {
        // the print flow wraps the recurring "This is the default supplied
        // with WebSphere MQ (except on z/OS, where it is 999 999 999), but
        // your / installation might have changed it." across a blank line;
        // stopping at the blank is what made DEFINE QLOCAL MAXDEPTH's hover
        // end at "…but your". The join must survive extractor edits.
        let m = model();
        let i = m.commands["DEFINE"]["QLOCAL"];
        let p = m
            .entry(i)
            .params
            .iter()
            .find(|p| p.name == "MAXDEPTH")
            .expect("DEFINE QLOCAL MAXDEPTH");
        let f = p
            .footnotes
            .iter()
            .find(|f| f.contains("999 999 999"))
            .expect("MAXDEPTH default footnote");
        assert!(
            f.ends_with("installation might have changed it."),
            "MAXDEPTH footnote dangles: {f:?}"
        );
    }
}
