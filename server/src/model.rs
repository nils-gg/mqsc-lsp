//! MQSC command model, extracted from the IBM "Script (MQSC) Command
//! Reference" by tools/extract_model.py.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;

const MODEL_JSON: &str = include_str!("../../model/mqsc-model.json");

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

#[derive(Debug, Deserialize)]
struct RawModel {
    commands: HashMap<String, HashMap<String, CommandEntry>>,
    #[serde(default)]
    param_docs: HashMap<String, Vec<(String, String)>>,
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
    "ALTER", "ARCHIVE", "BACKUP", "CLEAR", "DEFINE", "DELETE", "DISPLAY",
    "MOVE", "PING", "RECOVER", "REFRESH", "RESET", "RESOLVE", "RESUME",
    "RVERIFY", "SET", "START", "STOP", "SUSPEND",
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
            let raw: RawModel = serde_json::from_str(MODEL_JSON)
                .expect("embedded mqsc-model.json must be valid");

            let mut entries: Vec<Box<Command>> = Vec::new();
            let mut commands: HashMap<&'static str, HashMap<String, usize>> = HashMap::new();
            let mut verb_syn: HashMap<String, &'static str> = HashMap::new();
            let mut obj_syn: HashMap<(&'static str, String), String> = HashMap::new();
            let mut global_obj_syn: HashMap<String, String> = HashMap::new();
            let mut known_params: HashSet<String> = HashSet::new();
            let mut known_attrs: HashSet<String> = HashSet::new();

            // stable verb ordering for completion
            for (verb_s, objs) in raw.commands {
                let verb: &'static str = match VERBS.iter().copied().find(|v| *v == verb_s.as_str()) {
                    Some(v) => v,
                    None => continue,
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
            if let Some(q) = commands.get("DISPLAY").and_then(|m| m.get("QUEUE")).copied() {
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
                if let Some(i) = self.obj_syn.get(&(canon, o.clone())).and_then(|real| objs.get(real.as_str()).copied()) {
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
        let Some(canon) = self.canon_verb(verb) else { return Vec::new() };
        let mut out: Vec<&str> = self
            .commands
            .get(canon)
            .map(|m| m.keys().map(|s| s.as_str()).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// documentation text for a parameter name, preferring contexts that
    /// mention `object`.
    pub fn param_doc(&self, name: &str, object: &str) -> Option<&str> {
        for cand in [name.to_string(), strip_no(name)] {
            let Some(docs) = self.param_docs.get(&cand) else { continue };
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

pub fn suggest<'a, I: IntoIterator<Item = &'a str>>(word: &str, pool: I, max: usize) -> Vec<&'a str> {
    let w = word.to_ascii_uppercase();
    let mut hits: Vec<(usize, &str)> = pool
        .into_iter()
        .map(|c| (distance(&w, c), c))
        .filter(|(d, _)| *d <= 2.max(w.len() / 3))
        .collect();
    hits.sort_by_key(|(d, c)| (*d, *c));
    hits.truncate(max);
    hits.into_iter().map(|(_, c)| c).collect()
}
