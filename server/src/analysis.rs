//! Tree-sitter parsing and all MQSC language analysis: diagnostics, hover,
//! completion, document symbols, folding, highlights, semantic tokens.

use std::collections::{BTreeMap, HashMap, HashSet};

use lsp_types::{
    self as lsp, CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentHighlight, DocumentHighlightKind, DocumentSymbol, FoldingRange, FoldingRangeKind,
    Hover, HoverContents, InsertTextFormat, MarkupContent, MarkupKind, Position, Range,
    SemanticToken, SymbolKind, Url,
};
use serde_json::json;
use tree_sitter::{Language, Node, Tree};

use crate::model::{self, AvailRule, Command, Model, Param, ValueKind};
use crate::settings::{AvailViolation, Target};

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

/// Compact example value for a parameter, so messages can show the expected
/// shape (`MAXMSGL(99)`, `DESCR('some text')`, `TARGET(NAME)`, ...) instead
/// of only naming the parameter.
fn value_example(p: &Param) -> String {
    match &p.kind {
        ValueKind::Flag => String::new(),
        ValueKind::Integer => "99".into(),
        ValueKind::Enum(vals) => vals.first().cloned().unwrap_or_else(|| "VALUE".into()),
        ValueKind::Str | ValueKind::Free => "'some text'".into(),
        ValueKind::Name => "NAME".into(),
        ValueKind::Star => "*".into(),
    }
}

/// Join candidates for a message, honestly ellipsizing when cut off.
fn join_capped<'a, I: IntoIterator<Item = &'a str>>(items: I, max: usize) -> String {
    let all: Vec<&str> = items.into_iter().collect();
    let mut s = all.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if all.len() > max {
        s.push_str(", …");
    }
    s
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
const CODE_MISSING_MANDATORY: i32 = 14;
/// Parameter exists in the model but not for the user-configured MQ version
/// or platform (`mqsc.targetVersion` / `mqsc.platform`). Never fires when no
/// target is configured.
const CODE_NOT_IN_TARGET: i32 = 15;

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
        CODE_MISSING_MANDATORY => "MISSING_MANDATORY",
        CODE_NOT_IN_TARGET => "NOT_IN_TARGET",
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

/// Analyze with no target configured: every availability rule dormant
/// (the pre-Phase-4 behavior). Tests and callers without settings use this.
pub fn analyze(
    text: &str,
    tree: &Tree,
    m: &Model,
    uri: &Url,
    extra_defs: &Definitions,
) -> Vec<Diagnostic> {
    analyze_with_target(text, tree, m, uri, extra_defs, &Target::default())
}

/// Analyze under a configured MQ target: parameters whose curated
/// availability rule conflicts with `target` get code 15 NOT_IN_TARGET,
/// and a not-in-model parameter that the target's version/platform does
/// provide stays unflagged. With `target` unset this is byte-identical to
/// `analyze`.
pub fn analyze_with_target(
    text: &str,
    tree: &Tree,
    m: &Model,
    _uri: &Url,
    extra_defs: &Definitions,
    target: &Target,
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
                "MQSC does not treat a tab as whitespace — this character becomes part of a \
                 word and splits the command in the wrong place. Replace the tab with blanks."
                    .into(),
            );
        }
        let len = line.chars().count();
        if len > 2048 {
            let s = lines.line_start(i);
            d.push(
                lines.range(s, s + 1),
                DiagnosticSeverity::WARNING,
                CODE_LEXICAL,
                format!(
                    "This line has {len} characters; MQSC allows at most 2048. Split the \
                     command across lines: end a line with '+' to continue it on the next."
                ),
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
                "Incomplete command: something must follow here. A command is \
                 `VERB OBJECTTYPE(NAME)` followed by `PARAM(value)` items."
                    .to_string()
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
                    msg = format!("{msg}: `{snippet}`.");
                }
                msg = format!(
                    "{msg} Commands look like `VERB OBJECTTYPE(NAME)` plus `PARAM(value)` \
                     items — check for a stray, missing or unclosed token here."
                );
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
                    "'{marker}' continuation marker is never continued: the rest of the file \
                     has no continuation line for this command, so it is truncated. Add the \
                     continued part on the next line, or remove the '{marker}'."
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
            check_command(cmd, src, &lines, m, &defs, target, &mut d);
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
                defs.names.insert(name.clone());
                // register per object TYPE independently of the flat set:
                // one spelling may be DEFINEd under several classes (a
                // QLOCAL and a CHANNEL `SHOP.FRONT`), and namespace-aware
                // resolution needs each type's own list — the flat-set
                // gate used to hide every type but the first
                let list = defs.by_obj.entry(obj.clone()).or_default();
                if !list.contains(&name) {
                    list.push(name);
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
        // Backout queue (BOQNAME), plus the other queue-name-valuing
        // QMGR/queue params. The documented special values
        // (BOQNAME(NO/YES), DEADQ(NONE)...) are exempted in
        // check_obj_ref, not hidden from this table.
        "BOQNAME" | "DEADQ" | "DEFXMITQ" => Some(ObjRef::QueueFamily),
        // INITQ on QLOCAL/QMODEL names the initiation queue; on START
        // CHINIT it names a *queue manager* (the initiator to start).
        "INITQ" if cmd_obj.starts_with('Q') => Some(ObjRef::QueueFamily),
        _ => None,
    }
}

/// The semantic namespace a name site belongs to: which object a written
/// name refers to (or introduces). MQ keeps one name namespace per object
/// class, and every queue type — QLOCAL/QMODEL/QALIAS/QREMOTE/QCLUSTER and
/// the QUEUE umbrella — shares ONE namespace: `DISPLAY QUEUE` finds local
/// queues, a `TARGET` may point at any of them, while a CHANNEL of the
/// same spelling is a different object entirely. Everything that resolves
/// a name (navigation, rename, code-12 existence, completion pools) goes
/// through this one rule; comparing the bare spelling conflates same-named
/// objects of different classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectScope {
    /// any queue-like object (canonical model type starting with `Q`)
    QueueFamily,
    /// exactly this canonical model object type
    Exact(String),
}

/// Namespace key of a canonical object type: the shared queue namespace
/// for queue types, the type itself otherwise. Two name sites name the
/// same object exactly when their namespaces — and names — match.
pub fn type_namespace(obj: &str) -> &str {
    if obj.starts_with('Q') {
        "QUEUE"
    } else {
        obj
    }
}

impl ObjectScope {
    /// The namespace this scope resolves inside of.
    pub fn namespace(&self) -> &str {
        match self {
            ObjectScope::QueueFamily => "QUEUE",
            ObjectScope::Exact(t) => type_namespace(t),
        }
    }

    /// Does a definition of canonical type `def_obj` satisfy this scope?
    pub fn accepts_type(&self, def_obj: &str) -> bool {
        match self {
            ObjectScope::QueueFamily => def_obj.starts_with('Q'),
            ObjectScope::Exact(t) => t == def_obj,
        }
    }

    /// Scope of a positional object-name slot (and of an object-name group
    /// of a command) whose object type is `cmd_obj`.
    pub fn for_object_type(cmd_obj: &str) -> Self {
        if cmd_obj.starts_with('Q') {
            ObjectScope::QueueFamily
        } else {
            ObjectScope::Exact(cmd_obj.to_string())
        }
    }
}

/// Scope of a curated object-reference parameter inside a command over
/// `cmd_obj` — the ObjRef table's family, made concrete.
fn obj_ref_scope(r: ObjRef, cmd_obj: &str) -> ObjectScope {
    match r {
        ObjRef::QueueFamily => ObjectScope::QueueFamily,
        // SameType: the reference names the very class the command works
        // on, so the queue collapse already happened in for_object_type.
        ObjRef::SameType => ObjectScope::for_object_type(cmd_obj),
    }
}

/// Value of the command's TARGTYPE parameter, uppercased (z/OS only):
/// `DEFINE/ALTER QALIAS ... TARGTYPE(QLOCAL|QALIAS|TOPIC)` declares what
/// object class the alias's TARGET actually names.
fn command_targtype(items: &[Item<'_>], src: &[u8]) -> Option<String> {
    items.windows(2).find_map(|w| match (&w[0], &w[1]) {
        (Item::Word(n), Item::Arg(g)) if node_text(*n, src).eq_ignore_ascii_case("TARGTYPE") => {
            value_nodes(*g)
                .first()
                .map(|v| unquote_name(node_text(*v, src).trim()).to_ascii_uppercase())
        }
        _ => None,
    })
}

/// Scope for a curated object-ref parameter, with the command's TARGTYPE
/// gate: a z/OS queue alias `TARGTYPE(TOPIC)` points its TARGET at a TOPIC,
/// not at a queue — treating it as a queue reference flagged thousands of
/// perfectly good topic aliases on the dmpmqcfg corpus.
fn ref_scope(pname: &str, r: ObjRef, cmd_obj: &str, targtype: Option<&str>) -> ObjectScope {
    if pname == "TARGET" && cmd_obj.starts_with('Q') && targtype == Some("TOPIC") {
        ObjectScope::Exact("TOPIC".to_string())
    } else {
        obj_ref_scope(r, cmd_obj)
    }
}

/// A name plus the namespace it was read in — the unit every resolution
/// feature (definition, references, rename, diagnostics, completion)
/// joins on, so they can never disagree about what a name means.
#[derive(Debug, Clone)]
pub struct SymbolKey {
    pub name: String,
    pub scope: ObjectScope,
}

/// Defined names whose type the scope accepts, as (object type, name)
/// pairs.
fn names_for<'a>(defs: &'a Definitions, scope: &ObjectScope) -> Vec<(&'a str, &'a str)> {
    defs.by_obj
        .iter()
        .filter(|(obj, _)| scope.accepts_type(obj))
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

    /// Whether code 12 may fire at all for a reference scope in this script:
    /// only when the script DEFINEs at least one object of a type the scope
    /// accepts. A dump that only contains, say, QALIAS definitions never
    /// gets TARGET hints (the real queues live in other scripts) — see
    /// README "Same-script object knowledge".
    fn arms(&self, scope: &ObjectScope) -> bool {
        self.by_obj.keys().any(|o| scope.accepts_type(o))
    }

    /// Whether `name` is DEFINEd as an object TYPE compatible with `scope`
    /// — the same rule navigation resolves on (`ObjectScope`), so "exists"
    /// never means two different things: a name that lives only in another
    /// namespace (a CHANNEL where a queue is expected) does not resolve
    /// here, exactly as it would not be found by goto-definition.
    pub fn contains_compatible(&self, name: &str, scope: &ObjectScope) -> bool {
        self.by_obj
            .iter()
            .any(|(obj, names)| scope.accepts_type(obj) && names.iter().any(|n| n == name))
    }
}

/// What a code-12 existence check is about: the parameter word, the
/// object class its command works on, and the TARGTYPE gate qualifying
/// a z/OS alias TARGET (keeps `check_obj_ref` within sane arity).
struct RefSubject<'a> {
    pname: &'a str,
    cmd_obj: &'a str,
    targtype: Option<&'a str>,
}

/// Code 15 NOT_IN_TARGET: a parameter the script uses is not available for
/// the user-configured MQ target (mqsc.targetVersion / mqsc.platform),
/// per the overlay's curated `availability` rules. Fires nowhere unless a
/// target is configured, and never for rules without usable data — the
/// conservative convention of name_max/mandatory throughout.
fn check_target_avail(
    m: &Model,
    target: &Target,
    pname: &str,
    cmd_obj: &str,
    node: Node<'_>,
    lines: &Lines,
    d: &mut Diags,
) {
    if !target.configured() {
        return;
    }
    let Some(rule) = m.availability_for(pname, cmd_obj) else {
        return;
    };
    let Some(violation) = target.gate(rule) else {
        return;
    };
    let version = target
        .version
        .map_or_else(|| "unset".to_string(), |v| v.to_string());
    let head = match violation {
        AvailViolation::TooOld(since) => format!(
            "'{pname}' is available from MQ {since} — mqsc.targetVersion is {version}. \
             Remove the parameter or raise targetVersion to the queue manager's version."
        ),
        AvailViolation::TooNew(until) => format!(
            "'{pname}' is available up to MQ {until} — mqsc.targetVersion is {version}. \
             Remove the parameter or lower targetVersion to the queue manager's version."
        ),
        AvailViolation::WrongPlatform(rp) => format!(
            "'{pname}' is only available on {rp} — mqsc.platform is {}.",
            target
                .platform
                .map_or_else(|| "unset".to_string(), |p| p.to_string())
        ),
    };
    let msg = if rule.note.is_empty() {
        head
    } else {
        format!("{head} {}", rule.note)
    };
    d.push(
        lines.range(node.start_byte(), node.end_byte()),
        DiagnosticSeverity::WARNING,
        CODE_NOT_IN_TARGET,
        msg,
    );
}

/// Code 15 NOT_IN_TARGET on the command itself: the whole `VERB OBJECT`
/// is unavailable for the configured target, per the overlay's curated
/// `command_availability` rules — the live-oracle differential is what
/// knows distributed has no CFSTRUCT family and that CHLAUTH starts at
/// 9.0. Same settings gate and conservative conventions as the parameter
/// check (fires nowhere without a target, never on unknown availability).
fn check_command_avail(
    m: &Model,
    target: &Target,
    e: &Command,
    verb_node: Node<'_>,
    obj_node: Option<Node<'_>>,
    lines: &Lines,
    d: &mut Diags,
) {
    if !target.configured() {
        return;
    }
    let Some(rule) = m.command_availability_for(e.verb, e.object) else {
        return;
    };
    // the decision logic lives in Target::gate on the parameter-shaped
    // rule; feed it this command's version/platform fields
    let gate = AvailRule {
        param: format!("{} {}", e.verb, e.object),
        obj: Vec::new(),
        since: rule.since.clone(),
        until: rule.until.clone(),
        platform: rule.platform.clone(),
        note: String::new(),
    };
    let Some(violation) = target.gate(&gate) else {
        return;
    };
    let cmd = format!("{} {}", e.verb, e.object);
    let version = target
        .version
        .map_or_else(|| "unset".to_string(), |v| v.to_string());
    let head = match violation {
        AvailViolation::TooOld(since) => format!(
            "'{cmd}' is available from MQ {since} — mqsc.targetVersion is {version}. \
             Remove the command or raise targetVersion to the queue manager's version."
        ),
        AvailViolation::TooNew(until) => format!(
            "'{cmd}' is available up to MQ {until} — mqsc.targetVersion is {version}. \
             Remove the command or lower targetVersion to the queue manager's version."
        ),
        AvailViolation::WrongPlatform(rp) => format!(
            "'{cmd}' is only available on {rp} — mqsc.platform is {}.",
            target
                .platform
                .map_or_else(|| "unset".to_string(), |p| p.to_string())
        ),
    };
    let msg = if rule.note.is_empty() {
        head
    } else {
        format!("{head} {}", rule.note)
    };
    // underline the whole VERB OBJECT head, not just the verb
    let end = obj_node.map_or_else(|| verb_node.end_byte(), |n| n.end_byte());
    d.push(
        lines.range(verb_node.start_byte(), end),
        DiagnosticSeverity::WARNING,
        CODE_NOT_IN_TARGET,
        msg,
    );
}

/// Code 12: an object-name reference (TARGET, XMITQ, LIKE, ...) that no
/// DEFINE in this script provides. Hint severity on purpose: runmqsc
/// scripts routinely reference queue-manager objects that exist already.
fn check_obj_ref(
    s: RefSubject<'_>,
    values: &[Node<'_>],
    src: &[u8],
    lines: &Lines,
    defs: &Definitions,
    d: &mut Diags,
) {
    let (pname, cmd_obj) = (s.pname, s.cmd_obj);
    let Some(r) = object_ref_param(pname, cmd_obj) else {
        return;
    };
    let scope = ref_scope(pname, r, cmd_obj, s.targtype);
    if !defs.any_defines || !defs.arms(&scope) {
        return;
    }
    // existence is namespace-qualified: "does a DEFINE this parameter
    // could actually point at provide this name?" — the same ObjectScope
    // rule goto-definition resolves on, so a name that exists only as a
    // different object class reads as unresolved here too
    for v in values {
        for name in value_name_words(*v, src) {
            // Blank/' ' values (legal in dumps, e.g. SHORTUSR(' ')) are never typos.
            if name.trim().is_empty() {
                continue;
            }
            // Documented special values of the queue-ref params are toggles,
            // not names: z/OS BOQNAME(NO|YES), DEADQ(NONE)/DEFXMITQ(NONE).
            if (pname == "BOQNAME" && (name == "NO" || name == "YES"))
                || (matches!(pname, "DEADQ" | "DEFXMITQ") && name == "NONE")
            {
                continue;
            }
            // IBM ships every SYSTEM.* object; never flag those.
            if defs.contains_compatible(&name, &scope) || name.starts_with("SYSTEM") {
                continue;
            }
            // suggest from the family this parameter accepts, not every
            // DEFINEd name: smaller, and never suggests a type-incompatible
            // name the quickfix would only get flagged again
            let pool = names_for(defs, &scope);
            let sugs = model::suggest(&name, pool.iter().map(|&(_, n)| n), 3);
            let scope = if defs.from_other_files {
                "this script or the other open documents"
            } else {
                "this script"
            };
            let msg = if sugs.is_empty() {
                format!(
                    "'{name}' is not DEFINEd in {scope}; it must already exist in the \
                     queue manager (or is a typo)",
                )
            } else {
                format!(
                    "'{name}' is not DEFINEd in {scope} — closest names DEFINEd there are \
                     {}; use one of them, or keep '{name}' only if it already exists in \
                     the queue manager",
                    sugs.join(", ")
                )
            };
            let fixes = sugs.iter().map(|s| s.to_string()).collect();
            d.push_fixes(
                lines.range(v.start_byte(), v.end_byte()),
                DiagnosticSeverity::HINT,
                CODE_UNDEFINED_OBJECT,
                msg,
                fixes,
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
    /// which object class this appearance names: a DEFINE introduces the
    /// object in its own class' namespace, a use in the namespace the
    /// slot expects (positional name → the command's, ref parameters →
    /// the curated family). Resolution matches name AND namespace.
    pub scope: ObjectScope,
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

    /// The symbol under `pos`: its normalised name plus the namespace the
    /// slot says it lives in. Every cross-feature lookup starts here.
    pub fn symbol_at(&self, pos: Position) -> Option<SymbolKey> {
        self.site_at(pos).map(|s| SymbolKey {
            name: s.name.clone(),
            scope: s.scope.clone(),
        })
    }

    /// Every site naming `name` inside namespace `ns` — THE join rule all
    /// resolution shares (see `ObjectScope`): a DEFINE of a queue and a
    /// DEFINE of a same-named channel simply never meet in one result.
    pub fn sites_in<'a>(
        &'a self,
        name: &'a str,
        ns: &'a str,
    ) -> impl Iterator<Item = &'a NameSite> {
        self.sites
            .iter()
            .filter(move |s| s.name == name && s.scope.namespace() == ns)
    }

    /// The DEFINE sites of `name` within namespace `ns`.
    pub fn definitions_in<'a>(&'a self, name: &'a str, ns: &'a str) -> Vec<&'a NameSite> {
        self.sites_in(name, ns)
            .filter(|s| s.defines.is_some())
            .collect()
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
        // the TARGTYPE gate for object-ref params (z/OS alias TARGET class)
        let targtype = command_targtype(&items, src);
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
                    let scope = if positional {
                        Some(ObjectScope::for_object_type(e.object))
                    } else {
                        owner.as_deref().and_then(|w| {
                            object_ref_param(w, e.object)
                                .map(|r| ref_scope(w, r, e.object, targtype.as_deref()))
                        })
                    };
                    let Some(scope) = scope else {
                        continue;
                    };
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
                                scope: scope.clone(),
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
    target: &Target,
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
                let verb_up = verb_txt.to_ascii_uppercase();
                let example = objs
                    .first()
                    .map(|o| format!("{verb_up} {o}(NAME)"))
                    .unwrap_or_else(|| format!("{verb_up} OBJECTTYPE(NAME)"));
                d.push(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::INFORMATION,
                    CODE_UNKNOWN_OBJECT,
                    format!(
                        "{verb_up} needs an object type before the name — e.g. {example}; \
                         {verb_up} accepts: {}",
                        join_capped(objs.iter().copied(), 10)
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
                        "MQSC has no `{verb_up} {up_w}` — {verb_up} accepts these object \
                         types: {}",
                        join_capped(objs.iter().copied(), 12)
                    )
                } else {
                    format!(
                        "MQSC has no `{verb_up} {up_w}` — did you mean {}?",
                        sugs.iter()
                            .map(|s| format!("{verb_up} {s}"))
                            .collect::<Vec<_>>()
                            .join(" or ")
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
                    format!(
                        "MQSC has no command '{verb_txt}' — check the spelling; on a fresh \
                         line completion lists all {} verbs",
                        m.all_verbs().len()
                    )
                } else {
                    format!(
                        "MQSC has no command '{verb_txt}' — did you mean {}?",
                        sugs.join(" or ")
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

    // Code 15 on the command itself: the configured target may not know
    // this whole VERB OBJECT (z/OS-only families, post-version commands)
    check_command_avail(m, target, e, verb_node, obj_word, lines, d);

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

    // Code 14 bookkeeping: curated mandatory-property rules for this DEFINE
    // (empty for every other verb and for objects without curated rules).
    // A rule's `when` clause reads the value actually supplied for one
    // further parameter (e.g. CHLTYPE); those values are recorded at the
    // two value sites inside the loop below.
    let mand: &[_] = if e.verb == "DEFINE" {
        m.mandatory_rules(e.object)
    } else {
        &[]
    };
    let when_params: HashSet<&str> = mand
        .iter()
        .filter_map(|r| r.when.as_ref())
        .map(|w| w.param.as_str())
        .collect();
    let mut when_vals: HashMap<String, String> = HashMap::new();
    // the TARGTYPE gate for object-ref params (z/OS alias TARGET class)
    let targtype = command_targtype(&items, src);

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
                        format!(
                            "'{up}' appears more than once — MQSC uses each parameter at most \
                             once per command. Remove this occurrence or merge the two values."
                        ),
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
                    // code 15: the model knows the parameter, the configured
                    // target may not (one report per use: the group that
                    // follows rides with this word)
                    check_target_avail(m, target, &up, e.object, *n, lines, d);
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
                // A word that is not a parameter of this command but whose
                // availability IS curated: the 2011-based model cannot know
                // modern parameters, the configured target can. With a
                // target that provides it, it is not an unknown parameter;
                // with a target it violates, it gets code 15 instead.
                if target.configured() {
                    if let Some(rule) = m.availability_for(&up, e.object) {
                        if target.gate(rule).is_some() {
                            check_target_avail(m, target, &up, e.object, *n, lines, d);
                        }
                        mute_next_group = true; // the group belongs to it
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
                        "'{w}' is not a parameter, attribute, or object name of {} {} — \
                         completion and hover list what this command accepts",
                        e.verb, e.object
                    )
                } else {
                    format!(
                        "'{w}' is not a parameter of {} {} — did you mean {}?",
                        e.verb,
                        e.object,
                        sugs.join(" or ")
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
                    if when_params.contains(up.as_str()) {
                        if let Some(v) = values.first() {
                            when_vals.insert(
                                up.clone(),
                                unquote_name(node_text(*v, src).trim()).to_ascii_uppercase(),
                            );
                        }
                    }
                    if let Some(&pi) = e.by_name.get(&up) {
                        let p = &e.params[pi];
                        check_values(p, &up, &values, src, lines, d);
                        check_obj_ref(
                            RefSubject {
                                pname: &up,
                                cmd_obj: e.object,
                                targtype: targtype.as_deref(),
                            },
                            &values,
                            src,
                            lines,
                            defs,
                            d,
                        );
                    }
                    if values.is_empty() {
                        let example = e
                            .by_name
                            .get(&up)
                            .map(|&pi| value_example(&e.params[pi]))
                            .unwrap_or_else(|| "value".to_string());
                        d.push(
                            lines.range(n.start_byte(), n.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            format!(
                                "`{up}()` has no value inside — add one, e.g. {up}({example}), \
                                 or remove the empty parameter."
                            ),
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
                    format!(
                        "Unexpected `( … )` group: no parameter stands before these values. \
                         Parentheses carry the object name right after the object type \
                         (`{}(NAME)`) or a value right after a parameter (`PARAM(value)`).",
                        e.object
                    ),
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
                    format!(
                        "'*' belongs to no parameter of {} {} — a '*' pattern is only valid \
                         as the object name, and this command already has one.",
                        e.verb, e.object
                    ),
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
                    check_obj_ref(
                        RefSubject {
                            pname: &up,
                            cmd_obj: e.object,
                            targtype: targtype.as_deref(),
                        },
                        &vs,
                        src,
                        lines,
                        defs,
                        d,
                    );
                    if when_params.contains(up.as_str()) {
                        if let Some(v) = vs.first() {
                            when_vals.insert(
                                up.clone(),
                                unquote_name(node_text(*v, src).trim()).to_ascii_uppercase(),
                            );
                        }
                    }
                }
            }
            Item::Verb(_) => {}
        }
    }

    // pending parameter word without its group
    if let Some((up, n)) = &pending {
        // On DISPLAY every known parameter word doubles as a selectable
        // attribute: `DISPLAY QMGR DESCR` lists that column and is complete
        // MQSC (the 10.0.1 railroads print these words bare). Only the
        // group-form uses (`WHERE(...)`) demand a value, so a bare word on
        // DISPLAY is never a missing-value error.
        if e.verb == "DISPLAY" {
            // no diagnostic
        } else {
            let example = e
                .by_name
                .get(up)
                .map(|&pi| value_example(&e.params[pi]))
                .unwrap_or_else(|| "value".to_string());
            d.push(
                lines.range(n.start_byte(), n.end_byte()),
                DiagnosticSeverity::ERROR,
                CODE_BAD_VALUE,
                format!(
                    "Parameter {up} was given without its value — write {up}({example}); the \
                     value belongs between parentheses."
                ),
            );
        }
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
                    "{} {} is usually given parameter {} — expected something like \
                     {}({})",
                    e.verb,
                    e.object,
                    p.name,
                    p.name,
                    value_example(p)
                ),
            );
        }
    }
    // Missing mandatory properties (code 14): the reference calls certain
    // parameters required ("This parameter is required") while its railroad
    // diagrams bracket them all, so the model's `optional` flag cannot carry
    // that fact — a curated per-object table in the overlay's `mandatory`
    // section does (a documented heuristic like name_max). DEFINE only:
    // ALTER/DELETE act on objects that already carry these properties.
    // Presence of the word is what counts: a bare `TARGET` without its group
    // already reports the code-5 "without its value" error, and a blank
    // sentinel such as the shipped templates' XMITQ(' ') is a supplied value.
    // SYSTEM.* definitions are never flagged — IBM creates those objects and
    // their blank properties were never typed by the user (hint-12 rule).
    let system_obj = name_node
        .map(|n| unquote_name(node_text(n, src).trim()).starts_with("SYSTEM"))
        .unwrap_or(false);
    if !system_obj {
        for rule in mand {
            if let Some(w) = &rule.when {
                match when_vals.get(w.param.as_str()) {
                    Some(v) => {
                        if !w.values.is_empty() && !w.values.iter().any(|x| x == v) {
                            continue;
                        }
                        if w.unless_values.iter().any(|x| x == v) {
                            continue;
                        }
                    }
                    // A value-gated rule needs its gate parameter; when that
                    // parameter is missing, its own unsatisfied rule says so.
                    None if !w.values.is_empty() => continue,
                    None => {}
                }
            }
            if rule.any.iter().any(|p| used.contains(p.as_str())) {
                continue;
            }
            let need = if rule.any.len() == 1 {
                format!("the mandatory parameter {}", rule.any[0])
            } else {
                format!("one of the mandatory parameters {}", rule.any.join(", "))
            };
            let why = if rule.why.is_empty() {
                "the reference marks it required for this object type"
            } else {
                rule.why.as_str()
            };
            let example = if rule.example.is_empty() {
                format!("{} {}(NAME) {}(VALUE)", e.verb, e.object, rule.any[0])
            } else {
                rule.example.clone()
            };
            d.push(
                lines.range(cmd.start_byte(), verb_node.end_byte()),
                DiagnosticSeverity::WARNING,
                CODE_MISSING_MANDATORY,
                format!(
                    "{} {} is missing {need} — {why} Expected something like {example}.",
                    e.verb, e.object
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
            format!(
                "{} {} has no object name yet — write {} {}(NAME), or {} {}('NAME.*') \
                 for a pattern",
                e.verb, e.object, e.verb, e.object, e.verb, e.object
            ),
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
                            "A {} name holds at most {max} characters, but '{shown}{more}' \
                             has {len} — shorten the name.",
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
                    // a quoted number is the right number with the wrong
                    // packaging: name the exact fix instead of just denying
                    let q_inner = if has_string {
                        let inner = unquote_name(raw.trim());
                        (!inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()))
                            .then_some(inner)
                    } else {
                        None
                    };
                    match q_inner {
                        Some(inner) => d.push_fixes(
                            lines.range(v.start_byte(), v.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            format!(
                                "Parameter {up} expects a plain number — `{raw}` is a \
                                 quoted string; remove the quotes: {up}({inner})."
                            ),
                            vec![inner],
                        ),
                        None => d.push(
                            lines.range(v.start_byte(), v.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            format!(
                                "Parameter {up} expects a whole number, e.g. {up}(99) — \
                                 `{raw}` is not an integer."
                            ),
                        ),
                    }
                }
            }
            ValueKind::Enum(vals) => {
                if has_string {
                    // A quoted value can never match an enum keyword — but it
                    // is often exactly the keyword (or a misspelling of it)
                    // with quotes around it: say precisely that.
                    let inner = unquote_name(raw.trim());
                    let ui = inner.to_ascii_uppercase();
                    let list = join_capped(vals.iter().map(|s| s.as_str()), 10);
                    let quoted_int = p.alt_integer
                        && !inner.is_empty()
                        && inner.chars().all(|c| c.is_ascii_digit());
                    let exact = vals.iter().any(|x| *x == ui);
                    let sugs = if exact || quoted_int {
                        Vec::new()
                    } else {
                        model::suggest(&ui, vals.iter().map(|s| s.as_str()), 3)
                    };
                    let (msg, fixes) = if quoted_int {
                        (
                            format!(
                                "Parameter {up} takes plain integers, e.g. {up}({inner}) — \
                                 a quoted `{inner}` is a string, not a number.",
                            ),
                            vec![inner.clone()],
                        )
                    } else if exact {
                        (
                            format!(
                                "Parameter {up}: `{raw}` is quoted, but {} values are plain \
                                 keywords — remove the quotes: {up}({})",
                                up,
                                mirror_case(raw, &ui)
                            ),
                            vec![mirror_case(raw, &ui)],
                        )
                    } else if sugs.is_empty() {
                        (
                            format!(
                                "Parameter {up}: `{raw}` is a quoted string; its values are \
                                 plain keywords, one of: {list}.",
                            ),
                            Vec::new(),
                        )
                    } else {
                        (
                            format!(
                                "Parameter {up}: `{raw}` matches no value — {} accepts the \
                                 plain keywords {list}; did you mean {}? (no quotes)",
                                up,
                                sugs.join(" or ")
                            ),
                            sugs.iter().map(|s| mirror_case(raw, s)).collect(),
                        )
                    };
                    d.push_fixes(
                        lines.range(v.start_byte(), v.end_byte()),
                        DiagnosticSeverity::WARNING,
                        CODE_BAD_VALUE,
                        msg,
                        fixes,
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
                        let list = join_capped(vals.iter().map(|s| s.as_str()), 10);
                        let msg = if sugs.is_empty() {
                            format!(
                                "Parameter {up} accepts only these values: {list} — `{raw}` \
                                 is not one of them."
                            )
                        } else {
                            format!(
                                "Parameter {up}: `{raw}` is not one of its accepted values \
                                 ({list}) — did you mean {}?",
                                sugs.join(" or ")
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
                let noun = match &p.kind {
                    ValueKind::Integer => "a whole number".to_string(),
                    ValueKind::Enum(vals) => {
                        format!(
                            "one of its listed keywords ({})",
                            join_capped(vals.iter().map(|s| s.as_str()), 6)
                        )
                    }
                    _ => "a plain value".to_string(),
                };
                d.push(
                    lines.range(v.start_byte(), v.end_byte()),
                    DiagnosticSeverity::WARNING,
                    CODE_WILDCARD,
                    format!(
                        "Parameter {up}: `{raw}` uses a wildcard, but this value must be \
                         {noun} — remove '*', '+' and '?' from it.",
                    ),
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
                            format!(
                                "The '?' wildcard only works in DISPLAY name patterns — \
                                 this command is `{verb}`; use '*' for a multi-character \
                                 match here."
                            ),
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
        // Summarise only the object this appearance NAMES: when a queue
        // and a channel share the spelling, hovering the channel must not
        // mix in the queue's definition or uses.
        let ns = site.scope.namespace();
        let uses = idx
            .sites_in(&site.name, ns)
            .filter(|s| s.defines.is_none())
            .count();
        let defs = idx.definitions_in(&site.name, ns);
        let md = if defs.is_empty() {
            // not defined in this document — is it DEFINEd in another open
            // one? (T-17) Same-namespace entries only.
            let ext: Vec<&(String, u32, String)> = ext_defs
                .get(&site.name)
                .map(|v| v.iter().filter(|e| type_namespace(&e.2) == ns).collect())
                .unwrap_or_default();
            match ext.first() {
                Some((file, line, _)) => {
                    let types: Vec<&str> = ext.iter().map(|(_, _, t)| t.as_str()).collect();
                    format!(
                        "**{} `{}`** — DEFINEd at line {} of `{}` (another open document){}\n\n\
                         {} {} in this script.",
                        types.join("/"),
                        site.name,
                        line + 1,
                        file,
                        if ext.len() > 1 {
                            format!(" — {} DEFINE locations there", ext.len())
                        } else {
                            String::new()
                        },
                        uses,
                        if uses == 1 { "use" } else { "uses" },
                    )
                }
                None => format!(
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
        // curated availability facts (overlay), informational like the
        // parameter card: shown independent of any configured target
        if let Some(r) = m.command_availability_for(e.verb, e.object) {
            let mut bits: Vec<String> = Vec::new();
            if let Some(pl) = r
                .platform
                .as_deref()
                .and_then(crate::settings::Platform::parse)
            {
                bits.push(format!("{pl} only"));
            }
            if let Some(s) = r.since.as_deref() {
                bits.push(format!("from MQ {s}"));
            }
            if let Some(u) = r.until.as_deref() {
                bits.push(format!("up to MQ {u}"));
            }
            if !bits.is_empty() {
                md.push_str(&format!(
                    "\n\n_Availability (curated): {}._",
                    bits.join(", ")
                ));
            }
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
    let base = if !p.description.is_empty() {
        p.description.clone()
    } else {
        m.param_doc(&p.name, obj).unwrap_or("").to_string()
    };
    // curated availability facts (overlay), shown wherever the parameter is
    // documented — informational, independent of any configured target
    let avail = match m.availability_for(&p.name, obj) {
        Some(r) => {
            let mut bits: Vec<String> = Vec::new();
            if let Some(pl) = r
                .platform
                .as_deref()
                .and_then(crate::settings::Platform::parse)
            {
                bits.push(format!("available on {pl} only"));
            }
            if let Some(s) = r.since.as_deref() {
                bits.push(format!("from MQ {s}"));
            }
            if let Some(u) = r.until.as_deref() {
                bits.push(format!("up to MQ {u}"));
            }
            if bits.is_empty() {
                String::new()
            } else {
                format!("\n\n_Availability (curated): {}._", bits.join(", "))
            }
        }
        None => String::new(),
    };
    format!("{base}{avail}")
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
                            let tt = command_targtype(&command_items(cmd), src);
                            let scope = ref_scope(&w, r, e.object, tt.as_deref());
                            let mut defs = collect_definitions(tree, src, m);
                            defs.add_external(extra_defs);
                            out.extend(name_items(&names_for(&defs, &scope)));
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
    let want = ObjectScope::for_object_type(cmd_obj);
    let pairs: Vec<(&str, &str)> = defs
        .by_obj
        .iter()
        .filter(|(obj, _)| want.accepts_type(obj))
        .flat_map(|(obj, ns)| ns.iter().map(|n| (obj.as_str(), n.as_str())))
        .collect();
    name_items(&pairs)
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

/// Tokens overlapping `range`, delta-encoded against the DOCUMENT origin.
/// The LSP encodes a range result exactly like a full one: clients
/// accumulate deltas from the document start, so the first token must
/// carry its absolute line/character (encoding from `range.start` shifts
/// every token as soon as the first token sits past the range start —
/// which is the normal case, since ranges rarely begin on token edges).
/// A token merely crossing the range border belongs to the range: the
/// client clips it.
pub fn semantic_tokens_range(text: &str, tree: &Tree, range: lsp::Range) -> Vec<SemanticToken> {
    let (sl, sc) = (range.start.line as usize, range.start.character as usize);
    let (el, ec) = (range.end.line as usize, range.end.character as usize);
    let raw: Vec<_> = raw_tokens(text, tree)
        .into_iter()
        .filter(|(l, c, len, ..)| {
            // token [start, end) vs range [start, end): overlap iff
            // token_start < range_end && token_end > range_start
            let starts_before_end = *l < el || (*l == el && *c < ec);
            let ends_after_start = *l > sl || (*l == sl && *c + *len > sc);
            starts_before_end && ends_after_start
        })
        .collect();
    encode_tokens(raw, 0, 0)
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

    /// Same under a configured target (T-19): availability rules are live.
    fn codes_t(text: &str, target: &Target) -> Vec<i32> {
        let m = Model::load();
        let tree = parse(text);
        let uri = Url::parse("file:///fixture.mqsc").unwrap();
        analyze_with_target(text, &tree, m, &uri, &Definitions::default(), target)
            .into_iter()
            .filter_map(|d| match d.code {
                Some(lsp::NumberOrString::Number(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn target(version: &str, platform: &str) -> Target {
        Target {
            version: crate::settings::MqVersion::parse(version),
            platform: crate::settings::Platform::parse(platform),
        }
    }

    /// Message of the first `code` diagnostic under a target.
    fn msg_t(text: &str, code: i32, target: &Target) -> String {
        let m = Model::load();
        let tree = parse(text);
        analyze_with_target(
            text,
            &tree,
            m,
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
            target,
        )
        .into_iter()
        .find(|d| d.code == Some(lsp::NumberOrString::Number(code)))
        .unwrap_or_else(|| panic!("no diagnostic {code} in {text:?}"))
        .message
    }

    /// Message of the first diagnostic with `code`, for message-contract tests.
    fn msg(text: &str, code: i32) -> String {
        analyze(
            text,
            &parse(text),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        )
        .into_iter()
        .find(|d| d.code == Some(lsp::NumberOrString::Number(code)))
        .unwrap_or_else(|| panic!("no diagnostic {code} in {text:?}"))
        .message
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

    /// Decode a delta-encoded token stream back to absolute document
    /// positions `(line, col, len, type, mods)` for position assertions.
    fn decode_tokens(tokens: &[SemanticToken]) -> Vec<(usize, usize, usize, u32, u32)> {
        let mut line = 0usize;
        let mut col = 0usize;
        tokens
            .iter()
            .map(|t| {
                line += t.delta_line as usize;
                col = if t.delta_line == 0 {
                    col + t.delta_start as usize
                } else {
                    t.delta_start as usize
                };
                (
                    line,
                    col,
                    t.length as usize,
                    t.token_type,
                    t.token_modifiers_bitset,
                )
            })
            .collect()
    }

    #[test]
    fn semantic_tokens_range_encodes_document_absolute_positions() {
        // The plan's acceptance case: range starts at line 100, the first
        // token inside it sits at line 103. A range response is delta
        // chains from the DOCUMENT start (clients accumulate it the same
        // way as a full response), so the decoded line must be 103 — an
        // encoding based at range.start would report every token ~3 lines
        // too early.
        let mut text = "\n".repeat(100);
        text.push_str("DEFINE QLOCAL(PAD1)\n\n\nDEFINE QLOCAL(PAD2)\n"); // PAD1 L100, PAD2 L103
        let tree = parse(&text);
        let range = lsp::Range::new(lsp::Position::new(100, 22), lsp::Position::new(103, 26));
        let decoded = decode_tokens(&semantic_tokens_range(&text, &tree, range));
        // PAD1 (cols 14..18) ends before the range start at col 22: the
        // first token must be PAD2's command on line 103, absolutely placed
        assert_eq!(decoded[0].0, 103, "first token line: {decoded:?}");
        assert_eq!((decoded[0].1, decoded[0].2), (0, 6)); // DEFINE at 103:0
    }

    #[test]
    fn semantic_tokens_range_includes_border_crossing_tokens() {
        let text = "DEFINE QLOCAL(SHOP.FRONT)\n";
        let tree = parse(text);
        // The range begins inside SHOP.FRONT (14..24): the token overlaps
        // and the client clips it.
        let start_mid = decode_tokens(&semantic_tokens_range(
            text,
            &tree,
            lsp::Range::new(lsp::Position::new(0, 18), lsp::Position::new(0, 25)),
        ));
        assert!(
            start_mid.iter().any(|t| *t == (0, 14, 10, 4, 0)),
            "name token missing: {start_mid:?}"
        );
        // The range ends inside the same token: still included.
        let end_mid = decode_tokens(&semantic_tokens_range(
            text,
            &tree,
            lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 20)),
        ));
        assert!(
            end_mid.iter().any(|t| t.1 == 14 && t.2 == 10),
            "name token missing: {end_mid:?}"
        );
        // A range strictly between tokens (inside the parens gap after the
        // name) keeps the name out: overlap, not proximity.
        let outside = decode_tokens(&semantic_tokens_range(
            text,
            &tree,
            lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 7)),
        ));
        assert!(
            !outside.iter().any(|t| t.1 == 14),
            "token outside the range leaked in: {outside:?}"
        );
    }

    #[test]
    fn semantic_tokens_full_and_range_agree_per_token() {
        // Same document, several ranges: every token a range response
        // reports must carry exactly the full response's absolute position
        // and type for the same token — VS Code highlights a token
        // identically whichever request surfaced it.
        let text = "DEFINE QLOCAL(Q1) DESCR('It works')\nDEFINE QALIAS(A2) TARGET(Q1) REPLACE\nALTER QLOCAL('Q1') MAXDEPTH(99)\n";
        let tree = parse(text);
        let full = decode_tokens(&semantic_tokens(text, &tree));
        for range in [
            lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(2, 30)),
            lsp::Range::new(lsp::Position::new(1, 5), lsp::Position::new(1, 30)),
            lsp::Range::new(lsp::Position::new(0, 12), lsp::Position::new(2, 10)),
        ] {
            let (sl, sc) = (range.start.line as usize, range.start.character as usize);
            let (el, ec) = (range.end.line as usize, range.end.character as usize);
            let want: Vec<_> = full
                .iter()
                .filter(|(l, c, len, ..)| {
                    (*l < el || (*l == el && *c < ec)) && (*l > sl || (*l == sl && *c + *len > sc))
                })
                .copied()
                .collect();
            let got = decode_tokens(&semantic_tokens_range(text, &tree, range));
            assert_eq!(got, want, "range {range:?} diverges from full response");
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
    fn bare_param_word_on_display_is_an_attribute_selector() {
        // Oracle: DISPLAY railroads print known parameter words bare
        // (`DISPLAY QMGR DESCR`, `DISPLAY CHLAUTH(*) MATCH`): valid MQSC,
        // never a missing-value error. Non-DISPLAY verbs still flag it.
        assert!(!has("DISPLAY QMGR DESCR\n", CODE_BAD_VALUE));
        assert!(!has("DISPLAY CHLAUTH(*) MATCH EXACT\n", CODE_BAD_VALUE));
        assert!(has("ALTER QLOCAL(A) DESCR\n", CODE_BAD_VALUE));
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
    fn code12_existence_is_namespace_qualified() {
        // "defined" means DEFINEd as an object this parameter could point
        // at — the ObjectScope rule goto-definition uses, not bare spelling.
        // A channel named SHOP.FRONT does not satisfy a TARGET looking for
        // a queue, and a same-named queue is the only thing that does.
        assert!(has(
            "DEFINE CHANNEL(SHOP.FRONT) CHLTYPE(SVRCONN)\nDEFINE QALIAS(A) TARGET(SHOP.FRONT)\n",
            CODE_UNDEFINED_OBJECT
        ));
        assert!(!has(
            "DEFINE CHANNEL(SHOP.FRONT) CHLTYPE(SVRCONN)\nDEFINE QLOCAL(SHOP.FRONT)\nDEFINE QALIAS(A) TARGET(SHOP.FRONT)\n",
            CODE_UNDEFINED_OBJECT
        ));
        // the reverse: LIKE on a CHANNEL wants a CHANNEL template — a QLOCAL
        // of the same spelling is a different object and leaves it dangling
        assert!(has(
            "DEFINE QLOCAL(MY.MODEL)\nDEFINE CHANNEL(C) CHLTYPE(SDR) TRPTYPE(TCP) LIKE(MY.MODEL)\n",
            CODE_UNDEFINED_OBJECT
        ));
        assert!(!has(
            "DEFINE QLOCAL(MY.MODEL)\nDEFINE CHANNEL(MY.MODEL) CHLTYPE(SDR) TRPTYPE(TCP)\nDEFINE CHANNEL(C) CHLTYPE(SDR) TRPTYPE(TCP) LIKE(MY.MODEL)\n",
            CODE_UNDEFINED_OBJECT
        ));
    }

    #[test]
    fn code12_targtype_topic_points_target_at_topics() {
        // z/OS alias TARGTYPE(TOPIC): the TARGET names a TOPIC — a defined
        // topic satisfies it (the dmpmqcfg topic-publishing pattern that a
        // plain queue-family check flagged 2,328 times) …
        assert!(!has(
            "DEFINE TOPIC(P) TOPICSTR('t')\nDEFINE QALIAS(A) TARGTYPE(TOPIC) TARGET(P)\n",
            CODE_UNDEFINED_OBJECT
        ));
        // … while a queue of that spelling does not, once the script proves
        // it tracks topics at all (arms() convention)
        assert!(has(
            "DEFINE TOPIC(OTHER) TOPICSTR('o')\nDEFINE QLOCAL(P)\nDEFINE QALIAS(A) TARGTYPE(TOPIC) TARGET(P)\n",
            CODE_UNDEFINED_OBJECT
        ));
        // without the gate TARGET stays a queue reference
        assert!(!has(
            "DEFINE QLOCAL(P)\nDEFINE QALIAS(A) TARGET(P)\n",
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
    fn code14_qalias_needs_target() {
        assert!(has(
            "DEFINE QALIAS(A1) DEFPSIST(YES)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(has("DEFINE QALIAS(A1)\n", CODE_MISSING_MANDATORY));
        // a real target — or a template to copy one from — is fine
        assert!(!has(
            "DEFINE QALIAS(A1) TARGET(BASE.Q)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE QALIAS(A1) LIKE(OTHER.A) REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
        // a group-less word counts as supplied (code 5 already flags it)
        assert!(!has("DEFINE QALIAS(A1) TARGET\n", CODE_MISSING_MANDATORY));
        // ALTER acts on an alias that already carries its target
        assert!(!has(
            "ALTER QALIAS(A1) DESCR('x')\n",
            CODE_MISSING_MANDATORY
        ));
        // SYSTEM.* objects are IBM-created with blank defaults: never flagged
        assert!(!has(
            "DEFINE QALIAS('SYSTEM.DEFAULT.ALIAS.QUEUE') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
        // verb/object synonyms resolve to the same rule
        assert!(has("DEF QA(A1) DEFPSIST(YES)\n", CODE_MISSING_MANDATORY));
        // the message keeps the Elm contract: token, why, concrete example
        let m = msg("DEFINE QALIAS(A1) DEFPSIST(YES)\n", CODE_MISSING_MANDATORY);
        assert!(m.contains("TARGET"), "{m}");
        assert!(m.contains("DEFINE QALIAS(ALIAS.ONE) TARGET(BASE.Q)"), "{m}");
    }

    #[test]
    fn code14_qremote_needs_rname_or_rqname() {
        // a real transmission queue but no remote mapping — the flagged case
        assert!(has(
            "DEFINE QREMOTE(R1) XMITQ(XQ)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(has("DEFINE QREMOTE(R1)\n", CODE_MISSING_MANDATORY));
        assert!(!has(
            "DEFINE QREMOTE(R1) RNAME(ORDERS) XMITQ(XQ)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE QREMOTE(R1) RQMNAME(REM.QM)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE QREMOTE(R1) LIKE(SYSTEM.DEF.QREMOTE)\n",
            CODE_MISSING_MANDATORY
        ));
        // cluster alias qremote: the documented pattern with a deliberately
        // blank RQMNAME (corpus-proven, so exempt via CLUSTER/CLUSNL)
        assert!(!has(
            "DEFINE QREMOTE('ANY.CLUSTER') CLUSNL('VCCK') DEFBIND(NOTFIXED) XMITQ(' ') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
        // generic qremote form: blank XMITQ(' ') marks a deliberately blank mapping
        assert!(!has(
            "DEFINE QREMOTE('VCC.CLUSTER') DESCR('QMGR Alias for all clusters') XMITQ(' ') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
        // SYSTEM.* names are never flagged
        assert!(!has(
            "DEFINE QREMOTE('SYSTEM.DEFAULT.REMOTE.QUEUE') XMITQ(' ') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
    }

    #[test]
    fn code14_channel_chltype_then_xmitq() {
        // CHLTYPE itself is the first gate
        assert!(has(
            "DEFINE CHANNEL(A.TO.B) XMITQ(Q)\n",
            CODE_MISSING_MANDATORY
        ));
        // SDR and SVR each need XMITQ (CONNAME is deliberately unchecked:
        // the corpus proves the command server accepts definitions without it)
        assert!(has(
            "DEFINE CHANNEL(A.TO.B) CHLTYPE(SDR) CONNAME('h(1414)')\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(has(
            "DEFINE CHANNEL(F.C) CHLTYPE(SVR) CONNAME('h(1414)')\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE CHANNEL(A.TO.B) CHLTYPE(SDR) XMITQ(B.TO.A) CONNAME('h(1414)')\n",
            CODE_MISSING_MANDATORY
        ));
        // the shipped templates' blank XMITQ(' ') counts as supplied
        assert!(!has(
            "DEFINE CHANNEL('SYSTEM.DEF.SENDER') CHLTYPE(SDR) TRPTYPE(TCP) XMITQ(' ') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
        // other channel types never need XMITQ
        assert!(!has(
            "DEFINE CHANNEL(A.CL) CHLTYPE(CLUSSDR) TRPTYPE(TCP) CLUSTER(CL1)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE CHANNEL(B.FROM.C) CHLTYPE(CLUSRCVR) TRPTYPE(TCP) CLUSTER(CL1)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE CHANNEL(F.C) CHLTYPE(SVRCONN) TRPTYPE(TCP)\n",
            CODE_MISSING_MANDATORY
        ));
    }

    #[test]
    fn code14_topic_needs_topicstr() {
        assert!(has(
            "DEFINE TOPIC(T1) PUB(ENABLED)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE TOPIC(T1) TOPICSTR('weather/europe')\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE TOPIC(T1) LIKE(SYSTEM.DEF.TOPIC)\n",
            CODE_MISSING_MANDATORY
        ));
    }

    #[test]
    fn code14_sub_needs_topic_and_destination() {
        // both the topic rule and the DEST rule are unmet
        assert!(has("DEFINE SUB(S1) DURSUB(YES)\n", CODE_MISSING_MANDATORY));
        // a topic from a string, an object, or both
        assert!(!has(
            "DEFINE SUB(S1) TOPICSTR('weather/#') DEST(LOCAL.Q)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(!has(
            "DEFINE SUB(S1) TOPICOBJ(T1) DEST(LOCAL.Q)\n",
            CODE_MISSING_MANDATORY
        ));
        // DESTCLAS(MANAGED) has MQ create the destination; PROVIDED still
        // demands an explicit DEST
        assert!(!has(
            "DEFINE SUB(S1) TOPICSTR('weather/#') DESTCLAS(MANAGED)\n",
            CODE_MISSING_MANDATORY
        ));
        assert!(has(
            "DEFINE SUB(S1) TOPICSTR('weather/#') DESTCLAS(PROVIDED)\n",
            CODE_MISSING_MANDATORY
        ));
        // the shipped SYSTEM.DEFAULT.SUB template carries no DEST
        assert!(!has(
            "DEFINE SUB('SYSTEM.DEFAULT.SUB') TOPICSTR('') REPLACE\n",
            CODE_MISSING_MANDATORY
        ));
    }

    #[test]
    fn code14_buffpool_needs_buffers() {
        assert!(has("DEFINE BUFFPOOL(25)\n", CODE_MISSING_MANDATORY));
        assert!(!has(
            "DEFINE BUFFPOOL(25) BUFFERS(5000)\n",
            CODE_MISSING_MANDATORY
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
    fn object_scope_namespace_rules() {
        // every queue kind shares ONE namespace; the queue namespace and a
        // channel (or topic, process, ...) never collide
        assert_eq!(type_namespace("QLOCAL"), "QUEUE");
        assert_eq!(type_namespace("QCLUSTER"), "QUEUE");
        assert_eq!(type_namespace("QUEUE"), "QUEUE");
        assert_eq!(type_namespace("CHANNEL"), "CHANNEL");
        let q = ObjectScope::for_object_type("QALIAS");
        assert_eq!(q, ObjectScope::QueueFamily);
        assert_eq!(q.namespace(), "QUEUE");
        assert!(q.accepts_type("QLOCAL") && q.accepts_type("QREMOTE") && q.accepts_type("QUEUE"));
        assert!(!q.accepts_type("CHANNEL") && !q.accepts_type("TOPIC"));
        let c = ObjectScope::for_object_type("CHANNEL");
        assert_eq!(c, ObjectScope::Exact("CHANNEL".to_string()));
        assert!(c.accepts_type("CHANNEL"));
        assert!(!c.accepts_type("QLOCAL"));
        // LIKE on a queue command is the queue family; on other commands it
        // names the command's own class
        assert_eq!(
            obj_ref_scope(ObjRef::SameType, "QMODEL"),
            ObjectScope::QueueFamily
        );
        assert_eq!(
            obj_ref_scope(ObjRef::SameType, "CHANNEL"),
            ObjectScope::Exact("CHANNEL".to_string())
        );
    }

    #[test]
    fn object_index_sites_carry_their_namespace() {
        // the plan's conflation case: a queue and a channel with one name
        let text = "DEFINE QLOCAL(SHOP.FRONT)\nDEFINE CHANNEL(SHOP.FRONT) CHLTYPE(SVRCONN)\nDEFINE QALIAS(A) TARGET(SHOP.FRONT)\nDISPLAY CHANNEL(SHOP.FRONT)\n";
        let idx = index_of(text);
        // queue side: the QLOCAL DEFINE plus the TARGET use — the channel
        // pair never joins queue results
        let queue: Vec<_> = idx.sites_in("SHOP.FRONT", "QUEUE").collect();
        assert_eq!(queue.len(), 2, "{queue:?}");
        assert_eq!(queue[0].defines.as_deref(), Some("QLOCAL"));
        assert!(queue[1].defines.is_none());
        assert_eq!(queue[1].range.start.line, 2); // TARGET(SHOP.FRONT)
                                                  // channel side: its DEFINE plus the DISPLAY use
        let chan: Vec<_> = idx.sites_in("SHOP.FRONT", "CHANNEL").collect();
        assert_eq!(chan.len(), 2, "{chan:?}");
        assert_eq!(chan[0].defines.as_deref(), Some("CHANNEL"));
        assert_eq!(chan[1].range.start.line, 3);
        // the cursor's symbol identity: same NAME, different object
        let from_target = idx.symbol_at(Position::new(2, 25)).expect("TARGET name");
        assert_eq!(
            (from_target.name.as_str(), from_target.scope.namespace()),
            ("SHOP.FRONT", "QUEUE")
        );
        let from_channel = idx.symbol_at(Position::new(1, 16)).expect("channel name");
        assert_eq!(
            (from_channel.name.as_str(), from_channel.scope.namespace()),
            ("SHOP.FRONT", "CHANNEL")
        );
    }

    #[test]
    fn targtype_topic_directs_target_site_at_the_topic_namespace() {
        // the alias TARGET sits in the TOPIC namespace, not the queue one:
        // same spelling, the topic is the referenced object
        let text = "DEFINE TOPIC(PILOT)\nDEFINE QLOCAL(PILOT)\nDEFINE QALIAS(A) TARGTYPE(TOPIC) TARGET(PILOT)\n";
        let idx = index_of(text);
        let t = idx.symbol_at(Position::new(2, 42)).expect("TARGET name");
        assert_eq!((t.name.as_str(), t.scope.namespace()), ("PILOT", "TOPIC"));
        let defs = idx.definitions_in("PILOT", "TOPIC");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].range.start.line, 0);
        // the queue still owns its own namespace
        assert_eq!(idx.definitions_in("PILOT", "QUEUE").len(), 1);
    }

    #[test]
    fn hover_summarises_the_object_of_its_namespace() {
        let text = "DEFINE QLOCAL(SHOP.FRONT)\nDEFINE CHANNEL(SHOP.FRONT) CHLTYPE(SVRCONN)\nDEFINE QALIAS(A) TARGET(SHOP.FRONT)\n";
        let m = Model::load();
        let tree = parse(text);
        let md_of = |h: Hover| match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        // on the channel DEFINE: speaks about the channel object alone
        let md =
            md_of(hover(text, &tree, m, Position::new(1, 16), &Default::default()).expect("ch"));
        assert!(
            md.contains("CHANNEL") && md.contains("this is the definition"),
            "{md}"
        );
        assert!(
            !md.contains("QLOCAL"),
            "queue leaked into channel hover: {md}"
        );
        // on the TARGET use: resolves to the QUEUE definition (line 1), not the channel
        let md =
            md_of(hover(text, &tree, m, Position::new(2, 25), &Default::default()).expect("t"));
        assert!(
            md.contains("QLOCAL") && md.contains("DEFINEd at line 1"),
            "{md}"
        );
        assert!(
            !md.contains("CHANNEL"),
            "channel leaked into queue hover: {md}"
        );
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

    #[test]
    fn messages_quote_the_token_and_show_the_fix() {
        // Elm-style message contract: name the offending token, say what was
        // expected, and show the concrete fix (example form or suggestion).
        // quoted enum keyword: exact diagnosis + quote-free rewrite shown
        let d = msg("DEFINE QLOCAL(Q1) QSGDISP('QMGR')\n", CODE_BAD_VALUE);
        assert!(
            d.contains("plain keywords") && d.contains("remove the quotes"),
            "{d}"
        );
        // quoted integer for an integer parameter: same treatment
        assert!(
            msg("ALTER QLOCAL(Q1) MAXDEPTH('10')\n", CODE_BAD_VALUE).contains("remove the quotes")
        );
        // missing name shows the command shape to write
        let d = msg("DEFINE QLOCAL\n", CODE_MISSING_NAME);
        assert!(d.contains("DEFINE QLOCAL(NAME)"), "{d}");
        // unknown object: suggests the real command, whole verb+object span
        assert!(msg("DEFINE QUBLOCAL(A)\n", CODE_UNKNOWN_OBJECT).contains("DEFINE QLOCAL"));
        // a parameter left without its group shows a filled-in example
        assert!(msg("DEFINE QLOCAL(Q1) MAXDEPTH\n", CODE_BAD_VALUE).contains("MAXDEPTH(99)"));
    }

    #[test]
    fn undefined_name_hint_suggests_closest_defined_names() {
        let text = "DEFINE QLOCAL(GHOST)\nDEFINE QALIAS(A) TARGET(GOST)\n";
        let diags = analyze(
            text,
            &parse(text),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let d = diags
            .iter()
            .find(|d| d.code == Some(lsp::NumberOrString::Number(CODE_UNDEFINED_OBJECT)))
            .expect("GOST must be hinted");
        assert!(
            d.message.contains("closest names DEFINEd there are GHOST"),
            "{}",
            d.message
        );
        // the suggestion also rides along as a one-click quickfix
        assert_eq!(
            d.data.as_ref().and_then(|v| v.get("quickfixes")),
            Some(&serde_json::json!(["GHOST"])),
            "{:?}",
            d.data
        );
    }

    #[test]
    fn boqname_is_known_after_the_overlay_backout_is_not() {
        // CURATED overlay: BOQNAME takes a queue name everywhere the base
        // model carries it — none of these may read as unknown parameter (4)
        // or bad value (5). The live MQ 10.0.1 oracle confirms
        // BOQNAME( string ); it also shows there is no BACKOUT parameter at
        // all, so that spelling must read as an unknown parameter.
        let text = "DEFINE QLOCAL(L) BOQNAME(BQ)\nALTER QLOCAL(L) BOQNAME(BQ)\n\
                    ALTER QMODEL(M) BOQNAME(BQ)\n";
        let diags = analyze(
            text,
            &parse(text),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let bad: Vec<_> = diags
            .iter()
            .filter(|d| {
                d.code == Some(lsp::NumberOrString::Number(CODE_UNKNOWN_PARAM))
                    || d.code == Some(lsp::NumberOrString::Number(CODE_BAD_VALUE))
            })
            .collect();
        assert!(bad.is_empty(), "BOQNAME must be accepted: {bad:?}");
        // The reverted spelling: unknown parameter, nowhere.
        let c = codes("DEFINE QLOCAL(L) BACKOUT(BQ)\n");
        assert!(c.contains(&CODE_UNKNOWN_PARAM), "{c:?}");
    }

    #[test]
    fn backout_params_register_rename_sites() {
        // Renaming a queue must rewrite references hiding in backout/dead/
        // default-xmit params.
        let text = "DEFINE QLOCAL(BOQ)\nDEFINE QLOCAL(ORDERS) BOQNAME(BOQ)\n\
                    ALTER QLOCAL(ORDERS) BOQNAME(BOQ)\nALTER QMODEL(M) BOQNAME(BOQ)\n\
                    ALTER QMGR DEADQ(BOQ) DEFXMITQ(BOQ)\n";
        let idx = index_of(text);
        let a: Vec<_> = idx.sites_of("BOQ").collect();
        assert_eq!(a.len(), 6, "{a:?}");
        assert_eq!(a[0].defines.as_deref(), Some("QLOCAL"));
        assert!(a[1..].iter().all(|s| s.defines.is_none()));
        let lines: Vec<u32> = a[1..].iter().map(|s| s.range.start.line).collect();
        assert_eq!(lines, vec![1, 2, 3, 4, 4]);
    }

    #[test]
    fn backout_refs_are_checked_special_values_are_not() {
        let text = "DEFINE QLOCAL(BOQ)\nDEFINE QLOCAL(ORDERS) BOQNAME(GHOST)\n\
                    ALTER QLOCAL(ORDERS) BOQNAME(NO)\nDEFINE QLOCAL(M1) INITQ(GHOST)\n\
                    START CHINIT(A) INITQ(B)\nALTER QMGR DEADQ(NONE)\n";
        let diags = analyze(
            text,
            &parse(text),
            Model::load(),
            &Url::parse("file:///t.mqsc").unwrap(),
            &Definitions::default(),
        );
        let hint_lines: Vec<u32> = diags
            .iter()
            .filter(|d| d.code == Some(lsp::NumberOrString::Number(CODE_UNDEFINED_OBJECT)))
            .map(|d| d.range.start.line)
            .collect();
        // BOQNAME(GHOST) and the QLOCAL INITQ(GHOST) are flagged; the z/OS
        // BOQNAME(NO) and DEADQ(NONE) special values are not, and the INITQ
        // of START CHINIT names a queue MANAGER, not a queue (no hint).
        assert_eq!(hint_lines, vec![1, 3], "{diags:?}");
    }

    // ------------------------------------------------- T-19 target availability

    #[test]
    fn no_target_means_no_availability_checks() {
        // pre-Phase-4 behavior: a z/OS-only parameter on an untargeted
        // script is nobody's business
        assert!(!has(
            "DEFINE QLOCAL(A) QSGDISP(GROUP)\n",
            CODE_NOT_IN_TARGET
        ));
        // and the plain helpers stay target-less for any other param too
        assert!(!has("DEFINE QLOCAL(A) BOQNAME(QM1)\n", CODE_NOT_IN_TARGET));
    }

    #[test]
    fn wrong_platform_flags_and_names_the_other_platform() {
        // QSGDISP is the oracle-verified z/OS-only rule: the reference says
        // 'applies to z/OS only' and the 10.0.1 distributed parser rejects
        // it on all 40 model pairs that carry it.
        let t = target("9.4", "distributed");
        let c = codes_t("DEFINE QLOCAL(A) QSGDISP(GROUP)\n", &t);
        assert!(c.contains(&CODE_NOT_IN_TARGET), "{c:?}");
        let m = msg_t("DEFINE QLOCAL(A) QSGDISP(GROUP)\n", CODE_NOT_IN_TARGET, &t);
        assert!(m.contains("z/OS") && m.contains("platform"), "{m}");
        // and on z/OS the same script is clean for that param
        let z = target("9.4", "zos");
        assert!(!codes_t("DEFINE QLOCAL(A) QSGDISP(GROUP)\n", &z).contains(&CODE_NOT_IN_TARGET));
    }

    #[test]
    fn obj_restricted_rule_only_fires_for_those_objects() {
        // No shipped rule is obj-restricted right now (the former BACKOUT
        // obj rule was disproved by the oracle and removed), so the
        // lookup-level contract is what can be asserted: a general rule
        // resolves for any object that carries the parameter, and a
        // parameter with no rule resolves to None — on QMODEL the reverted
        // BACKOUT is simply an unknown parameter (code 4), never 15.
        let m = Model::load();
        assert!(m.availability_for("QSGDISP", "QLOCAL").is_some());
        assert!(m.availability_for("BACKOUT", "QLOCAL").is_none());
        let t = target("9.0", "distributed");
        let c = codes_t("ALTER QMODEL(M) BACKOUT(Q)\n", &t);
        assert!(c.contains(&CODE_UNKNOWN_PARAM), "{c:?}");
        assert!(!c.contains(&CODE_NOT_IN_TARGET), "{c:?}");
    }

    // ------------------------------------------- T-19 command-level rules

    #[test]
    fn zos_only_command_flagged_under_distributed_target() {
        // the command_availability families (live-oracle evidence: these
        // pairs are absent from distributed runmqsc's own command lists)
        let t = target("9.4", "distributed");
        let c = codes_t("DEFINE CFSTRUCT(SCI) CF_TYPE(S)\n", &t);
        assert!(c.contains(&CODE_NOT_IN_TARGET), "{c:?}");
        let msg = msg_t("DEFINE CFSTRUCT(SCI) CF_TYPE(S)\n", CODE_NOT_IN_TARGET, &t);
        assert!(
            msg.contains("'DEFINE CFSTRUCT'") && msg.contains("z/OS") && msg.contains("platform"),
            "{msg}"
        );
        // the whole family flags, one report per command...
        assert!(codes_t("MOVE QLOCAL(Q) FROM(BOQ)\n", &t).contains(&CODE_NOT_IN_TARGET));
        // ...and on z/OS the very same scripts are clean
        let z = target("9.4", "zos");
        assert!(!codes_t(
            "DEFINE CFSTRUCT(SCI) CF_TYPE(S)\nMOVE QLOCAL(Q) FROM(BOQ)\n",
            &z
        )
        .contains(&CODE_NOT_IN_TARGET));
    }

    #[test]
    fn command_version_floor_follows_target_version() {
        // SET/DISPLAY CHLAUTH: distributed commands, but only from MQ 9.0
        let old = target("8.0", "distributed");
        assert!(
            codes_t("SET CHLAUTH(*) TYPE(BLOCKUSER) USERLIST(x)\n", &old)
                .contains(&CODE_NOT_IN_TARGET)
        );
        let msg = msg_t(
            "SET CHLAUTH(*) TYPE(BLOCKUSER) USERLIST(x)\n",
            CODE_NOT_IN_TARGET,
            &old,
        );
        assert!(
            msg.contains("from MQ 9.0") && msg.contains("targetVersion"),
            "{msg}"
        );
        // at exactly the floor (and above) the command is clean
        let ok = target("9.0", "distributed");
        assert!(
            !codes_t("SET CHLAUTH(*) TYPE(BLOCKUSER) USERLIST(x)\n", &ok)
                .contains(&CODE_NOT_IN_TARGET)
        );
    }

    #[test]
    fn command_rules_never_fire_without_a_target() {
        // the settings gate covers commands exactly like parameters
        assert!(!has(
            "DEFINE CFSTRUCT(SCI)\nMOVE QLOCAL(Q) FROM(BOQ)\n",
            CODE_NOT_IN_TARGET
        ));
    }

    #[test]
    fn hover_command_card_shows_curated_availability() {
        // informational, independent of any configured target (same
        // convention as the parameter card's 'Availability (curated)')
        let m = Model::load();
        let md_of = |h: Hover| match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        let text = "DEFINE CFSTRUCT(SCI)\n";
        let tree = parse(text);
        let md = md_of(hover(text, &tree, m, Position::new(0, 2), &Default::default()).expect("v"));
        assert!(
            md.contains("Availability (curated)") && md.contains("z/OS only"),
            "{md}"
        );
        let text = "SET CHLAUTH(*) TYPE(BLOCKUSER)\n";
        let tree = parse(text);
        let md = md_of(hover(text, &tree, m, Position::new(0, 2), &Default::default()).expect("v"));
        assert!(
            md.contains("Availability (curated)") && md.contains("from MQ 9.0"),
            "{md}"
        );
        // a plain command keeps the card as it was
        let text = "DEFINE QLOCAL(Q)\n";
        let tree = parse(text);
        let md = md_of(hover(text, &tree, m, Position::new(0, 2), &Default::default()).expect("v"));
        assert!(!md.contains("Availability (curated)"), "{md}");
    }

    #[test]
    fn ccsid_hover_names_the_common_codes() {
        // the curated CCSID text carries the practical code list on every
        // command that has a hoverable CCSID parameter
        let m = Model::load();
        let md_of = |h: Hover| match h.contents {
            HoverContents::Markup(mk) => mk.value,
            _ => panic!("expected markup"),
        };
        let text = "ALTER QMGR CCSID(1208)\n";
        let tree = parse(text);
        let md =
            md_of(hover(text, &tree, m, Position::new(0, 13), &Default::default()).expect("q"));
        assert!(
            md.contains("1208 = UTF-8") && md.contains("819") && md.contains("EBCDIC"),
            "{md}"
        );
        // the reference text is kept, the codes are appended
        assert!(md.contains("Coded character set identifier"), "{md}");
        let text = "DEFINE COMMINFO(C) CCITYPE(MQI) CCSID(819)\n";
        let tree = parse(text);
        // the CCSID word spans chars 32..37 on line 0
        let md =
            md_of(hover(text, &tree, m, Position::new(0, 34), &Default::default()).expect("c"));
        assert!(
            md.contains("MQI bridge") && md.contains("1208 = UTF-8"),
            "{md}"
        );
    }

    #[test]
    fn every_command_rule_targets_a_resolvable_command() {
        // a rule for a pair the model cannot resolve is dead weight and a
        // silent lie about coverage (DISPLAY BUFFPOOL was drafted that way:
        // z/OS-only in the docs, but not in the model at all)
        let m = Model::load();
        let rules = m.all_command_availability();
        assert_eq!(rules.len(), 52);
        for r in rules {
            let (verb, obj) = r.command.split_once(' ').unwrap();
            assert!(
                m.resolve(verb, Some(obj)).is_some(),
                "command_availability rule for unresolvable {}",
                r.command
            );
        }
    }

    #[test]
    fn curated_param_in_model_follows_the_target() {
        // QSGDISP is a base-model parameter on DEFINE QALIAS, so a
        // distributed target must flag 15 without code 4 (the Word branch
        // covers model-known params too), and with no target nothing fires.
        let d = target("9.4", "distributed");
        let c = codes_t("DEFINE QALIAS(A) TARGET(B) QSGDISP(GROUP)\n", &d);
        assert!(c.contains(&CODE_NOT_IN_TARGET), "{c:?}");
        assert!(!c.contains(&CODE_UNKNOWN_PARAM), "{c:?}");
        assert!(!codes("DEFINE QALIAS(A) TARGET(B) QSGDISP(GROUP)\n").contains(&CODE_NOT_IN_TARGET));
    }

    #[test]
    fn availability_lookup_prefers_obj_rule_and_normalizes_synonyms() {
        let m = Model::load();
        // general rule (no obj) found for any command carrying it, and
        // parameter case does not matter; object must be canonical, like
        // name_max_len/mandatory_rules — callers pass Command::object
        assert!(m.availability_for("QSGDISP", "QALIAS").is_some());
        assert!(m.availability_for("qsgdisp", "qalias").is_some());
    }

    #[test]
    fn hover_documents_curated_availability() {
        let m = Model::load();
        let e = m.entry(m.commands["ALTER"]["QLOCAL"]);
        let p = e.params.iter().find(|p| p.name == "QSGDISP").unwrap();
        let doc = doc_for(m, p, "QLOCAL");
        assert!(doc.contains("Availability (curated)"), "{doc}");
        assert!(doc.contains("z/OS"), "{doc}");
        // a param without a rule carries no annotation
        let p = e.params.iter().find(|p| p.name == "DESCR").unwrap();
        assert!(!doc_for(m, p, "QLOCAL").contains("Availability"));
    }
}
