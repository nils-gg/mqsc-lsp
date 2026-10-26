//! Tree-sitter parsing and all MQSC language analysis: diagnostics, hover,
//! completion, document symbols, folding, highlights, semantic tokens.

use std::collections::{BTreeMap, HashSet};

use lsp_types::{
    self as lsp, CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentHighlight, DocumentHighlightKind, DocumentSymbol, FoldingRange, FoldingRangeKind,
    Hover, HoverContents, InsertTextFormat, MarkupContent, MarkupKind, Position, Range,
    SemanticToken, SymbolKind, Url,
};
use serde_json::json;
use tree_sitter::{Language, Node, Tree};

use crate::model::{self, Model, Param, ValueKind};

extern "C" {
    fn tree_sitter_mqsc() -> *const tree_sitter::ffi::TSLanguage;
}

pub fn language() -> Language {
    unsafe { Language::from_raw(tree_sitter_mqsc()) }
}

pub fn parse(text: &str) -> Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language())
        .expect("tree-sitter ABI compatible grammar");
    parser.parse(text, None).expect("tree-sitter parse")
}

// ------------------------------------------------------------------ text index

/// Byte-offset <-> LSP (UTF-16 line/character) conversion.
pub struct Lines<'a> {
    starts: Vec<usize>,
    text: &'a str,
}

impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Lines<'a> {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Lines { starts, text }
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        }
    }

    pub fn pos(&self, offset: usize) -> Position {
        let offset = offset.min(self.text.len());
        let line = self.line_of(offset);
        let start = self.starts[line];
        // Callers legitimately do byte arithmetic (`s + 1`, `end_byte() + 1`)
        // to address the position just after a character; if that lands
        // inside a multi-byte char, snap back to its start instead of
        // panicking the slice below (REVIEW.md C1).
        let mut offset = offset;
        while offset > start && !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let col = utf16_len(&self.text[start..offset]);
        Position {
            line: line as u32,
            character: col as u32,
        }
    }

    pub fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.pos(start), self.pos(end))
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.starts[line.min(self.starts.len() - 1)]
    }

    /// byte offset of an LSP position (clamped)
    pub fn offset(&self, p: Position) -> usize {
        let l = (p.line as usize).min(self.starts.len() - 1);
        let start = self.starts[l];
        let end = self.starts.get(l + 1).copied().unwrap_or(self.text.len());
        let mut off = start;
        let mut col = 0u32;
        for ch in self.text[start..end].chars() {
            if col >= p.character {
                break;
            }
            col += ch.len_utf16() as u32;
            off += ch.len_utf8();
        }
        off.min(end)
    }
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

fn node_text<'a>(n: Node<'a>, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// MQSC is case-insensitive: when the user typed a token entirely in lower
/// case, quickfix suggestions should not force it to upper case.
fn mirror_case(sample: &str, canonical_upper: &str) -> String {
    if sample.chars().any(|c| c.is_lowercase()) && !sample.chars().any(|c| c.is_uppercase()) {
        canonical_upper.to_ascii_lowercase()
    } else {
        canonical_upper.to_string()
    }
}

fn node_at(root: Node<'_>, off: usize) -> Node<'_> {
    let mut node = root;
    loop {
        let mut next = None;
        for i in 0..node.named_child_count() {
            if let Some(c) = node.named_child(i) {
                if c.start_byte() <= off && off < c.end_byte() {
                    next = Some(c);
                    break;
                }
                // zero-width or boundary-touching named nodes still win when
                // nothing better is found
                if c.start_byte() <= off && off <= c.end_byte() && next.is_none() {
                    next = Some(c);
                }
            }
        }
        match next {
            Some(c) if c.end_byte() > c.start_byte() => node = c,
            Some(c) if node.end_byte() == node.start_byte() => node = c,
            _ => break,
        }
    }
    node
}

fn ancestor_of<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut x = n;
    loop {
        if x.kind() == kind {
            return Some(x);
        }
        x = x.parent()?;
    }
}

// ---------------------------------------------------------------- diagnostics

const CODE_PARSE: i32 = 1;
const CODE_UNKNOWN_VERB: i32 = 2;
const CODE_UNKNOWN_OBJECT: i32 = 3;
const CODE_UNKNOWN_PARAM: i32 = 4;
const CODE_BAD_VALUE: i32 = 5;
const CODE_DUPLICATE: i32 = 6;
const CODE_MISSING_CONT: i32 = 7;
const CODE_LEXICAL: i32 = 8;
const CODE_MISSING_REQUIRED: i32 = 9;
const CODE_MISSING_NAME: i32 = 10;
const CODE_WILDCARD: i32 = 11;
const CODE_UNDEFINED_OBJECT: i32 = 12;
const CODE_NAME_TOO_LONG: i32 = 13;

/// Friendly settings key for a diagnostic code (see README Configuration).
pub fn diag_name(code: i32) -> &'static str {
    match code {
        CODE_PARSE => "PARSE_ERROR",
        CODE_UNKNOWN_VERB => "UNKNOWN_VERB",
        CODE_UNKNOWN_OBJECT => "UNKNOWN_OBJECT",
        CODE_UNKNOWN_PARAM => "UNKNOWN_PARAM",
        CODE_BAD_VALUE => "BAD_VALUE",
        CODE_DUPLICATE => "DUPLICATE_PARAM",
        CODE_MISSING_CONT => "MISSING_CONTINUATION",
        CODE_LEXICAL => "LEXICAL",
        CODE_MISSING_REQUIRED => "MISSING_REQUIRED",
        CODE_MISSING_NAME => "MISSING_NAME",
        CODE_WILDCARD => "WILDCARD",
        CODE_UNDEFINED_OBJECT => "UNDEFINED_OBJECT",
        CODE_NAME_TOO_LONG => "NAME_TOO_LONG",
        _ => "OTHER",
    }
}

struct Diags {
    items: Vec<Diagnostic>,
}

impl Diags {
    fn push(&mut self, r: Range, severity: DiagnosticSeverity, code: i32, msg: String) {
        self.push_opt(r, severity, code, msg, None)
    }

    /// Diagnostic carrying one-click replacements (LSP codeAction quickfixes)
    /// for the diagnostic range itself.
    fn push_fixes(
        &mut self,
        r: Range,
        severity: DiagnosticSeverity,
        code: i32,
        msg: String,
        fixes: Vec<String>,
    ) {
        let data = if fixes.is_empty() {
            None
        } else {
            Some(json!({ "quickfixes": fixes }))
        };
        self.push_opt(r, severity, code, msg, data)
    }

    fn push_opt(
        &mut self,
        r: Range,
        severity: DiagnosticSeverity,
        code: i32,
        msg: String,
        data: Option<serde_json::Value>,
    ) {
        self.items.push(Diagnostic {
            range: r,
            severity: Some(severity),
            code: Some(lsp::NumberOrString::Number(code)),
            source: Some("mqsc".into()),
            message: msg,
            data,
            ..Default::default()
        });
    }
}

/// An item in a command: the flat child list after the verb.
enum Item<'t> {
    Verb(Node<'t>),
    Word(Node<'t>),
    Arg(Node<'t>),
    /// bare wildcard directly under command
    Wild(Node<'t>),
    /// value directly under command (z/OS NAME=value form)
    Val(Node<'t>),
}

fn command_items(cmd: Node<'_>) -> Vec<Item<'_>> {
    let mut items = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.children(&mut cursor) {
        match child.kind() {
            "verb" => items.push(Item::Verb(child)),
            "word" => items.push(Item::Word(child)),
            "argument" => items.push(Item::Arg(child)),
            "wildcard" => items.push(Item::Wild(child)),
            "value" => items.push(Item::Val(child)),
            _ => {}
        }
    }
    items
}

pub fn analyze(
    text: &str,
    tree: &Tree,
    m: &Model,
    _uri: &Url,
    extra_defs: &Definitions,
) -> Vec<Diagnostic> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut d = Diags { items: Vec::new() };

    // ---- lexical checks over raw text
    for (i, line) in text.lines().enumerate() {
        for col in line.match_indices('\t').map(|(c, _)| c) {
            let s = lines.line_start(i) + line[..col].len();
            d.push(
                lines.range(s, s + 1),
                DiagnosticSeverity::WARNING,
                CODE_LEXICAL,
                "MQSC lines may contain blank characters only; replace tabs with blanks".into(),
            );
        }
        if line.chars().count() > 2048 {
            let s = lines.line_start(i);
            d.push(
                lines.range(s, s + 1),
                DiagnosticSeverity::WARNING,
                CODE_LEXICAL,
                "MQSC lines are limited to 2048 characters".into(),
            );
        }
    }

    // ---- parse errors
    let root = tree.root_node();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.is_error() {
            let off = n.start_byte();
            let word_at = first_word(n, src);
            let mut msg = if n.kind() == "MISSING" {
                "Missing element".to_string()
            } else {
                "Syntax error".to_string()
            };
            let mut code = CODE_PARSE;

            if let Some(w) = &word_at {
                let up = w.to_ascii_uppercase();
                let looks_param = m.canon_verb(&up).is_none()
                    && (m.known_params.contains(&up) || m.known_attrs.contains(&up));
                if looks_param {
                    if let Some(pw) = command_before(tree, off) {
                        let end_line = line_of(&lines, pw.end_byte());
                        code = CODE_MISSING_CONT;
                        msg = format!(
                            "'{w}' is not a command: it continues the previous command that ends \
                             on line {}. Add '+' or '-' at the end of that line to continue it",
                            end_line + 1
                        );
                    }
                }
            }
            if n.kind() == "ERROR" {
                let snippet: String = node_text(n, src).trim().chars().take(48).collect();
                if !snippet.is_empty() {
                    msg = format!("{msg}: `{snippet}`");
                }
            }
            d.push(
                lines.range(n.start_byte(), n.end_byte().max(n.start_byte() + 1)),
                DiagnosticSeverity::ERROR,
                code,
                msg,
            );
            continue; // do not descend into error noise
        }
        if n.kind() == "dangling_continuation" {
            // A '-'/+ continuation marker the scanner could not resolve into
            // a real continuation: the rest of the file is blank lines,
            // comments or nothing at all. The command is truncated.
            let marker = node_text(n, src).trim().chars().next().unwrap_or('-');
            d.push(
                lines.range(n.start_byte(), n.end_byte().max(n.start_byte() + 1)),
                DiagnosticSeverity::ERROR,
                CODE_MISSING_CONT,
                format!(
                    "'{marker}' continuation marker is never continued: the file ends \
                     without a continuation line for this command"
                ),
            );
        }
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i) {
                stack.push(c);
            }
        }
    }

    // ---- same-script object knowledge (shared by all semantic checks)
    let mut defs = collect_definitions(tree, src, m);
    defs.add_external(extra_defs);

    // ---- semantic checks per command
    let mut cursor = root.walk();
    for cmd in root.named_children(&mut cursor) {
        if cmd.kind() == "command" {
            check_command(cmd, src, &lines, m, &defs, &mut d);
        }
    }

    check_question_wildcards(tree, src, &lines, m, &mut d);

    d.items
}

fn line_of(lines: &Lines, off: usize) -> usize {
    lines.pos(off).line as usize
}

fn first_word(n: Node<'_>, src: &[u8]) -> Option<String> {
    let mut stack = vec![n];
    while let Some(x) = stack.pop() {
        if x.kind() == "word" {
            return Some(node_text(x, src).to_string());
        }
        for i in 0..x.named_child_count() {
            if let Some(c) = x.named_child(i) {
                stack.push(c);
            }
        }
    }
    None
}

fn command_before(tree: &Tree, byte_off: usize) -> Option<Node<'_>> {
    let root = tree.root_node();
    let mut last = None;
    let mut cursor = root.walk();
    for c in root.named_children(&mut cursor) {
        if c.kind() == "command" && c.end_byte() <= byte_off {
            last = Some(c);
        } else if c.start_byte() >= byte_off {
            break;
        }
    }
    last
}

// ------------------------------------------------- same-script object knowledge

/// Objects the script itself defines (`DEFINE <verb> (name)`). Used to offer
/// known names in completion and to hint at references that are never
/// DEFINEd in this file (code 12). Whole-file and order-independent: an
/// object DEFINEd later in the script counts as defined.
#[derive(Debug, Clone, Default)]
pub struct Definitions {
    /// every defined name, uppercase
    pub names: HashSet<String>,
    /// canonical object type -> defined names (in script order, unique)
    pub by_obj: BTreeMap<String, Vec<String>>,
    /// whether the script contains at least one resolvable DEFINE
    pub any_defines: bool,
    /// names merged in from OTHER open documents (T-17); code-12 messages
    /// widen their scope wording accordingly
    pub from_other_files: bool,
}

/// Every `DEFINE` in the tree. `pub` so the server can cache one per open
/// document and merge them into a workspace-wide view (T-17).
pub fn collect_definitions(tree: &Tree, src: &[u8], m: &Model) -> Definitions {
    let mut defs = Definitions::default();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for cmd in root.named_children(&mut cursor) {
        if cmd.kind() != "command" {
            continue;
        }
        let items = command_items(cmd);
        let verb = match items.first() {
            Some(Item::Verb(n)) => node_text(*n, src),
            _ => continue,
        };
        if m.canon_verb(verb) != Some("DEFINE") {
            continue;
        }
        let obj_txt = match items.get(1) {
            Some(Item::Word(n)) => Some(node_text(*n, src)),
            _ => None,
        };
        let Some(i) = m.resolve(verb, obj_txt) else {
            continue;
        };
        let obj = m.entry(i).object.to_string();
        // the object-name group is the first argument group after the verb
        let Some(Item::Arg(g)) = items.get(2) else {
            continue;
        };
        for v in value_nodes(*g) {
            for name in value_name_words(v, src) {
                defs.any_defines = true;
                if defs.names.insert(name.clone()) {
                    defs.by_obj.entry(obj.clone()).or_default().push(name);
                }
            }
        }
    }
    defs
}

/// Plain-name text of a value node: exactly one word or quoted string and no
/// wildcard/integer parts (a generic pattern is not one concrete name).
fn value_name_words(v: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut has_wildcard = false;
    let mut name: Option<String> = None;
    let mut multi = false;
    let mut vc = v.walk();
    for c in v.named_children(&mut vc) {
        match c.kind() {
            "wildcard" => has_wildcard = true,
            "integer" => multi = true,
            "word" => {
                if name.is_some() {
                    multi = true;
                } else {
                    name = Some(node_text(c, src).to_ascii_uppercase());
                }
            }
            "string" => {
                let inner = node_text(c, src).trim_matches('\'').replace("''", "'");
                if name.is_some() {
                    multi = true;
                } else {
                    name = Some(inner.to_ascii_uppercase());
                }
            }
            _ => {}
        }
    }
    match name {
        Some(n) if !has_wildcard && !multi => vec![n],
        _ => Vec::new(),
    }
}

/// Which family of objects a name-valued parameter points at. The model
/// types all of these plain string, so this is a curated heuristic table
/// (documented in README "Same-script object knowledge").
#[derive(Clone, Copy)]
enum ObjRef {
    /// an object of the same type as the current command's object
    SameType,
    /// a queue-like object (QLOCAL/QMODEL/QALIAS/QREMOTE/QCLUSTER/QUEUE)
    QueueFamily,
}

fn object_ref_param(pname: &str, cmd_obj: &str) -> Option<ObjRef> {
    match pname {
        "LIKE" => Some(if cmd_obj.starts_with('Q') {
            ObjRef::QueueFamily
        } else {
            ObjRef::SameType
        }),
        "TARGET" | "TGTQ" | "XMITQ" | "MODEL" | "MDURMDL" | "MNDURMDL" | "FROMQ" | "TOQLOCAL"
        | "TOQALIAS" | "TOQNAME" => Some(ObjRef::QueueFamily),
        _ => None,
    }
}

fn obj_ref_matches(r: ObjRef, def_obj: &str, cmd_obj: &str) -> bool {
    match r {
        ObjRef::QueueFamily => def_obj.starts_with('Q'),
        ObjRef::SameType => def_obj == cmd_obj,
    }
}

/// Defined names matching a reference family, as (object type, name) pairs.
/// `None` means every defined name.
fn names_for<'a>(
    defs: &'a Definitions,
    r: Option<ObjRef>,
    cmd_obj: &str,
) -> Vec<(&'a str, &'a str)> {
    defs.by_obj
        .iter()
        .filter(|(obj, _)| match r {
            Some(rr) => obj_ref_matches(rr, obj, cmd_obj),
            None => true,
        })
        .flat_map(|(obj, ns)| ns.iter().map(|n| (obj.as_str(), n.as_str())))
        .collect()
}

impl Definitions {
    /// Merge the defined names of other open documents into this view
    /// (T-17). A name defined in a sibling script stops the code-12 hint and
    /// appears in completions, exactly as if it were DEFINEd here — the
    /// gate (arming) widens the same way, which is the whole point for
    /// per-object-type dump scripts.
    pub fn add_external(&mut self, other: &Definitions) {
        if other.names.is_empty() {
            return;
        }
        self.from_other_files = true;
        self.any_defines = true;
        self.names.extend(other.names.iter().cloned());
        for (obj, ns) in &other.by_obj {
            let entry = self.by_obj.entry(obj.clone()).or_default();
            for n in ns {
                if !entry.contains(n) {
                    entry.push(n.clone());
                }
            }
        }
    }

    /// Whether code 12 may fire at all for a reference family in this script:
    /// only when the script DEFINEs at least one object of a matching type.
    /// A dump that only contains, say, QALIAS definitions never gets TARGET
    /// hints (the real queues live in other scripts) — see README
    /// "Same-script object knowledge".
    fn arms(&self, r: ObjRef, cmd_obj: &str) -> bool {
        match r {
            ObjRef::QueueFamily => self.by_obj.keys().any(|o| o.starts_with('Q')),
            ObjRef::SameType => self.by_obj.contains_key(cmd_obj),
        }
    }
}

/// Code 12: an object-name reference (TARGET, XMITQ, LIKE, ...) that no
/// DEFINE in this script provides. Hint severity on purpose: runmqsc
/// scripts routinely reference queue-manager objects that exist already.
fn check_obj_ref(
    pname: &str,
    cmd_obj: &str,
    values: &[Node<'_>],
    src: &[u8],
    lines: &Lines,
    defs: &Definitions,
    d: &mut Diags,
) {
    let Some(r) = object_ref_param(pname, cmd_obj) else {
        return;
    };
    if !defs.any_defines || !defs.arms(r, cmd_obj) {
        return;
    }
    for v in values {
        for name in value_name_words(*v, src) {
            // Blank/' ' values (legal in dumps, e.g. SHORTUSR(' ')) are never typos.
            if name.trim().is_empty() {
                continue;
            }
            // IBM ships every SYSTEM.* object; never flag those.
            if defs.names.contains(&name) || name.starts_with("SYSTEM") {
                continue;
            }
            d.push(
                lines.range(v.start_byte(), v.end_byte()),
                DiagnosticSeverity::HINT,
                CODE_UNDEFINED_OBJECT,
                format!(
                    "'{name}' is not DEFINEd in {}; it must already exist in the \
                     queue manager (or is a typo)",
                    if defs.from_other_files {
                        "this script or the other open documents"
                    } else {
                        "this script"
                    }
                ),
            );
        }
    }
}

// ---------------------- object-name sites (navigation: T-16, hover: T-18)

/// Where a name is DEFINEd in the OTHER open documents (T-17 cross-file
/// hover): UPPERCASE name -> (display file name, 0-based line, object type).
pub type ExternalDefs = BTreeMap<String, Vec<(String, u32, String)>>;

/// One appearance of an object name in the script: the name introduced by
/// `DEFINE <TYPE>(NAME)` or a place that uses it (the positional object
/// name, `TARGET(...)`, `XMITQ(...)`, ...). Ranges cover the token itself,
/// quotes included for quoted names.
#[derive(Debug, Clone)]
pub struct NameSite {
    /// uppercased, normalised name — the join key across sites
    pub name: String,
    /// the name text as written (quotes removed)
    pub word: String,
    pub range: Range,
    /// written in `'quoted'` form (rename must re-quote such sites)
    pub quoted: bool,
    /// canonical model object type for a DEFINE site; `None` for uses
    pub defines: Option<String>,
}

/// Every name site in one document. A name counts only where the command
/// resolves against the model and only at the positions the code-12 hint
/// also treats as references (positional name + curated `object_ref_param`
/// table), so navigation and the hint never disagree about what a
/// reference is.
#[derive(Debug, Default)]
pub struct ObjectIndex {
    pub sites: Vec<NameSite>,
}

impl ObjectIndex {
    /// The name site containing `pos` (name tokens never overlap).
    pub fn site_at(&self, pos: Position) -> Option<&NameSite> {
        self.sites.iter().find(|s| {
            let after_start = pos.line > s.range.start.line
                || (pos.line == s.range.start.line && pos.character >= s.range.start.character);
            let before_end = pos.line < s.range.end.line
                || (pos.line == s.range.end.line && pos.character < s.range.end.character);
            after_start && before_end
        })
    }

    pub fn sites_of<'a>(&'a self, name_upper: &'a str) -> impl Iterator<Item = &'a NameSite> {
        self.sites.iter().filter(move |s| s.name == name_upper)
    }

    pub fn definitions_of<'a>(&'a self, name_upper: &'a str) -> Vec<&'a NameSite> {
        self.sites_of(name_upper)
            .filter(|s| s.defines.is_some())
            .collect()
    }

    pub fn definitions(&self) -> impl Iterator<Item = &NameSite> {
        self.sites.iter().filter(|s| s.defines.is_some())
    }
}

/// The name token of a value node: `(UPPERCASE, as-written, node)` or None
/// when the value is not exactly one name (wildcard patterns, integers,
/// multi-token values). Mirrors `value_name_words`, but keeps the node so
/// callers get a range.
fn name_value_node<'t>(v: Node<'t>, src: &[u8]) -> Option<(String, String, Node<'t>)> {
    let mut out: Option<(String, String, Node<'_>)> = None;
    let mut multi = false;
    let mut vc = v.walk();
    for c in v.named_children(&mut vc) {
        match c.kind() {
            "wildcard" | "integer" => multi = true,
            "word" => {
                if out.is_some() {
                    multi = true;
                } else {
                    let t = node_text(c, src);
                    out = Some((t.to_ascii_uppercase(), t.to_string(), c));
                }
            }
            "string" => {
                if out.is_some() {
                    multi = true;
                } else {
                    let t = node_text(c, src);
                    let inner = t.trim_matches('\'').replace("''", "'");
                    out = Some((inner.to_ascii_uppercase(), inner, c));
                }
            }
            _ => {}
        }
    }
    if multi {
        None
    } else {
        out
    }
}

/// Build the name-site index for a document (per-request: MQSC files are
/// line-per-command small, and this keeps the index perfectly in sync with
/// the cached tree).
pub fn object_index(text: &str, tree: &Tree, m: &Model) -> ObjectIndex {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut idx = ObjectIndex::default();
    let mut cursor = tree.root_node().walk();
    for cmd in tree.root_node().named_children(&mut cursor) {
        if cmd.kind() != "command" {
            continue;
        }
        let Some((verb, obj_txt, e)) = command_context(cmd, src, m) else {
            continue;
        };
        let is_define = m.canon_verb(verb) == Some("DEFINE");
        let _ = obj_txt;
        let items = command_items(cmd);
        // track the last parameter word for object-ref parameters
        let mut owner: Option<String> = None;
        for (i, it) in items.iter().enumerate() {
            match it {
                Item::Word(n) => owner = Some(node_text(*n, src).to_ascii_uppercase()),
                Item::Verb(_) => owner = None,
                Item::Arg(g) => {
                    // The object-name group sits directly after the object
                    // word (items[1] word, items[2] arg); any other group is
                    // a parameter's, taken from its owner word.
                    let positional = i == 2 && matches!(items.get(1), Some(Item::Word(_)));
                    let ref_param = !positional
                        && owner
                            .as_deref()
                            .and_then(|w| object_ref_param(w, e.object))
                            .is_some();
                    if !(positional || ref_param) {
                        continue;
                    }
                    for v in value_nodes(*g) {
                        if let Some((name, word, node)) = name_value_node(v, src) {
                            if name.trim().is_empty() {
                                continue; // blank string values are data, not names
                            }
                            let quoted = node.kind() == "string";
                            idx.sites.push(NameSite {
                                name,
                                word,
                                range: lines.range(node.start_byte(), node.end_byte()),
                                quoted,
                                defines: if positional && is_define {
                                    Some(e.object.to_string())
                                } else {
                                    None
                                },
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    idx
}

fn check_command(
    cmd: Node<'_>,
    src: &[u8],
    lines: &Lines,
    m: &Model,
    defs: &Definitions,
    d: &mut Diags,
) {
    let items = command_items(cmd);
    let verb_node = match items.first() {
        Some(Item::Verb(n)) => *n,
        _ => return,
    };
    let verb_txt = node_text(verb_node, src);

    let mut idx = 1usize;
    let obj_word: Option<Node<'_>> = match items.get(idx) {
        Some(Item::Word(n)) => {
            idx += 1;
            Some(*n)
        }
        _ => None,
    };
    let obj_txt = obj_word.map(|n| node_text(n, src));
    let cmd_idx = m.resolve(verb_txt, obj_txt);

    let e = match (obj_word, cmd_idx) {
        (None, _) => {
            if m.canon_verb(verb_txt).is_some() {
                let objs = m.objects_for(verb_txt);
                d.push(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::INFORMATION,
                    CODE_UNKNOWN_OBJECT,
                    format!(
                        "{} requires an object type: {}",
                        verb_txt.to_ascii_uppercase(),
                        objs.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
            return;
        }
        (Some(w), None) => {
            let known_verb = m.canon_verb(verb_txt).is_some();
            let up_w = node_text(w, src).to_ascii_uppercase();
            if known_verb {
                let objs = m.objects_for(verb_txt);
                let sugs = model::suggest(&up_w, objs.iter().copied(), 3);
                let verb_up = verb_txt.to_ascii_uppercase();
                let msg = if sugs.is_empty() {
                    format!(
                        "Unknown {verb_up} command '{up_w}'; expected one of {}",
                        objs.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
                    )
                } else {
                    format!(
                        "Unknown {verb_up} command '{up_w}'; did you mean {}?",
                        sugs.iter()
                            .map(|s| format!("{verb_up} {s}"))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    )
                };
                // the fix replaces the whole verb+object span
                let fixes = sugs.iter().map(|s| format!("{verb_up} {s}")).collect();
                d.push_fixes(
                    lines.range(verb_node.start_byte(), w.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_OBJECT,
                    msg,
                    fixes,
                );
            } else if m.known_params.contains(&up_w) || m.known_attrs.contains(&up_w) {
                d.push(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_MISSING_CONT,
                    format!(
                        "'{up_w}' is not a command; it is a {}. If this line continues the \
                         command above, end the previous line with '+' or '-'",
                        if m.known_params.contains(&up_w) {
                            "parameter"
                        } else {
                            "attribute"
                        }
                    ),
                );
            } else {
                let sugs = model::suggest(verb_txt, m.all_verbs(), 3);
                let msg = if sugs.is_empty() {
                    format!("Unknown command '{verb_txt}'")
                } else {
                    format!(
                        "Unknown command '{verb_txt}'; did you mean {}?",
                        sugs.join(", ")
                    )
                };
                let fixes = sugs.iter().map(|s| s.to_string()).collect();
                d.push_fixes(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_VERB,
                    msg,
                    fixes,
                );
            }
            return;
        }
        (Some(_), Some(i)) => m.entry(i),
    };

    let multi_pos = matches!(
        e.verb,
        "DISPLAY"
            | "START"
            | "STOP"
            | "CLEAR"
            | "RESET"
            | "MOVE"
            | "PING"
            | "REFRESH"
            | "RESUME"
            | "SUSPEND"
    );
    let positional_required = matches!(e.verb, "DEFINE" | "ALTER" | "DELETE");

    let mut seen_name = false;
    // The node holding the positional object name (a bare word, or the
    // value inside the first argument group); recorded at the `seen_name`
    // sites below for the code-13 length check at the end.
    let mut name_node: Option<Node<'_>> = None;
    // When a word has already been flagged as not-a-parameter, the argument
    // group directly after it belongs to the same typo: report once, not twice.
    let mut mute_next_group = false;
    let mut used: HashSet<String> = HashSet::new();
    let mut pending: Option<(String, Node<'_>)> = None;

    for (k, item) in items[idx..].iter().enumerate() {
        match item {
            Item::Word(n) => {
                mute_next_group = false;
                let w = node_text(*n, src);
                let up = w.to_ascii_uppercase();
                // Code 6 applies only to genuine keyword parameters. Bare
                // positional object names (`DISPLAY QLOCAL(A) B B`) and
                // repeated DISPLAY attributes are legal MQSC and must not
                // trigger it; `used` still records every word so the
                // missing-required check below sees what was supplied.
                if e.by_name.contains_key(&up) && used.contains(&up) {
                    d.push(
                        lines.range(n.start_byte(), n.end_byte()),
                        DiagnosticSeverity::ERROR,
                        CODE_DUPLICATE,
                        format!("Parameter {up} appears more than once; MQSC allows it once"),
                    );
                }
                used.insert(up.clone());

                if let Some(&pi) = e.by_name.get(&up) {
                    let p = &e.params[pi];
                    let wants_group =
                        !matches!(p.kind, ValueKind::Flag | ValueKind::Star | ValueKind::Free)
                            || matches!(p.kind, ValueKind::Enum(_));
                    if wants_group && !matches!(p.kind, ValueKind::Flag) {
                        pending = Some((up.clone(), *n));
                    }
                    continue;
                }
                if e.attr_set.contains(&up) {
                    continue; // positional display attribute
                }
                if !seen_name && e.positional.is_some() {
                    seen_name = true;
                    continue; // positional object name
                }
                if e.positional.is_some() && multi_pos {
                    // Additional bare names (e.g. `DISPLAY QUEUE (A) B C`).
                    // A word that carries an argument group is a keyword
                    // parameter, not a name: fall through to the unknown-word
                    // path so the model gap shows up on the word, once,
                    // instead of an "Unexpected argument group" cascade.
                    let next_is_group = matches!(items.get(idx + k + 1), Some(Item::Arg(_)));
                    if !next_is_group {
                        continue;
                    }
                }
                let pool: Vec<&str> = e
                    .params
                    .iter()
                    .map(|p| p.name.as_str())
                    .chain(e.attrs.iter().map(|s| s.as_str()))
                    .collect();
                let sugs = model::suggest(&up, pool.iter().copied(), 3);
                let msg = if sugs.is_empty() {
                    format!(
                        "'{w}' is not a parameter, attribute, or object name of {} {}",
                        e.verb, e.object
                    )
                } else {
                    format!(
                        "'{w}' is not a parameter of {} {}; did you mean {}?",
                        e.verb,
                        e.object,
                        sugs.join(", ")
                    )
                };
                let fixes = sugs.iter().map(|s| s.to_string()).collect();
                d.push_fixes(
                    lines.range(n.start_byte(), n.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_PARAM,
                    msg,
                    fixes,
                );
                mute_next_group = true;
            }
            Item::Arg(n) => {
                if mute_next_group {
                    mute_next_group = false;
                    continue; // part of the preceding flagged word
                }
                let values = value_nodes(*n);
                let owner = pending.take();
                if let Some((up, _wn)) = owner {
                    if let Some(&pi) = e.by_name.get(&up) {
                        let p = &e.params[pi];
                        check_values(p, &up, &values, src, lines, d);
                        check_obj_ref(&up, e.object, &values, src, lines, defs, d);
                    }
                    if values.is_empty() {
                        d.push(
                            lines.range(n.start_byte(), n.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            format!("Parameter {up} has an empty value list"),
                        );
                    }
                    continue;
                }
                if !seen_name {
                    seen_name = true; // argument right after the object word = object name
                                      // The name is the first value inside this group; keep it
                                      // for the code-13 length check below.
                    name_node = values.first().copied();
                    continue;
                }
                d.push(
                    lines.range(n.start_byte(), n.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_PARAM,
                    "Unexpected argument group: values must follow a parameter name".into(),
                );
            }
            Item::Wild(n) => {
                if !seen_name {
                    seen_name = true;
                    continue; // e.g. DISPLAY QLOCAL (*) or START CHINIT name*
                }
                if matches!(e.verb, "DISPLAY" | "RESET") {
                    continue; // trailing `*` = show every attribute
                }
                d.push(
                    lines.range(n.start_byte(), n.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_PARAM,
                    "Unexpected '*': only one positional object name/pattern is allowed".into(),
                );
            }
            Item::Val(n) => {
                // z/OS NAME=value: the preceding Word was recorded as pending
                let owner = pending.take();
                let up = match owner {
                    Some((u, _)) => u,
                    None => continue,
                };
                if let Some(&pi) = e.by_name.get(&up) {
                    let p = &e.params[pi];
                    let vs = if n.kind() == "value" {
                        vec![*n]
                    } else {
                        value_nodes(*n)
                    };
                    check_values(p, &up, &vs, src, lines, d);
                    check_obj_ref(&up, e.object, &vs, src, lines, defs, d);
                }
            }
            Item::Verb(_) => {}
        }
    }

    // pending parameter word without its group
    if let Some((up, n)) = &pending {
        d.push(
            lines.range(n.start_byte(), n.end_byte()),
            DiagnosticSeverity::ERROR,
            CODE_BAD_VALUE,
            format!("Parameter {up} requires a value in parentheses: {up}( ... )"),
        );
    }

    // missing required parameters. Only report when exactly one parameter is
    // required-and-missing: commands like DEFINE CHANNEL mark several mutually
    // exclusive transport groups (DECNET/LU62/NETBIOS/SPX/TCP ...) as required,
    // and the diagram cannot express that exactly one of them must appear.
    let missing: Vec<&Param> = e
        .params
        .iter()
        .filter(|p| {
            !p.optional
                && !used.contains(&p.name)
                && !matches!(p.name.as_str(), "CMDSCOPE" | "QSGDISP")
        })
        .collect();
    let missing_one = if missing.len() == 1 {
        missing
    } else {
        Vec::new()
    };
    for p in missing_one {
        {
            d.push(
                lines.range(cmd.start_byte(), verb_node.end_byte()),
                DiagnosticSeverity::WARNING,
                CODE_MISSING_REQUIRED,
                format!(
                    "{} {} usually requires parameter {}",
                    e.verb, e.object, p.name
                ),
            );
        }
    }
    if positional_required && !seen_name && e.positional.is_some() && cmd.child_count() > 0 {
        let ow = obj_word.unwrap();
        d.push(
            lines.range(ow.end_byte(), ow.end_byte() + 1),
            DiagnosticSeverity::WARNING,
            CODE_MISSING_NAME,
            format!("{} {} requires the object name", e.verb, e.object),
        );
    }

    // Object-name length (code 13): MQSC caps object names per type — queues,
    // processes, topics, listeners, ... take 48 characters, channels 1024.
    // The 2011 reference states these limits only in prose the extractor
    // cannot capture, so the table is curated in the overlay's `name_max`
    // section (like the object-reference parameter list: a documented
    // heuristic). Pattern names (`DEFINE QLOCAL('fin.*')`) are legal MQSC
    // and exempt; length is measured on the unquoted, unescaped name.
    if positional_required {
        if let (Some(n), Some(max)) = (name_node, m.name_max_len(e.object)) {
            let name = unquote_name(node_text(n, src).trim());
            if !name.contains(['*', '+', '?']) {
                let len = name.chars().count();
                if len > max {
                    let shown: String = name.chars().take(48).collect();
                    let more = if len > 48 { " …" } else { "" };
                    d.push(
                        lines.range(n.start_byte(), n.end_byte()),
                        DiagnosticSeverity::WARNING,
                        CODE_NAME_TOO_LONG,
                        format!(
                            "{} name '{shown}{more}' is {len} characters; the maximum is {max}",
                            e.object
                        ),
                    );
                }
            }
        }
    }
}

/// The literal behind a name token: a quoted string is unwrapped and `''`
/// unescaped to `'`; a bare word is taken as it stands.
pub fn unquote_name(raw: &str) -> String {
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        raw[1..raw.len() - 1].replace("''", "'")
    } else {
        raw.to_string()
    }
}

/// the `value` nodes inside an argument group
fn value_nodes(arg: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![arg];
    while let Some(n) = stack.pop() {
        if n.kind() == "value" {
            out.push(n);
            continue;
        }
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i) {
                stack.push(c);
            }
        }
    }
    out.reverse();
    out
}

fn check_values(
    p: &Param,
    up: &str,
    values: &[Node<'_>],
    src: &[u8],
    lines: &Lines,
    d: &mut Diags,
) {
    for v in values {
        let mut has_wildcard = false;
        let mut has_string = false;
        let mut has_int = false;
        let mut has_word = None;
        let mut vc = v.walk();
        for c in v.named_children(&mut vc) {
            match c.kind() {
                "wildcard" => has_wildcard = true,
                "string" => has_string = true,
                "integer" => {
                    has_int = true;
                    has_word.get_or_insert_with(|| node_text(c, src).to_string());
                }
                "word" => has_word = Some(node_text(c, src).to_string()),
                _ => {}
            }
        }
        let raw = node_text(*v, src);

        match &p.kind {
            ValueKind::Integer => {
                let ok = has_int
                    && !has_string
                    && !has_word
                        .map(|w| !w.chars().all(|c| c.is_ascii_digit()))
                        .unwrap_or(false);
                if !ok {
                    d.push(
                        lines.range(v.start_byte(), v.end_byte()),
                        DiagnosticSeverity::WARNING,
                        CODE_BAD_VALUE,
                        format!("Parameter {up}: value `{raw}` is not an integer"),
                    );
                }
            }
            ValueKind::Enum(vals) => {
                if has_string {
                    d.push(
                        lines.range(v.start_byte(), v.end_byte()),
                        DiagnosticSeverity::WARNING,
                        CODE_BAD_VALUE,
                        format!(
                            "Parameter {up}: value must be one of {}",
                            vals.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
                        ),
                    );
                } else if let Some(w) = &has_word {
                    let uw = w.to_ascii_uppercase();
                    // enum-plus-integer parameters (APPLTYPE, DEFPRTY, ...)
                    // take plain integers as well
                    let int_ok = p.alt_integer
                        && has_int
                        && !has_string
                        && !has_wildcard
                        && w.chars().all(|c| c.is_ascii_digit());
                    if !int_ok && !vals.iter().any(|x| x == &uw) {
                        let sugs = model::suggest(&uw, vals.iter().map(|s| s.as_str()), 3);
                        let list = vals.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
                        let msg = if sugs.is_empty() {
                            format!("Parameter {up}: value `{raw}` must be one of: {list}")
                        } else {
                            format!(
                                "Parameter {up}: value `{raw}` must be one of {list} (did you mean {}?)",
                                sugs.join(", ")
                            )
                        };
                        let fixes = sugs.iter().map(|s| mirror_case(raw, s)).collect();
                        d.push_fixes(
                            lines.range(v.start_byte(), v.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            msg,
                            fixes,
                        );
                    }
                }
            }
            ValueKind::Star => {
                let _ = has_wildcard;
            }
            _ => {}
        }

        // wildcards in values that cannot carry them
        if has_wildcard && p.alt_integer && has_int {
            continue;
        }
        if has_wildcard {
            let allows = matches!(
                p.kind,
                ValueKind::Str | ValueKind::Name | ValueKind::Star | ValueKind::Free
            ) || up == "GENERIC";
            if !allows {
                d.push(
                    lines.range(v.start_byte(), v.end_byte()),
                    DiagnosticSeverity::WARNING,
                    CODE_WILDCARD,
                    format!("Parameter {up}: wildcards are not valid in this value"),
                );
            }
        }
    }
}

fn check_question_wildcards(tree: &Tree, src: &[u8], lines: &Lines, m: &Model, d: &mut Diags) {
    let root = tree.root_node();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "command" {
            let verb = n
                .child_by_field_name("verb")
                .map(|v| node_text(v, src))
                .unwrap_or("");
            let is_display = m.canon_verb(verb).map(|c| c == "DISPLAY").unwrap_or(false);
            if !is_display {
                let mut s2 = vec![n];
                while let Some(x) = s2.pop() {
                    if x.kind() == "wildcard" && node_text(x, src) == "?" {
                        d.push(
                            lines.range(x.start_byte(), x.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_WILDCARD,
                            "The '?' wildcard is valid only in DISPLAY command patterns".into(),
                        );
                    }
                    for i in 0..x.named_child_count() {
                        if let Some(c) = x.named_child(i) {
                            s2.push(c);
                        }
                    }
                }
            }
        }
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i) {
                stack.push(c);
            }
        }
    }
}

// ------------------------------------------------------------------- hover

fn command_context<'t>(
    cmd: Node<'t>,
    src: &'t [u8],
    m: &'t Model,
) -> Option<(&'t str, Option<&'t str>, &'t model::Command)> {
    let items = command_items(cmd);
    let verb = match items.first() {
        Some(Item::Verb(n)) => node_text(*n, src),
        _ => return None,
    };
    let obj = match items.get(1) {
        Some(Item::Word(n)) => Some(node_text(*n, src)),
        _ => None,
    };
    let e = m.resolve(verb, obj).map(|i| m.entry(i))?;
    Some((verb, obj, e))
}

pub fn hover(
    text: &str,
    tree: &Tree,
    m: &Model,
    pos: Position,
    ext_defs: &ExternalDefs,
) -> Option<Hover> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let off = lines.offset(pos);
    let node = node_at(tree.root_node(), off);
    if node.kind() == "comment" {
        return None;
    }
    let cmd = ancestor_of(node, "command")?;
    let (verb, _obj, e) = command_context(cmd, src, m)?;

    let mk = |value: String| Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(lines.range(node.start_byte(), node.end_byte())),
    };

    // Object name itself: definition/usage summary (T-18). Checked before
    // the command card and parameter-doc matches below — a name token is
    // more specific than the generic parameter-word lookup, which would
    // otherwise misread names that collide with attribute words.
    let idx = object_index(text, tree, m);
    if let Some(site) = idx.site_at(pos) {
        let uses = idx
            .sites_of(&site.name)
            .filter(|s| s.defines.is_none())
            .count();
        let defs = idx.definitions_of(&site.name);
        let md = if defs.is_empty() {
            // not defined in this document — is it DEFINEd in another open
            // one? (T-17)
            match ext_defs.get(&site.name) {
                Some(ext) if !ext.is_empty() => {
                    let types: Vec<&str> = ext.iter().map(|(_, _, t)| t.as_str()).collect();
                    format!(
                        "**{} `{}`** — DEFINEd at line {} of `{}` (another open document){}\n\n\
                         {} {} in this script.",
                        types.join("/"),
                        site.name,
                        ext[0].1 + 1,
                        ext[0].0,
                        if ext.len() > 1 {
                            format!(" — {} DEFINE locations there", ext.len())
                        } else {
                            String::new()
                        },
                        uses,
                        if uses == 1 { "use" } else { "uses" },
                    )
                }
                _ => format!(
                    "**`{}`** — an object name, but not DEFINEd in this script.\n\n\
                     It must already exist in the queue manager.",
                    site.name
                ),
            }
        } else {
            let mut types: Vec<&str> = Vec::new();
            for d in &defs {
                if let Some(t) = &d.defines {
                    if !types.contains(&t.as_str()) {
                        types.push(t.as_str());
                    }
                }
            }
            let where_ = if site.defines.is_some() {
                "**this is the definition**".to_string()
            } else {
                format!("DEFINEd at line {}", defs[0].range.start.line + 1)
            };
            let more = if defs.len() > 1 {
                format!(" — {} DEFINE locations", defs.len())
            } else {
                String::new()
            };
            format!(
                "**{} `{}`** — {}\n\n{} {} in this script{}",
                types.join("/"),
                site.name,
                where_,
                uses,
                if uses == 1 { "use" } else { "uses" },
                more
            )
        };
        return Some(mk(md));
    }

    let items = command_items(cmd);
    let obj_node = match items.get(1) {
        Some(Item::Word(n)) => Some(*n),
        _ => None,
    };

    // verb or object word -> the command card
    if node.kind() == "verb" || obj_node.map(|n| n.id() == node.id()).unwrap_or(false) {
        let mut md = format!("**MQSC: {} {}**\n\n", e.verb, e.object);
        if e.description.is_empty() {
            md.push_str("_No description in the reference._");
        } else {
            md.push_str(&e.description);
        }
        if !e.synonyms.is_empty() {
            md.push_str(&format!("\n\n_Synonym:_ `{}`", e.synonyms.join(" ")));
        }
        if !e.attrs.is_empty() {
            md.push_str(&format!(
                "\n\n_Displayable attributes ({})_: `{}`{}",
                e.attrs.len(),
                e.attrs
                    .iter()
                    .take(14)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
                if e.attrs.len() > 14 { " …" } else { "" }
            ));
        }
        return Some(mk(md));
    }

    // parameter word or value inside its group
    let pname = if node.kind() == "word" {
        node_text(node, src).to_ascii_uppercase()
    } else if let Some(arg) = ancestor_of(node, "argument") {
        let owner = preceding_word(cmd, arg, src);
        owner.map(|(w, _)| w).unwrap_or_default()
    } else {
        return None;
    };
    if pname.is_empty() {
        return None;
    }

    if let Some(&pi) = e.by_name.get(&pname) {
        let p = &e.params[pi];
        let mut md = format!("**{} {} — `{pname}`**\n\n", e.verb, e.object);
        match &p.kind {
            ValueKind::Flag => md.push_str("_Keyword; no value._\n\n"),
            ValueKind::Enum(vals) => {
                md.push_str(&format!(
                    "*Values:* `{}`{}\n\n",
                    vals.join("`, `"),
                    if vals.len() > 12 { " …" } else { "" }
                ));
            }
            ValueKind::Integer => md.push_str("*Value:* integer\n\n"),
            ValueKind::Str => md.push_str("*Value:* string\n\n"),
            ValueKind::Name => md.push_str("*Value:* a name token\n\n"),
            ValueKind::Star => md.push_str("*Value:* `*` or a name\n\n"),
            ValueKind::Free => {}
        }
        if let Some(dv) = &p.default {
            md.push_str(&format!("*Default:* `{dv}`\n\n"));
        }
        md.push_str(if p.optional {
            "_Optional._\n\n"
        } else {
            "_Required by the reference diagram._\n\n"
        });
        let doc = doc_for(m, p, e.object);
        if !doc.is_empty() {
            md.push_str(&doc);
        }
        for f in &p.footnotes {
            md.push_str(&format!("\n\n> {f}"));
        }
        return Some(mk(md));
    }
    if e.attr_set.contains(&pname) {
        let doc = m.param_doc(&pname, e.object).unwrap_or("");
        let mut md = format!(
            "**Displayable attribute — `{pname}`**\n\n_{} {}_\n\n",
            verb, e.object
        );
        if !doc.is_empty() {
            md.push_str(doc);
        }
        return Some(mk(md));
    }
    None
}

fn doc_for(m: &Model, p: &Param, obj: &str) -> String {
    if !p.description.is_empty() {
        p.description.clone()
    } else {
        m.param_doc(&p.name, obj).unwrap_or("").to_string()
    }
}

fn preceding_word<'t>(
    cmd: Node<'t>,
    before: Node<'_>,
    src: &'t [u8],
) -> Option<(String, Node<'t>)> {
    let mut best = None;
    let mut cursor = cmd.walk();
    for c in cmd.named_children(&mut cursor) {
        if c.kind() == "word" && c.end_byte() <= before.start_byte() {
            best = Some((node_text(c, src).to_ascii_uppercase(), c));
        }
    }
    best
}

// --------------------------------------------------------------- completion

pub fn completions(
    text: &str,
    tree: &Tree,
    m: &Model,
    pos: Position,
    snippet: bool,
    extra_defs: &Definitions,
) -> Vec<CompletionItem> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let off = lines.offset(pos);
    let line = lines.line_start(pos.line as usize);
    let end = lines
        .starts
        .get(pos.line as usize + 1)
        .copied()
        .unwrap_or(text.len());
    let line_text = &text[line..end.min(text.len())];
    let col = off.saturating_sub(line).min(line_text.len());
    let before = &line_text[..col];
    let frag = word_fragment(before);

    // inside an argument group?
    if let Some(arg) = argument_containing(tree, off) {
        if let Some(cmd) = ancestor_of(arg, "command") {
            if let Some((w, _)) = preceding_word(cmd, arg, src) {
                if let Some((_, _, e)) = command_context(cmd, src, m) {
                    if let Some(&pi) = e.by_name.get(&w) {
                        let p = &e.params[pi];
                        let mut out = value_completions(p, &frag);
                        // queue/object reference parameter: also offer names
                        // this very script already DEFINEd (plus the other
                        // open documents, T-17)
                        if let Some(r) = object_ref_param(&w, e.object) {
                            let mut defs = collect_definitions(tree, src, m);
                            defs.add_external(extra_defs);
                            out.extend(name_items(&names_for(&defs, Some(r), e.object)));
                        }
                        // (value_completions already filtered its items;
                        // re-filtering both sets together is harmless)
                        filter_prefix(&mut out, &frag);
                        return out;
                    }
                }
            }
            // The argument is either the object-name group (directly after the
            // object word) or belongs to an unknown word: MQSC object names and
            // most keyword values are free tokens, so offer the generic pattern
            // plus everything already DEFINEd for a compatible object type.
            let mut out = vec![CompletionItem {
                label: "*".into(),
                kind: Some(CompletionItemKind::TEXT),
                detail: Some("generic name pattern".into()),
                insert_text: Some("*".into()),
                sort_text: Some("0*".into()),
                ..Default::default()
            }];
            if let Some((_, _, e)) = command_context(cmd, src, m) {
                out.extend(defined_name_items(tree, src, m, extra_defs, e.object));
            }
            filter_prefix(&mut out, &frag);
            return out;
        }
    }

    let cmd_opt = command_on_line(tree, off, line);
    let before_trimmed = before.trim_start();

    // The cursor may sit on a continuation line: tree-sitter keeps the
    // whole `+` chain in one command node, so a node that started before
    // the cursor's line means this line is not "fresh" even when nothing
    // is typed on it yet — a verb could not syntactically continue it.
    let on_continuation = cmd_opt.map_or(false, |c| c.start_byte() < line);

    // fresh line: suggest verbs
    if cmd_opt.is_none() || (before_trimmed.is_empty() && !on_continuation) {
        let mut out = verb_items(m);
        filter_prefix(&mut out, &frag);
        return out;
    }

    let cmd = cmd_opt.unwrap();
    let items = command_items(cmd);
    let verb_node = match items.first() {
        Some(Item::Verb(n)) => *n,
        _ => return vec![],
    };
    let verb = node_text(verb_node, src);

    // Byte where the token under the cursor starts. `frag` is empty when
    // the cursor sits after whitespace, so "next word" starts at the cursor.
    let frag_start = off - frag.len();

    // still inside the verb token ("DEF|" / "DEFINE|"): verbs are what a
    // completion here can complete.
    if verb_node.start_byte() >= frag_start {
        let mut out = verb_items(m);
        filter_prefix(&mut out, &frag);
        return out;
    }

    // How many items after the verb are fully to the left of the token
    // under the cursor: that many command slots (object word, object name,
    // parameters) are already committed, so the cursor completes the next.
    let item_end = |it: &Item| match it {
        Item::Verb(n) | Item::Word(n) | Item::Arg(n) | Item::Wild(n) | Item::Val(n) => n.end_byte(),
    };
    let committed = items[1..]
        .iter()
        .filter(|it| item_end(it) <= frag_start)
        .count();

    // object-type slot: the only valid next words are the object types
    // this verb accepts ("DEFINE " -> QLOCAL, CHANNEL, ...). A leading word
    // that is no verb at all cannot have object types; recover by offering
    // verbs like a fresh line.
    if committed == 0 {
        let mut out = if m.canon_verb(verb).is_some() {
            object_type_items(m, verb)
        } else {
            verb_items(m)
        };
        filter_prefix(&mut out, &frag);
        return out;
    }

    let (_verb, _obj, e) = match command_context(cmd, src, m) {
        Some(x) => x,
        None => return vec![],
    };

    // object-name slot: a command with a positional name takes that bare
    // word next (`DEFINE QLOCAL <name>`), so offer the names this script
    // (and the other open documents, T-17) already DEFINEd for a compatible
    // object type — a parenthesised name is handled by the argument branch
    // above. The verbs whose analyzer enforces a real name (MISSING_NAME)
    // do not accept a '*' pattern here.
    if committed == 1 && e.positional.is_some() {
        let mut out = defined_name_items(tree, src, m, extra_defs, e.object);
        if !matches!(e.verb, "DEFINE" | "ALTER" | "DELETE") {
            out.push(CompletionItem {
                label: "*".into(),
                kind: Some(CompletionItemKind::TEXT),
                detail: Some("generic name pattern".into()),
                insert_text: Some("*".into()),
                sort_text: Some("0*".into()),
                ..Default::default()
            });
        }
        filter_prefix(&mut out, &frag);
        return out;
    }

    // used parameters: words fully before the cursor. The object-type word
    // (slot 0) and the positional object name (slot 1) are never parameters
    // yet can collide with parameter/attribute labels — counting the object
    // word here hid the SUB parameter from `DISPLAY SUB <name> ` (the one
    // such label collision the model contains).
    let mut used: HashSet<String> = HashSet::new();
    for (slot, it) in items[1..].iter().enumerate() {
        if slot == 0 || (slot == 1 && e.positional.is_some()) {
            continue;
        }
        if let Item::Word(n) = it {
            if n.end_byte() < off {
                used.insert(node_text(*n, src).to_ascii_uppercase());
            }
        }
    }

    let is_pos_verb = matches!(e.verb, "DISPLAY" | "RESET");
    let mut out: Vec<CompletionItem> = Vec::new();

    for p in &e.params {
        if used.contains(&p.name) {
            continue;
        }
        let (insert, kind, fmt) = match &p.kind {
            ValueKind::Flag => (
                p.name.clone(),
                CompletionItemKind::KEYWORD,
                InsertTextFormat::PLAIN_TEXT,
            ),
            _ if !snippet => (
                format!("{}(", p.name),
                CompletionItemKind::PROPERTY,
                InsertTextFormat::PLAIN_TEXT,
            ),
            _ => (
                format!("{}({})", p.name, snippet_body(p)),
                CompletionItemKind::PROPERTY,
                InsertTextFormat::SNIPPET,
            ),
        };
        let detail = match &p.kind {
            ValueKind::Flag => "keyword".to_string(),
            ValueKind::Enum(vals) => {
                format!(
                    "values: {}",
                    vals.iter().take(4).cloned().collect::<Vec<_>>().join("/")
                )
            }
            ValueKind::Integer => "integer".into(),
            ValueKind::Str => "string".into(),
            ValueKind::Name => "name".into(),
            ValueKind::Star => "* or name".into(),
            ValueKind::Free => String::new(),
        };
        let doc = doc_for(m, p, e.object);
        out.push(CompletionItem {
            label: p.name.clone(),
            kind: Some(kind),
            detail: Some(detail),
            documentation: Some(lsp::Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc,
            })),
            insert_text: Some(insert),
            insert_text_format: Some(fmt),
            sort_text: Some(format!("{:01}{}", if p.optional { 1 } else { 0 }, p.name)),
            ..Default::default()
        });
    }

    if is_pos_verb {
        for a in &e.attrs {
            if used.contains(a) {
                continue;
            }
            out.push(CompletionItem {
                label: a.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some("displayable attribute".into()),
                insert_text: Some(a.clone()),
                sort_text: Some(format!("2{a}")),
                ..Default::default()
            });
        }
    }

    if e.verb == "DISPLAY" || e.attrs.is_empty() {
        out.push(CompletionItem {
            label: "*".into(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some("all objects / generic name".into()),
            insert_text: Some("*".into()),
            sort_text: Some("0*".into()),
            ..Default::default()
        });
    }

    filter_prefix(&mut out, &frag);
    out
}

/// Completion items for `(object type, name)` pairs DEFINEd in this script.
fn name_items(pairs: &[(&str, &str)]) -> Vec<CompletionItem> {
    pairs
        .iter()
        .map(|(obj, n)| CompletionItem {
            label: n.to_string(),
            kind: Some(CompletionItemKind::REFERENCE),
            detail: Some(format!("DEFINEd in this script as {obj}")),
            sort_text: Some(format!("1{n}")),
            ..Default::default()
        })
        .collect()
}

/// Completion items for the model's verbs — a fresh line, the verb token
/// still under the cursor, or a leading word that is no verb at all.
fn verb_items(m: &Model) -> Vec<CompletionItem> {
    m.all_verbs()
        .iter()
        .map(|v| CompletionItem {
            label: v.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("MQSC command".into()),
            filter_text: Some(v.to_string()),
            sort_text: Some(format!("{:03}", v.chars().count())),
            ..Default::default()
        })
        .collect()
}

/// The object types `verb` accepts — the logical next words once the verb
/// is committed (`DEFINE ` -> CHANNEL, QLOCAL, QALIAS, ...).
fn object_type_items(m: &Model, verb: &str) -> Vec<CompletionItem> {
    m.objects_for(verb)
        .into_iter()
        .map(|o| CompletionItem {
            label: o.to_string(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(format!("{} {o}", verb.to_ascii_uppercase())),
            ..Default::default()
        })
        .collect()
}

/// Names DEFINEd in this document (plus the other open ones, T-17) for an
/// object type compatible with `cmd_obj` — the candidates for the
/// object-name slot and the name group.
fn defined_name_items(
    tree: &Tree,
    src: &[u8],
    m: &Model,
    extra_defs: &Definitions,
    cmd_obj: &str,
) -> Vec<CompletionItem> {
    let mut defs = collect_definitions(tree, src, m);
    defs.add_external(extra_defs);
    let pairs: Vec<(&str, &str)> = defs
        .by_obj
        .iter()
        .filter(|(obj, _)| name_group_matches(obj, cmd_obj))
        .flat_map(|(obj, ns)| ns.iter().map(|n| (obj.as_str(), n.as_str())))
        .collect();
    name_items(&pairs)
}

/// Names typed into a command's object-name group: same type, or any
/// queue-like type when either side is queue-like (DISPLAY QUEUE and the
/// QLOCAL/QMODEL/... aliases name the same objects).
fn name_group_matches(def_obj: &str, cmd_obj: &str) -> bool {
    def_obj == cmd_obj || (def_obj.starts_with('Q') && cmd_obj.starts_with('Q'))
}

/// Snippet placeholder for a parameter's value: `NAME(${1:...})` with the
/// cursor inside the group (and a choice list for short enums). MQSC words
/// never contain the snippet metacharacters `$`, `}` or `|`.
fn snippet_body(p: &Param) -> String {
    match &p.kind {
        ValueKind::Enum(vals)
            if !vals.is_empty() && vals.len() <= 6 && !vals.iter().any(|v| v.contains(',')) =>
        {
            format!("${{1|{}|}}", vals.join(","))
        }
        ValueKind::Enum(_) | ValueKind::Str | ValueKind::Free => "${1:value}".into(),
        ValueKind::Integer => "${1:n}".into(),
        ValueKind::Name => "${1:name}".into(),
        ValueKind::Star => "${1:*}".into(),
        ValueKind::Flag => String::new(),
    }
}

fn value_completions(p: &Param, frag: &str) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    match &p.kind {
        ValueKind::Enum(vals) => {
            for v in vals {
                out.push(CompletionItem {
                    label: v.clone(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some("value".into()),
                    sort_text: Some(if p.default.as_deref() == Some(v.as_str()) {
                        format!("0{v}")
                    } else {
                        format!("1{v}")
                    }),
                    ..Default::default()
                });
            }
        }
        ValueKind::Star => out.push(CompletionItem {
            label: "*".into(),
            kind: Some(CompletionItemKind::TEXT),
            ..Default::default()
        }),
        _ => {}
    }
    if matches!(p.kind, ValueKind::Name | ValueKind::Str) {
        out.push(CompletionItem {
            label: "*".into(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some("pattern".into()),
            ..Default::default()
        });
    }
    filter_prefix(&mut out, frag);
    out
}

fn word_fragment(before: &str) -> String {
    // The fragment is the trailing run of word-constituent characters at the
    // cursor. If the cursor sits after whitespace (or punctuation), there is no
    // active fragment and everything should be offered.
    if before.chars().next_back().map_or(true, |c| {
        !(c.is_alphanumeric() || matches!(c, '_' | '#' | '@' | '%' | '/' | '.' | '-'))
    }) {
        return String::new();
    }
    let t = before;
    let end = t
        .char_indices()
        .rfind(|(_, c)| {
            !(c.is_alphanumeric() || matches!(c, '_' | '#' | '@' | '%' | '/' | '.' | '-'))
        })
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    t[end..].to_string()
}

fn filter_prefix(items: &mut Vec<CompletionItem>, frag: &str) {
    if frag.is_empty() {
        return;
    }
    let f = frag.to_ascii_uppercase();
    items.retain(|i| i.label.to_ascii_uppercase().starts_with(&f));
}

fn command_containing(tree: &Tree, off: usize) -> Option<Node<'_>> {
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "command" && n.start_byte() <= off && off <= n.end_byte() {
            return Some(n);
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if c.start_byte() <= off && off <= c.end_byte() {
                    stack.push(c);
                }
            }
        }
    }
    None
}

/// The command the cursor completes: the one containing the cursor, or —
/// when the cursor sits in trailing whitespace, which tree-sitter keeps
/// out of the node, so after `DEFINE ` the node ends at the verb — the
/// command starting on the cursor's line before it.
fn command_on_line(tree: &Tree, off: usize, line_start: usize) -> Option<Node<'_>> {
    command_containing(tree, off).or_else(|| {
        let mut best: Option<Node> = None;
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if n.kind() == "command"
                && n.start_byte() >= line_start
                && n.start_byte() < off
                && best.map_or(true, |b| n.start_byte() > b.start_byte())
            {
                best = Some(n);
            }
            for i in 0..n.child_count() {
                if let Some(c) = n.child(i) {
                    if c.start_byte() < off {
                        stack.push(c);
                    }
                }
            }
        }
        best
    })
}

fn argument_containing(tree: &Tree, off: usize) -> Option<Node<'_>> {
    let mut best = None;
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "argument" && n.start_byte() < off && off <= n.end_byte() {
            best = Some(n);
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if c.start_byte() <= off && off <= c.end_byte() {
                    stack.push(c);
                }
            }
        }
    }
    best
}

// ------------------------------------------------------------ symbols/folding

// lsp-types 0.94 still requires the deprecated `deprecated` field in the
// DocumentSymbol struct literal; setting `tags` does not remove it.
#[allow(deprecated)]
pub fn symbols(text: &str, tree: &Tree) -> Vec<DocumentSymbol> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    for cmd in tree.root_node().named_children(&mut cursor) {
        if cmd.kind() != "command" {
            continue;
        }
        let items = command_items(cmd);
        // Informative label: "DEFINE QLOCAL (A)" — verb, object type and the
        // object name; the parameter groups move to `detail`. A verb-only
        // label made the outline a useless wall of "DEFINE".
        let text_of = |it: &Item| -> String {
            let n = match it {
                Item::Verb(n) | Item::Word(n) => *n,
                Item::Arg(n) | Item::Wild(n) | Item::Val(n) => *n,
            };
            node_text(n, src).to_string()
        };
        let mut label = match items.len() {
            0 => continue,
            1 => text_of(&items[0]),
            2 => format!("{} {}", text_of(&items[0]), text_of(&items[1])),
            _ => {
                let name: String = text_of(&items[2]).chars().take(32).collect();
                format!(
                    "{} {} {}",
                    text_of(&items[0]),
                    text_of(&items[1]),
                    name.trim()
                )
            }
        };
        label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        let detail: String = items
            .iter()
            .skip(3.min(items.len()))
            .map(text_of)
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let range = lines.range(cmd.start_byte(), cmd.end_byte());
        out.push(DocumentSymbol {
            name: label,
            detail: Some(detail.trim().chars().take(70).collect()),
            kind: SymbolKind::METHOD,
            range,
            selection_range: range,
            children: None,
            deprecated: None,
            tags: None,
        });
    }
    out
}

pub fn folding(text: &str, tree: &Tree) -> Vec<FoldingRange> {
    let lines = Lines::new(text);
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();

    let push = |out: &mut Vec<FoldingRange>, a: u32, b: u32, kind: FoldingRangeKind| {
        if b > a {
            out.push(FoldingRange {
                start_line: a,
                start_character: None,
                end_line: b,
                end_character: None,
                kind: Some(kind),
                collapsed_text: None,
            });
        }
    };

    // Adjacent `*` comment lines form one comment block for folding purposes.
    let mut comment_run: Option<(u32, u32)> = None;
    for node in tree.root_node().named_children(&mut cursor) {
        let a = lines.pos(node.start_byte()).line;
        let b = lines.pos(node.end_byte().saturating_sub(1)).line;
        match node.kind() {
            "command" => {
                if let Some((s, e)) = comment_run.take() {
                    push(&mut out, s, e, FoldingRangeKind::Comment);
                }
                push(&mut out, a, b, FoldingRangeKind::Region);
            }
            "comment" => match comment_run {
                Some((s, e)) if a == e + 1 => comment_run = Some((s, b)),
                Some((s, e)) => {
                    push(&mut out, s, e, FoldingRangeKind::Comment);
                    comment_run = Some((a, b));
                }
                None => comment_run = Some((a, b)),
            },
            _ => {
                if let Some((s, e)) = comment_run.take() {
                    push(&mut out, s, e, FoldingRangeKind::Comment);
                }
            }
        }
    }
    if let Some((s, e)) = comment_run.take() {
        push(&mut out, s, e, FoldingRangeKind::Comment);
    }
    out
}

pub fn highlights(text: &str, tree: &Tree, pos: Position) -> Vec<DocumentHighlight> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let off = lines.offset(pos);
    let node = node_at(tree.root_node(), off);
    if node.kind() != "word" && node.kind() != "string" {
        return vec![];
    }
    let wtxt = node_text(node, src).to_ascii_uppercase();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if (n.kind() == "word" || n.kind() == "string")
            && node_text(n, src).to_ascii_uppercase() == wtxt
        {
            out.push(DocumentHighlight {
                range: lines.range(n.start_byte(), n.end_byte()),
                kind: Some(DocumentHighlightKind::TEXT),
            });
        }
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i) {
                stack.push(c);
            }
        }
    }
    out
}

// ---------------------------------------------------------- semantic tokens

pub const TOKEN_TYPES: &[&str] = &[
    "keyword",    // 0 command verb
    "type",       // 1 object type
    "property",   // 2 parameter name
    "variable",   // 3 object name / positional value
    "enumMember", // 4 enumerated keyword value
    "string",     // 5 quoted string
    "number",     // 6 integer
    "comment",    // 7 comment
    "regexp",     // 8 wildcard
];

pub const TOKEN_MODIFIERS: &[&str] = &[
    "default", // 0 enum value equal to the parameter's reference default
];

/// Absolute-position tokens: (line, col, utf16-len, type, modifier bits).
fn raw_tokens(text: &str, tree: &Tree) -> Vec<(usize, usize, usize, u32, u32)> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut raw: Vec<(usize, usize, usize, u32, u32)> = Vec::new();

    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        match node.kind() {
            "comment" => {
                emit(node, src, &lines, 7, 0, &mut raw);
                continue;
            }
            "command" => {}
            _ => continue,
        }
        let items = command_items(node);
        // parameter words: any Word followed (in items order) by an Arg, plus
        // bare flag/attr words resolved against the model
        let cmd_ctx = command_context(node, src, Model::load());
        let mut obj_done = false;
        // reference default of the parameter owning the group we are in
        let mut owner_default: Option<String> = None;
        for (i, it) in items.iter().enumerate() {
            match it {
                Item::Verb(n) => emit(*n, src, &lines, 0, 0, &mut raw),
                Item::Word(n) => {
                    let up = node_text(*n, src).to_ascii_uppercase();
                    if !obj_done && i == 1 {
                        obj_done = true;
                        owner_default = None;
                        emit(*n, src, &lines, 1, 0, &mut raw); // object type
                        continue;
                    }
                    let e = cmd_ctx.map(|(_, _, e)| e);
                    let is_param = match e {
                        Some(e) => {
                            e.by_name.contains_key(&up)
                                || e.attr_set.contains(&up)
                                || matches!(items.get(i + 1), Some(Item::Arg(_)))
                        }
                        None => matches!(items.get(i + 1), Some(Item::Arg(_))),
                    };
                    owner_default = match e {
                        Some(e) => e
                            .by_name
                            .get(&up)
                            .map(|&pi| e.params[pi].default.clone())
                            .unwrap_or_default(),
                        None => None,
                    };
                    emit(*n, src, &lines, if is_param { 2 } else { 3 }, 0, &mut raw);
                }
                Item::Arg(n) => {
                    walk_group(*n, src, &lines, owner_default.as_deref(), &mut raw);
                    owner_default = None;
                }
                Item::Wild(n) => emit(*n, src, &lines, 8, 0, &mut raw),
                Item::Val(n) => {
                    // z/OS NAME=value: the owner default was set by the word
                    walk_group(*n, src, &lines, owner_default.as_deref(), &mut raw);
                    owner_default = None;
                }
            }
        }
    }

    raw.sort_by_key(|(l, c, ..)| (*l, *c));
    raw.dedup_by_key(|(l, c, ..)| (*l, *c));
    raw
}

/// One `(...)` group (or `=` value): value tokens, marking enum words that
/// equal the parameter's reference default with the `default` modifier.
fn walk_group(
    g: Node<'_>,
    src: &[u8],
    lines: &Lines,
    owner_default: Option<&str>,
    raw: &mut Vec<(usize, usize, usize, u32, u32)>,
) {
    let mut st = vec![g];
    while let Some(x) = st.pop() {
        match x.kind() {
            "string" => emit(x, src, lines, 5, 0, raw),
            "integer" => emit(x, src, lines, 6, 0, raw),
            "wildcard" => emit(x, src, lines, 8, 0, raw),
            "word" => {
                let mods = match owner_default {
                    Some(dv) if node_text(x, src).eq_ignore_ascii_case(dv) => 1,
                    _ => 0,
                };
                emit(x, src, lines, 4, mods, raw);
            }
            _ => {}
        }
        for k in 0..x.named_child_count() {
            if let Some(c) = x.named_child(k) {
                st.push(c);
            }
        }
    }
}

pub fn semantic_tokens(text: &str, tree: &Tree) -> Vec<SemanticToken> {
    encode_tokens(raw_tokens(text, tree), 0, 0)
}

/// Tokens starting inside `range`, delta-encoded against the range start
/// (per the LSP `semanticTokens/range` convention).
pub fn semantic_tokens_range(text: &str, tree: &Tree, range: lsp::Range) -> Vec<SemanticToken> {
    let (sl, sc) = (range.start.line as usize, range.start.character as usize);
    let (el, ec) = (range.end.line as usize, range.end.character as usize);
    let raw: Vec<_> = raw_tokens(text, tree)
        .into_iter()
        .filter(|(l, c, len, ..)| {
            (*l > sl || (*l == sl && *c >= sc)) && (*l < el || (*l == el && *c + *len <= ec))
        })
        .collect();
    encode_tokens(raw, range.start.line, range.start.character)
}

fn encode_tokens(
    raw: Vec<(usize, usize, usize, u32, u32)>,
    base_line: u32,
    base_char: u32,
) -> Vec<SemanticToken> {
    // delta-encode
    let mut prev_line = base_line as usize;
    let mut prev_char = base_char as usize;
    let mut out = Vec::with_capacity(raw.len());
    for (line, col, len, ty, mods) in raw {
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 {
            col - prev_char
        } else {
            col
        };
        out.push(SemanticToken {
            delta_line: delta_line as u32,
            delta_start: delta_start as u32,
            length: len as u32,
            token_type: ty,
            token_modifiers_bitset: mods,
        });
        prev_line = line;
        prev_char = col;
    }
    out
}

fn emit(
    n: Node<'_>,
    src: &[u8],
    lines: &Lines,
    ty: u32,
    mods: u32,
    out: &mut Vec<(usize, usize, usize, u32, u32)>,
) {
    // A node can span a `+`/`-` continuation and therefore contain newlines.
    // LSP semantic tokens must stay inside their line, so emit one token per
    // line segment of the node text (continuation lines start at column 0).
    let text = node_text(n, src).trim_end_matches(['\n', '\r']);
    let p = lines.pos(n.start_byte());
    let mut line = p.line as usize;
    let mut col = p.character as usize;
    for seg in text.split('\n') {
        let len = utf16_len(seg.trim_end_matches('\r'));
        if len > 0 {
            out.push((line, col, len, ty, mods));
        }
        line += 1;
        col = 0;
    }
}

// ---------------------------------------------------------------- tests (T-10)

#[cfg(test)]
mod tests {
    use super::*;

    /// Analyze a standalone snippet and return the diagnostic codes emitted.
    /// Every MQSC behavior fixture goes through here — one snippet, one
    /// isolated parse, so a broken line cannot derail neighboring cases.
    fn codes(text: &str) -> Vec<i32> {
        let m = Model::load();
        let tree = parse(text);
        let uri = Url::parse("file:///fixture.mqsc").unwrap();
        analyze(text, &tree, m, &uri, &Definitions::default())
            .into_iter()
            .filter_map(|d| match d.code {
                Some(lsp::NumberOrString::Number(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn has(text: &str, code: i32) -> bool {
        codes(text).contains(&code)
    }

    // ------------------------------------------------------------ text index

    #[test]
    fn lines_ascii_and_crlf_positions() {
        let l = Lines::new("AB\r\nCD\nEF");
        assert_eq!(l.pos(0), Position::new(0, 0));
        assert_eq!(l.pos(4), Position::new(1, 0)); // after "\r\n"
        assert_eq!(l.pos(5), Position::new(1, 1));
        assert_eq!(l.pos(7), Position::new(2, 0));
        assert_eq!(l.line_start(1), 4);
        assert_eq!(
            l.range(4, 6),
            Range::new(Position::new(1, 0), Position::new(1, 2))
        );
    }

    #[test]
    fn lines_counts_utf16_columns() {
        // 'a' = 1 unit, U+1F600 = 2 UTF-16 units (4 UTF-8 bytes), 'b' = 1
        let l = Lines::new("a\u{1F600}b");
        assert_eq!(l.pos(1), Position::new(0, 1));
        assert_eq!(l.pos(5), Position::new(0, 3));
        // LSP positions inside a surrogate pair snap to its edges
        assert_eq!(l.offset(Position::new(0, 3)), 5);
        assert_eq!(l.offset(Position::new(0, 4)), 6);
    }

    #[test]
    fn pos_snaps_inward_from_non_char_boundaries() {
        // REVIEW.md C1: callers do byte arithmetic (`s + 1`, `end + 1`); an
        // offset inside a multi-byte char used to panic the UTF-16 column
        // slice. It must snap back to the char's start instead.
        let l = Lines::new("éмир");
        assert_eq!(l.pos(1), Position::new(0, 0)); // inside 'é'
        assert_eq!(l.pos(3), Position::new(0, 1)); // inside 'м'
        assert_eq!(l.pos(2), Position::new(0, 1)); // clean boundary
        assert_eq!(
            l.range(0, 1), // the shape the 2048-char check builds
            Range::new(Position::new(0, 0), Position::new(0, 0))
        );
    }

    #[test]
    fn analyze_survives_multibyte_at_panic_sites() {
        // The exact repro of REVIEW.md C1: a >2048-char line whose first
        // character is multi-byte lints as a lexical warning, not a panic.
        let long = "é".to_string() + &"x".repeat(2100) + "\n";
        assert!(codes(&long).contains(&CODE_LEXICAL));
        // Adjacent shapes for the other `+1` call sites (error nodes,
        // missing-name): analyze must run to completion on all of them.
        for snippet in [
            "ézz(((\n",
            "DEFINE QLOCAL(名前) DESCR('ä')\n",
            "ALTER QLOCAL(AB)中PARAM(1)\n",
            "DEFINE QLOCAL(Ä)\n",
        ] {
            let _ = codes(snippet); // must not panic
        }
    }

    #[test]
    fn semantic_tokens_never_span_lines() {
        // A string with a `+` continuation is one node covering two lines;
        // emit() must split it or clients get out-of-line lengths.
        let text = "DEFINE QLOCAL(A) DESCR('first +\n+ second') REPLACE\n";
        let tree = parse(text);
        let line_lens: Vec<usize> = text.split('\n').map(utf16_len).collect();
        let mut line = 0usize;
        let mut col = 0usize;
        for t in semantic_tokens(text, &tree) {
            line += t.delta_line as usize;
            col = if t.delta_line == 0 {
                col + t.delta_start as usize
            } else {
                t.delta_start as usize
            };
            assert!(
                col + t.length as usize <= line_lens[line],
                "token {line}:{col} len {} crosses its line (line is {} wide)",
                t.length,
                line_lens[line]
            );
        }
    }

    #[test]
    fn lines_offset_roundtrips_ascii() {
        let text = "DEFINE QLOCAL(A)\nDISPLAY QLOCAL(A)\n";
        let l = Lines::new(text);
        for off in 0..=text.len() {
            assert_eq!(l.offset(l.pos(off)), off, "roundtrip at byte {off}");
        }
    }

    #[test]
    fn lines_clamps_out_of_range_positions() {
        let l = Lines::new("AB\nCD");
        assert_eq!(l.offset(Position::new(99, 99)), text_len("AB\nCD"));
        assert_eq!(l.pos(usize::MAX / 2).line, 1); // clamped into last line
    }

    fn text_len(s: &str) -> usize {
        s.len()
    }

    #[test]
    fn quickfix_text_mirrors_typed_case() {
        assert_eq!(mirror_case("maxmsgs", "MAXMSGL"), "maxmsgl");
        assert_eq!(mirror_case("MAXMSGS", "MAXMSGL"), "MAXMSGL");
        assert_eq!(mirror_case("MaxMsgs", "MAXMSGL"), "MAXMSGL"); // mixed stays upper
    }

    // ------------------------------------------------------------- codes 1..12

    #[test]
    fn code01_parse_error() {
        assert!(has("DEFINE QLOCAL(A) #oops 1\n", CODE_PARSE));
    }

    #[test]
    fn code02_unknown_verb() {
        let diags_codes = codes("FIND QLOCAL(A)\n");
        assert!(diags_codes.contains(&CODE_UNKNOWN_VERB));
    }

    #[test]
    fn code03_unknown_object() {
        assert!(has("DEFINE FROBNIC(A)\n", CODE_UNKNOWN_OBJECT));
    }

    #[test]
    fn code04_unknown_param() {
        assert!(has("DEFINE QLOCAL(A) BOGUSPARM(1)\n", CODE_UNKNOWN_PARAM));
    }

    #[test]
    fn code05_bad_enum_value() {
        assert!(has("DEFINE QLOCAL(A) DISTL(BOGUS)\n", CODE_BAD_VALUE));
    }

    #[test]
    fn code06_duplicate_param() {
        assert!(has(
            "DEFINE QLOCAL(A) MAXDEPTH(1) MAXDEPTH(2)\n",
            CODE_DUPLICATE
        ));
    }

    #[test]
    fn code06_does_not_flag_bare_display_attributes() {
        // T-8 acceptance: repeated bare positional names are legal MQSC
        assert!(!has("DISPLAY QLOCAL(A) B B\n", CODE_DUPLICATE));
    }

    #[test]
    fn code07_param_word_used_as_command() {
        let diags = analyze(
            "DESCR REPLACE\n",
            &parse("DESCR REPLACE\n"),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let d = diags
            .iter()
            .find(|d| d.code == Some(lsp::NumberOrString::Number(CODE_MISSING_CONT)));
        assert!(
            d.is_some(),
            "expected MISSING_CONT, got {:?}",
            codes("DESCR REPLACE\n")
        );
        assert!(d.unwrap().message.contains("parameter"));
    }

    #[test]
    fn code07_dangling_continuation_at_eof() {
        // A continuation marker with no content line after it (the rest of
        // the file is blanks, comments or nothing): flagged, and the parser
        // must not invent a phantom token in error recovery.
        for src in [
            "DEFINE QLOCAL(A) -\n",
            "DEFINE QLOCAL(A) -", // marker as the file's last byte
            "DEFINE QLOCAL(A) +\n",
            "DEFINE QLOCAL(A) -\n\n\n",
            "DEFINE QLOCAL(A) -\n* only a comment\n",
            "-\n",
        ] {
            assert!(
                has(src, CODE_MISSING_CONT),
                "expected a dangling-continuation flag for {src:?}, got {:?}",
                codes(src)
            );
            let c = codes(src);
            assert!(
                !c.contains(&CODE_PARSE) && !c.contains(&CODE_UNKNOWN_PARAM),
                "phantom parse diagnostic for {src:?}: {c:?}"
            );
        }
    }

    #[test]
    fn code07_not_flagged_when_the_command_really_continues() {
        for src in [
            "DEFINE QLOCAL(A) DESCR('x') -\nREPLACE\n",
            "DEFINE QLOCAL(A) DESCR('x') -\n* a comment line\nREPLACE\n",
            "DEFINE QLOCAL(A) DESCR('x') -\n\n  REPLACE\n",
        ] {
            assert!(
                !has(src, CODE_MISSING_CONT),
                "legit continuation flagged for {src:?}, got {:?}",
                codes(src)
            );
        }
    }

    #[test]
    fn code08_tab_character() {
        assert!(has("DEFINE\tQLOCAL(A)\n", CODE_LEXICAL));
    }

    #[test]
    fn code09_missing_required_param() {
        assert!(has("MOVE QLOCAL(A)\n", CODE_MISSING_REQUIRED));
    }

    #[test]
    fn code10_missing_object_name() {
        assert!(has("DEFINE QLOCAL REPLACE\n", CODE_MISSING_NAME));
    }

    #[test]
    fn code11_wildcard_outside_display() {
        assert!(has("ALTER QLOCAL(A?B)\n", CODE_WILDCARD));
        assert!(!has("DISPLAY QLOCAL(A?)\n", CODE_WILDCARD));
    }

    #[test]
    fn code12_hint_only_for_scripts_that_define_the_type() {
        // T-24: the hint arms only when a matching object is DEFINEd here
        assert!(has(
            "DEFINE QLOCAL(X)\nDEFINE CHANNEL(A) CHLTYPE(SDR) TRPTYPE(TCP) XMITQ(NOTDEF)\n",
            CODE_UNDEFINED_OBJECT
        ));
        assert!(!has(
            "DEFINE CHANNEL(A) CHLTYPE(SDR) TRPTYPE(TCP) XMITQ(NOTDEF)\n",
            CODE_UNDEFINED_OBJECT
        ));
    }

    #[test]
    fn code13_object_name_length() {
        let n48 = "Q".repeat(48);
        let n49 = "Q".repeat(49);
        // the queue-name cap is exactly 48 (corpus-confirmed: real 48-char
        // QLOCAL/QALIAS/PROCESS names exist on live queue managers)
        assert!(!has(&format!("DEFINE QLOCAL({n48})\n"), CODE_NAME_TOO_LONG));
        assert!(has(&format!("DEFINE QLOCAL({n49})\n"), CODE_NAME_TOO_LONG));
        // measured on the unwrapped name, not the quoted token
        assert!(!has(
            &format!("DEFINE QLOCAL('{n48}')\n"),
            CODE_NAME_TOO_LONG
        ));
        assert!(has(&format!("ALTER QLOCAL('{n49}')\n"), CODE_NAME_TOO_LONG));
        assert!(has(&format!("DELETE QLOCAL({n49})\n"), CODE_NAME_TOO_LONG));
        // a doubled '' counts as one character: 49 apostrophes -> flagged
        let wrapped = format!("'{}'", "'".repeat(49).replace('\'', "''"));
        assert!(has(
            &format!("DEFINE QLOCAL({wrapped})\n"),
            CODE_NAME_TOO_LONG
        ));
        // pattern/wildcard names are legal MQSC regardless of length
        assert!(!has(
            &format!("DEFINE QLOCAL('{}*')\n", "Q".repeat(60)),
            CODE_NAME_TOO_LONG
        ));
        // channels cap at 1024, not 48
        assert!(!has(
            &format!("DEFINE CHANNEL({}) TRPTYPE(TCP)\n", "C".repeat(100)),
            CODE_NAME_TOO_LONG
        ));
        // object types without a curated cap are never flagged (COMMINFO:
        // positional name, deliberately not in the name_max table)
        assert!(!has(
            &format!("DEFINE COMMINFO({})\n", "G".repeat(200)),
            CODE_NAME_TOO_LONG
        ));
        // non-positional verbs (DISPLAY's name argument) are not checked
        assert!(!has(
            &format!("DISPLAY QLOCAL({}) MAXDEPTH\n", "D".repeat(200)),
            CODE_NAME_TOO_LONG
        ));
    }

    #[test]
    fn unquote_name_unwraps_and_unescapes() {
        assert_eq!(unquote_name("ABC"), "ABC");
        assert_eq!(unquote_name("'ABC'"), "ABC");
        assert_eq!(unquote_name("''"), "");
        assert_eq!(unquote_name("'A''B'"), "A'B");
        assert_eq!(unquote_name("'"), "'"); // lone quote: parse-error shape
        assert_eq!(unquote_name("'unterminated"), "'unterminated");
    }

    // ------------------------------------------------- enum + integer alts (T-3/T-7)

    #[test]
    fn enum_params_with_integer_alternative_accept_both() {
        for src in [
            "DEFINE PROCESS(P) APPLTYPE(CICS)\n",
            "DEFINE PROCESS(P) APPLTYPE(7)\n",
            "DEFINE TOPIC(T) DEFPRTY(0)\n",
            "DEFINE TOPIC(T) DEFPRTY(ASPARENT)\n",
            "DEFINE QMODEL(M) ACCTQ(OFF)\n",
            "SET AUTHREC OBJTYPE(CLNTCONN) PROFILE('X')\n",
            "DEFINE CHANNEL(C) CHLTYPE(SDR) TRPTYPE(TCP) KAINT(AUTO)\n",
            "DEFINE CHANNEL(C) CHLTYPE(SDR) TRPTYPE(TCP) KAINT(360)\n",
            "DEFINE SUB(S) TOPICSTR(a/b) EXPIRY(UNLIMITED)\n",
            "DEFINE SUB(S) TOPICSTR(a/b) EXPIRY(120)\n",
        ] {
            assert!(
                !has(src, CODE_BAD_VALUE),
                "unexpected BAD_VALUE for {src:?}: {:?}",
                codes(src)
            );
        }
    }

    #[test]
    fn enum_validation_still_fires_next_to_integer_alts() {
        assert!(has("DEFINE PROCESS(P) APPLTYPE(BOGUS)\n", CODE_BAD_VALUE));
        assert!(has("DEFINE TOPIC(T) DEFPRTY(BOGUS)\n", CODE_BAD_VALUE));
    }

    #[test]
    fn quickfix_values_mirror_typed_case() {
        // mirror_case is wired to enum-value suggestions (check_values):
        // the replacement must fit what the user typed, MQSC is CI
        let lower = analyze(
            "DEFINE QLOCAL(A) DISTL(yesx)\n",
            &parse("DEFINE QLOCAL(A) DISTL(yesx)\n"),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let d = lower
            .iter()
            .find(|d| d.code == Some(lsp::NumberOrString::Number(CODE_BAD_VALUE)))
            .expect("DISTL(yesx) must produce BAD_VALUE");
        assert_eq!(
            d.data
                .as_ref()
                .and_then(|v| v.pointer("/quickfixes/0"))
                .and_then(|v| v.as_str()),
            Some("yes"),
            "quickfix must mirror the typed lowercase (data = {:?})",
            d.data
        );

        let upper = analyze(
            "DEFINE QLOCAL(A) DISTL(YESX)\n",
            &parse("DEFINE QLOCAL(A) DISTL(YESX)\n"),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let d = upper
            .iter()
            .find(|d| d.code == Some(lsp::NumberOrString::Number(CODE_BAD_VALUE)))
            .expect("DISTL(YESX) must produce BAD_VALUE");
        assert_eq!(
            d.data
                .as_ref()
                .and_then(|v| v.pointer("/quickfixes/0"))
                .and_then(|v| v.as_str()),
            Some("YES")
        );
    }

    // -------------------------------------------------------------- robustness

    #[test]
    fn analyze_and_parse_survive_garbage() {
        let uri = Url::parse("file:///g.mqsc").unwrap();
        let m = Model::load();
        for text in [
            "",
            "\n\n",
            "(((",
            "'''",
            "'unclosed",
            "\u{1}\u{2}x",
            "DEFINE",
            "DEFINE ()",
            "DISPLAY ?? ** //",
            "+ + + +",
            &"A".repeat(4096),
            &"ALTER QMGR ( ".repeat(100),
            "DESCR('a''b''c",
        ] {
            let tree = parse(text);
            let _ = analyze(text, &tree, m, &uri, &Definitions::default());
        }
    }

    // ------------------------------------------ name sites / navigation (T-16, T-18)

    fn index_of(text: &str) -> ObjectIndex {
        object_index(text, &parse(text), Model::load())
    }

    #[test]
    fn object_index_marks_definitions_and_uses() {
        let text = "DEFINE QLOCAL(A)\nALTER QLOCAL(A) MAXDEPTH(1)\nDEFINE QALIAS(B) TARGET(A)\nDISPLAY QLOCAL(B)\n";
        let idx = index_of(text);
        // A: the DEFINE plus the ALTER positional and the TARGET reference
        let a: Vec<_> = idx.sites_of("A").collect();
        assert_eq!(a.len(), 3, "{a:?}");
        assert_eq!(a[0].defines.as_deref(), Some("QLOCAL"));
        assert_eq!(a[0].range.start.line, 0);
        assert!(a[1..].iter().all(|s| s.defines.is_none()));
        assert_eq!(a[2].range.start.line, 2); // TARGET(A)
                                              // B: defined as QALIAS, used by DISPLAY on the next line
        let b: Vec<_> = idx.sites_of("B").collect();
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!(b[0].defines.as_deref(), Some("QALIAS"));
        assert_eq!(b[1].range.start.line, 3);
    }

    #[test]
    fn object_index_skips_patterns_and_plain_string_values() {
        let text = "DEFINE QLOCAL(A)\nDISPLAY QLOCAL(A*)\nDEFINE QLOCAL(B) DESCR('A')\n";
        let idx = index_of(text);
        // 'A*' is a pattern, not a name; DESCR('A') is not an object-ref
        // parameter — only the two DEFINE names are indexed.
        let names: Vec<String> = idx.sites.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn quoted_name_site_spans_the_quotes() {
        let text = "DEFINE QALIAS(B) TARGET('A')\n";
        let idx = index_of(text);
        let s = idx
            .site_at(Position::new(0, 25))
            .expect("cursor inside 'A'");
        assert_eq!(s.name, "A");
        assert!(s.quoted);
        assert_eq!(s.word, "A");
        assert_eq!(
            s.range,
            Range::new(Position::new(0, 24), Position::new(0, 27))
        );
    }

    #[test]
    fn no_name_site_on_verb_object_word_or_plain_value() {
        // DEFINE 0..6 | QLOCAL 7..13 | 'SHOP.FRONT' token 14..26 | REPLACE 28..35
        let text = "DEFINE QLOCAL('SHOP.FRONT') REPLACE\n";
        let idx = index_of(text);
        for (col, what) in [
            (0u32, "DEFINE first char"),
            (3, "DEFINE middle"),
            (7, "QLOCAL first char"),
            (10, "QLOCAL middle"),
            (28, "REPLACE first char"),
            (31, "REPLACE middle"),
        ] {
            assert!(
                idx.site_at(Position::new(0, col)).is_none(),
                "{what} at col {col} must not be a name site (rename would trigger on a verb)"
            );
        }
        // the quoted name itself is a site, quotes included
        for col in [14u32, 15, 24, 25] {
            assert!(
                idx.site_at(Position::new(0, col)).is_some(),
                "col {col} is inside the name token"
            );
        }
    }

    #[test]
    fn hover_on_names_reports_the_definition() {
        let text = "DEFINE QLOCAL(SRC)\nDEFINE QALIAS(ALIAS1) TARGET(SRC)\n";
        let m = Model::load();
        let tree = parse(text);
        let md_of = |h: Hover| match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        // on the definition itself
        let h =
            hover(text, &tree, m, Position::new(0, 15), &Default::default()).expect("hover def");
        let md = md_of(h);
        assert!(
            md.contains("QLOCAL") && md.contains("this is the definition"),
            "{md}"
        );
        // on the reference: names the defining line (1-based) and use count
        let h =
            hover(text, &tree, m, Position::new(1, 29), &Default::default()).expect("hover use");
        let md = md_of(h);
        assert!(md.contains("DEFINEd at line 1"), "{md}");
        assert!(md.contains("1 use"), "{md}");
    }

    // ------------------------------------------- cross-file knowledge (T-17)

    /// A sibling document that defines `name` as a QLOCAL.
    fn sibling_defining(name: &str) -> Definitions {
        let mut d = Definitions::default();
        d.names.insert(name.to_string());
        d.by_obj
            .entry("QLOCAL".to_string())
            .or_default()
            .push(name.to_string());
        d.any_defines = true;
        d
    }

    #[test]
    fn external_definitions_silence_the_undefined_hint() {
        let text = "DEFINE QALIAS(A) TARGET(GHOST)\n";
        let tree = parse(text);
        let uri = Url::parse("file:///a.mqsc").unwrap();
        let m = Model::load();
        let is12 =
            |d: &Diagnostic| d.code == Some(lsp::NumberOrString::Number(CODE_UNDEFINED_OBJECT));
        // alone: the hint fires on TARGET(GHOST)
        assert!(codes(text).contains(&CODE_UNDEFINED_OBJECT));
        // with a sibling document DEFINing GHOST: gone
        let diags = analyze(text, &tree, m, &uri, &sibling_defining("GHOST"));
        assert!(
            !diags.iter().any(is12),
            "{:?}",
            diags.iter().map(|d| &d.message)
        );
        // still hinted when the sibling defines something else — and then the
        // message widens its scope to the other documents
        let diags = analyze(text, &tree, m, &uri, &sibling_defining("OTHER"));
        let d = diags
            .iter()
            .find(|d| is12(d))
            .expect("GHOST must still be hinted");
        assert!(
            d.message
                .contains("this script or the other open documents"),
            "{}",
            d.message
        );
    }

    #[test]
    fn hover_points_at_a_definition_in_another_document() {
        let text = "DEFINE QALIAS(A) TARGET(GHOST)\n";
        let tree = parse(text);
        let m = Model::load();
        let mut ext: ExternalDefs = Default::default();
        ext.insert(
            "GHOST".into(),
            vec![("sibling.mqsc".to_string(), 7, "QLOCAL".to_string())],
        );
        // cursor inside GHOST (TARGET( sits at col 22, G at 24)
        let h = hover(text, &tree, m, Position::new(0, 25), &ext).expect("hover");
        let md = match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        assert!(
            md.contains("sibling.mqsc")
                && md.contains("line 8")
                && md.contains("another open document"),
            "{md}"
        );
        // without the sibling it falls back to the plain "not DEFINEd" text
        let h = hover(text, &tree, m, Position::new(0, 25), &Default::default()).expect("hover");
        let md = match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        assert!(md.contains("not DEFINEd in this script"), "{md}");
    }

    #[test]
    fn symbols_labels_carry_object_and_name() {
        let text = "DEFINE QLOCAL(FUNNY)\nDISPLAY QLOCAL(FUNNY) DESCR\n";
        let syms = symbols(text, &parse(text));
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "DEFINE QLOCAL (FUNNY)");
        assert_eq!(syms[1].name, "DISPLAY QLOCAL (FUNNY)");
        assert!(syms[1].detail.as_deref().unwrap().contains("DESCR"));
    }

    // ------------------------------------------------ completion (next words)

    /// Completion item labels for `text` with the cursor at (line, col).
    fn compl_labels(text: &str, line: u32, col: u32) -> Vec<String> {
        let m = Model::load();
        let tree = parse(text);
        completions(
            text,
            &tree,
            m,
            Position::new(line, col),
            false,
            &Definitions::default(),
        )
        .into_iter()
        .map(|i| i.label)
        .collect()
    }

    #[test]
    fn committed_verb_completes_object_types() {
        // after the verb the ONLY completions are the object types the
        // verb accepts — no verbs, no parameters
        let l = compl_labels("DEFINE ", 0, 7);
        for want in ["CHANNEL", "QLOCAL", "QALIAS", "QMODEL", "QREMOTE", "QUEUE"] {
            assert!(l.contains(&want.to_string()), "{l:?}");
        }
        for not in ["DEFINE", "ALTER", "DISPLAY", "DESCR", "REPLACE"] {
            assert!(!l.contains(&not.to_string()), "{l:?}");
        }
        // trailing blanks still count as "right after the verb"
        assert!(compl_labels("DEFINE     ", 0, 11).contains(&"CHANNEL".into()));
        assert!(compl_labels("ALTER ", 0, 6).contains(&"QMGR".into()));
        // synonym verbs canonicalise
        assert!(compl_labels("DEF ", 0, 4).contains(&"QALIAS".into()));
        assert!(compl_labels("SET ", 0, 4).contains(&"CHLAUTH".into()));
        assert_eq!(compl_labels("ADD ", 0, 4), vec!["CHLAUTH".to_string()]);
    }

    #[test]
    fn object_word_under_the_cursor_completes_object_types() {
        // "Q" alone resolves as DEFINE Q = DEFINE QUEUE, but mid-typing the
        // object word every Q* type must still be offered — not QUEUE's
        // parameters (this was the old behavior: 0 or wrong completions)
        let l = compl_labels("DEFINE Q", 0, 8);
        assert_eq!(
            l,
            vec![
                "QALIAS".to_string(),
                "QLOCAL".to_string(),
                "QMODEL".to_string(),
                "QREMOTE".to_string(),
                "QUEUE".to_string(),
            ]
        );
        // cursor at the end of the fully typed object word
        assert_eq!(
            compl_labels("DEFINE QLOCAL", 0, 13),
            vec!["QLOCAL".to_string()]
        );
    }

    #[test]
    fn object_name_slot_offers_defined_names() {
        let text = "DEFINE QLOCAL(Q1)\nDEFINE CHANNEL(CH1)\nDEFINE QLOCAL ";
        let l = compl_labels(text, 2, 14);
        assert!(l.contains(&"Q1".to_string()), "{l:?}");
        assert!(!l.contains(&"CH1".to_string()), "{l:?}"); // other object type
        assert!(!l.contains(&"*".to_string()), "{l:?}"); // DEFINE needs a real name
        assert!(!l.contains(&"DESCR".to_string()), "{l:?}"); // params come after
                                                             // DISPLAY's name slot also accepts the '*' pattern
        let l = compl_labels("DEFINE QLOCAL(Q1)\nDISPLAY QLOCAL ", 1, 15);
        assert!(
            l.contains(&"Q1".to_string()) && l.contains(&"*".to_string()),
            "{l:?}"
        );
    }

    #[test]
    fn params_follow_the_name_and_used_ones_drop_out() {
        let l = compl_labels("ALTER QLOCAL(Q1) ", 0, 17);
        assert!(
            l.contains(&"DESCR".to_string()) && l.contains(&"MAXMSGL".to_string()),
            "{l:?}"
        );
        let l = compl_labels("DEFINE QLOCAL(Q1) REPLACE ", 0, 26);
        assert!(!l.contains(&"REPLACE".to_string()), "{l:?}");
        assert!(l.contains(&"DESCR".to_string()), "{l:?}");
    }

    #[test]
    fn object_word_does_not_consume_its_own_parameter_label() {
        // DISPLAY SUB has a parameter named SUB: the object-type word must
        // not count as that parameter being used.
        let l = compl_labels("DISPLAY SUB NAME1 ", 0, 18);
        assert!(l.contains(&"SUB".to_string()), "{l:?}");
    }

    #[test]
    fn verb_and_fresh_line_still_complete_verbs() {
        let l = compl_labels("", 0, 0);
        assert!(l.contains(&"DEFINE".to_string()) && l.contains(&"DISPLAY".to_string()));
        let l = compl_labels("DE", 0, 2);
        assert!(l.contains(&"DEFINE".to_string()));
        assert!(!l.contains(&"QLOCAL".to_string())); // object types not yet
                                                     // a committed leading word that is no verb recovers with verbs
        let l = compl_labels("FOO ", 0, 4);
        assert!(l.contains(&"DEFINE".to_string()), "{l:?}");
    }

    #[test]
    fn continuation_lines_complete_the_command_slots() {
        // The `+` chain is one command node spanning several lines. At the
        // start of a continuation line the slots of THAT command complete —
        // verbs would be syntactically impossible there.
        let l = compl_labels("DEFINE QLOCAL(Q1) +\nDESCR('a')\n", 1, 0);
        assert!(l.contains(&"DESCR".to_string()), "{l:?}");
        assert!(l.contains(&"REPLACE".to_string()), "{l:?}");
        for not in ["DEFINE", "ALTER", "DISPLAY"] {
            assert!(!l.contains(&not.to_string()), "{l:?}");
        }
        // typing a param word on the continuation line still filters to it
        assert_eq!(
            compl_labels("DEFINE QLOCAL(Q1) +\nDESCR('a')\n", 1, 5),
            vec!["DESCR".to_string()]
        );
        // a parameter used on an EARLIER line of the same command drops out,
        // one typed later on the same line still counts as available
        let l = compl_labels("DEFINE QLOCAL(Q1) REPLACE +\nDESCR('a') \n", 1, 12);
        assert!(!l.contains(&"REPLACE".to_string()), "{l:?}");
        assert!(!l.contains(&"DESCR".to_string()), "{l:?}");
        assert!(l.contains(&"DEFPSIST".to_string()), "{l:?}");
    }
}
