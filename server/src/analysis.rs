//! Tree-sitter parsing and all MQSC language analysis: diagnostics, hover,
//! completion, document symbols, folding, highlights, semantic tokens.

use std::collections::HashSet;

use lsp_types::{
    self as lsp, CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    DocumentHighlight, DocumentHighlightKind, DocumentSymbol, FoldingRange, FoldingRangeKind,
    Hover, HoverContents, InsertTextFormat, MarkupContent, MarkupKind, Position, Range,
    SemanticToken, SymbolKind, Url,
};
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
pub struct Lines {
    starts: Vec<usize>,
    text: String,
}

impl Lines {
    pub fn new(text: &str) -> Lines {
        let mut starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Lines { starts, text: text.to_string() }
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
        let col = utf16_len(&self.text[start..offset]);
        Position { line: line as u32, character: col as u32 }
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
    s.chars().map(|c| c.len_utf16() as usize).sum()
}

fn node_text<'a>(n: Node<'a>, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
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

struct Diags {
    items: Vec<Diagnostic>,
}

impl Diags {
    fn push(&mut self, r: Range, severity: DiagnosticSeverity, code: i32, msg: String) {
        self.items.push(Diagnostic {
            range: r,
            severity: Some(severity),
            code: Some(lsp::NumberOrString::Number(code)),
            source: Some("mqsc".into()),
            message: msg,
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

pub fn analyze(text: &str, tree: &Tree, m: &Model, _uri: &Url) -> Vec<Diagnostic> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut d = Diags { items: Vec::new() };

    // ---- lexical checks over raw text
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find('\t') {
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
                let looks_param =
                    m.canon_verb(&up).is_none() && (m.known_params.contains(&up) || m.known_attrs.contains(&up));
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
        for i in 0..n.named_child_count() {
            if let Some(c) = n.named_child(i) {
                stack.push(c);
            }
        }
    }

    // ---- semantic checks per command
    let mut cursor = root.walk();
    for cmd in root.named_children(&mut cursor) {
        if cmd.kind() == "command" {
            check_command(cmd, src, &lines, m, &mut d);
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

fn command_before<'t>(tree: &'t Tree, byte_off: usize) -> Option<Node<'t>> {
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

fn check_command(cmd: Node<'_>, src: &[u8], lines: &Lines, m: &Model, d: &mut Diags) {
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
                d.push(
                    lines.range(verb_node.start_byte(), w.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_OBJECT,
                    msg,
                );
            } else if m.known_params.contains(&up_w) || m.known_attrs.contains(&up_w) {
                d.push(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_MISSING_CONT,
                    format!(
                        "'{up_w}' is not a command; it is a {}. If this line continues the \
                         command above, end the previous line with '+' or '-'",
                        if m.known_params.contains(&up_w) { "parameter" } else { "attribute" }
                    ),
                );
            } else {
                let sugs = model::suggest(verb_txt, m.all_verbs().into_iter(), 3);
                let msg = if sugs.is_empty() {
                    format!("Unknown command '{verb_txt}'")
                } else {
                    format!("Unknown command '{verb_txt}'; did you mean {}?", sugs.join(", "))
                };
                d.push(
                    lines.range(verb_node.start_byte(), verb_node.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_VERB,
                    msg,
                );
            }
            return;
        }
        (Some(_), Some(i)) => m.entry(i),
    };

    let multi_pos = matches!(
        e.verb,
        "DISPLAY" | "START" | "STOP" | "CLEAR" | "RESET" | "MOVE" | "PING" | "REFRESH" | "RESUME" | "SUSPEND"
    );
    let positional_required = matches!(e.verb, "DEFINE" | "ALTER" | "DELETE");

    let mut seen_name = false;
    // When a word has already been flagged as not-a-parameter, the argument
    // group directly after it belongs to the same typo: report once, not twice.
    let mut mute_next_group = false;
    let mut used: HashSet<String> = HashSet::new();
    let mut used_seq: Vec<(String, usize)> = Vec::new(); // name -> first word start
    let mut pending: Option<(String, Node<'_>)> = None;

    for (k, item) in items[idx..].iter().enumerate() {
        match item {
            Item::Word(n) => {
                mute_next_group = false;
                let w = node_text(*n, src);
                let up = w.to_ascii_uppercase();
                if used.contains(&up) {
                    d.push(
                        lines.range(n.start_byte(), n.end_byte()),
                        DiagnosticSeverity::ERROR,
                        CODE_DUPLICATE,
                        format!("Parameter {up} appears more than once; MQSC allows it once"),
                    );
                } else {
                    used.insert(up.clone());
                    used_seq.push((up.clone(), n.start_byte()));
                }

                if let Some(&pi) = e.by_name.get(&up) {
                    let p = &e.params[pi];
                    let wants_group = !matches!(p.kind, ValueKind::Flag | ValueKind::Star | ValueKind::Free)
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
                    format!("'{w}' is not a parameter, attribute, or object name of {} {}", e.verb, e.object)
                } else {
                    format!("'{w}' is not a parameter of {} {}; did you mean {}?", e.verb, e.object, sugs.join(", "))
                };
                d.push(
                    lines.range(n.start_byte(), n.end_byte()),
                    DiagnosticSeverity::ERROR,
                    CODE_UNKNOWN_PARAM,
                    msg,
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
                    let vs = if n.kind() == "value" { vec![*n] } else { value_nodes(*n) };
                    check_values(p, &up, &vs, src, lines, d);
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
    let missing_one = if missing.len() == 1 { missing } else { Vec::new() };
    for p in missing_one {
        {
            d.push(
                lines.range(cmd.start_byte(), verb_node.end_byte()),
                DiagnosticSeverity::WARNING,
                CODE_MISSING_REQUIRED,
                format!("{} {} usually requires parameter {}", e.verb, e.object, p.name),
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

fn check_values(p: &Param, up: &str, values: &[Node<'_>], src: &[u8], lines: &Lines, d: &mut Diags) {
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
                let ok = has_int && !has_string && !has_word.map(|w| !w.chars().all(|c| c.is_ascii_digit())).unwrap_or(false);
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
                    if !vals.iter().any(|x| x == &uw) {
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
                        d.push(
                            lines.range(v.start_byte(), v.end_byte()),
                            DiagnosticSeverity::WARNING,
                            CODE_BAD_VALUE,
                            msg,
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

fn command_context<'t>(cmd: Node<'t>, src: &'t [u8], m: &'t Model) -> Option<(&'t str, Option<&'t str>, &'t model::Command)> {
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

pub fn hover(text: &str, tree: &Tree, m: &Model, pos: Position) -> Option<Hover> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let off = lines.offset(pos);
    let node = node_at(tree.root_node(), off);
    if node.kind() == "comment" {
        return None;
    }
    let cmd = ancestor_of(node, "command")?;
    let (verb, obj, e) = command_context(cmd, src, m)?;

    let mk = |value: String| Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(lines.range(node.start_byte(), node.end_byte())),
    };

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
                e.attrs.iter().take(14).cloned().collect::<Vec<_>>().join(" "),
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
        md.push_str(if p.optional { "_Optional._\n\n" } else { "_Required by the reference diagram._\n\n" });
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
        let mut md = format!("**Displayable attribute — `{pname}`**\n\n_{} {}_\n\n", verb, e.object);
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

fn preceding_word<'t>(cmd: Node<'t>, before: Node<'_>, src: &'t [u8]) -> Option<(String, Node<'t>)> {
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

pub fn completions(text: &str, tree: &Tree, m: &Model, pos: Position) -> Vec<CompletionItem> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let off = lines.offset(pos);
    let line = lines.line_start(pos.line as usize);
    let end = lines.starts.get(pos.line as usize + 1).copied().unwrap_or(text.len());
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
                        return value_completions(&e.params[pi], &frag);
                    }
                }
            }
            // The argument is either the object-name group (directly after the
            // object word) or belongs to an unknown word: MQSC object names and
            // most keyword values are free tokens, so offer the generic pattern.
            return vec![CompletionItem {
                label: "*".into(),
                kind: Some(CompletionItemKind::TEXT),
                detail: Some("generic name pattern".into()),
                insert_text: Some("*".into()),
                ..Default::default()
            }];
        }
    }

    let cmd_opt = command_containing(tree, off);
    let before_trimmed = before.trim_start();

    // fresh line: suggest verbs
    if cmd_opt.is_none() || before_trimmed.is_empty() {
        let mut out: Vec<CompletionItem> = m
            .all_verbs()
            .iter()
            .map(|v| CompletionItem {
                label: v.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some("MQSC command".into()),
                filter_text: Some(v.to_string()),
                sort_text: Some(format!("{:03}", v.chars().count())),
                ..Default::default()
            })
            .collect();
        filter_prefix(&mut out, &frag);
        return out;
    }

    let cmd = cmd_opt.unwrap();
    let items = command_items(cmd);
    let verb = match items.first() {
        Some(Item::Verb(n)) => node_text(*n, src),
        _ => return vec![],
    };

    // still typing the verb (only one child so far)
    if items.len() == 1 {
        let mut out: Vec<CompletionItem> = m
            .all_verbs()
            .iter()
            .map(|v| CompletionItem {
                label: v.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            })
            .collect();
        filter_prefix(&mut out, &frag);
        return out;
    }

    // object position: verb + one word, no groups yet
    if items.len() <= 2 && matches!(items.get(1), Some(Item::Word(_))) && m.resolve(verb, word_at(&items, 1, src)).is_none() {
        // word under the cursor is the object type being typed
        let mut out: Vec<CompletionItem> = m
            .objects_for(verb)
            .into_iter()
            .map(|o| CompletionItem {
                label: o.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(format!("{} {o}", verb.to_ascii_uppercase())),
                ..Default::default()
            })
            .collect();
        if out.is_empty() {
            return out;
        }
        filter_prefix(&mut out, &frag);
        return out;
    }

    let (verb, obj, e) = match command_context(cmd, src, m) {
        Some(x) => x,
        None => return vec![],
    };

    // used parameters: words fully before the cursor
    let mut used: HashSet<String> = HashSet::new();
    for it in &items[1..] {
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
        let (insert, kind) = match &p.kind {
            ValueKind::Flag => (p.name.clone(), CompletionItemKind::KEYWORD),
            _ => (format!("{}(", p.name), CompletionItemKind::PROPERTY),
        };
        let detail = match &p.kind {
            ValueKind::Flag => "keyword".to_string(),
            ValueKind::Enum(vals) => {
                format!("values: {}", vals.iter().take(4).cloned().collect::<Vec<_>>().join("/"))
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
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
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
    let _ = obj;
    out
}

fn word_at<'t>(items: &[Item<'t>], idx: usize, src: &'t [u8]) -> Option<&'t str> {
    match items.get(idx) {
        Some(Item::Word(n)) => Some(node_text(*n, src)),
        _ => None,
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
    if before.chars().next_back().map_or(true, |c| !(c.is_alphanumeric() || matches!(c, '_' | '#' | '@' | '%' | '/' | '.' | '-'))) {
        return String::new();
    }
    let t = before;
    let end = t
        .char_indices()
        .rfind(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '#' | '@' | '%' | '/' | '.' | '-')))
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
        let mut label = String::new();
        let mut detail = String::new();
        for it in &items {
            let n = match it {
                Item::Verb(n) | Item::Word(n) => *n,
                Item::Arg(n) | Item::Wild(n) | Item::Val(n) => *n,
            };
            let t = node_text(n, src);
            if label.is_empty() {
                label = t.to_ascii_uppercase();
            } else {
                detail.push_str(t);
                detail.push(' ');
            }
        }
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
    "keyword",  // 0 command verb
    "type",     // 1 object type
    "property", // 2 parameter name
    "variable", // 3 object name / positional value
    "enumMember", // 4 enumerated keyword value
    "string",   // 5 quoted string
    "number",   // 6 integer
    "comment",  // 7 comment
    "regexp",   // 8 wildcard
];

pub fn semantic_tokens(text: &str, tree: &Tree) -> Vec<SemanticToken> {
    let src = text.as_bytes();
    let lines = Lines::new(text);
    let mut raw: Vec<(usize, usize, usize, u32, u32)> = Vec::new(); // line, col, len, ty, mods

    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        match node.kind() {
            "comment" => {
                emit(node, src, &lines, 7, &mut raw);
                continue;
            }
            "command" => {}
            _ => continue,
        }
        let items = command_items(node);
        // parameter words: any Word followed (in items order) by an Arg, plus
        // bare flag/attr words resolved against the model
        let cmd_ctx = command_context(node, src, &Model::load());
        let mut obj_done = false;
        for (i, it) in items.iter().enumerate() {
            match it {
                Item::Verb(n) => emit(*n, src, &lines, 0, &mut raw),
                Item::Word(n) => {
                    let up = node_text(*n, src).to_ascii_uppercase();
                    if !obj_done && i == 1 {
                        obj_done = true;
                        emit(*n, src, &lines, 1, &mut raw); // object type
                        continue;
                    }
                    let is_param = match cmd_ctx {
                        Some((_, _, e)) => {
                            e.by_name.contains_key(&up)
                                || e.attr_set.contains(&up)
                                || matches!(items.get(i + 1), Some(Item::Arg(_)))
                        }
                        None => matches!(items.get(i + 1), Some(Item::Arg(_))),
                    };
                    emit(*n, src, &lines, if is_param { 2 } else { 3 }, &mut raw);
                }
                Item::Arg(n) => {
                    let mut st = vec![*n];
                    while let Some(x) = st.pop() {
                        match x.kind() {
                            "string" => emit(x, src, &lines, 5, &mut raw),
                            "integer" => emit(x, src, &lines, 6, &mut raw),
                            "wildcard" => emit(x, src, &lines, 8, &mut raw),
                            "word" => emit(x, src, &lines, 4, &mut raw),
                            _ => {}
                        }
                        for k in 0..x.named_child_count() {
                            if let Some(c) = x.named_child(k) {
                                st.push(c);
                            }
                        }
                    }
                }
                Item::Wild(n) => emit(*n, src, &lines, 8, &mut raw),
                Item::Val(n) => {
                    let mut st = vec![*n];
                    while let Some(x) = st.pop() {
                        match x.kind() {
                            "string" => emit(x, src, &lines, 5, &mut raw),
                            "integer" => emit(x, src, &lines, 6, &mut raw),
                            "wildcard" => emit(x, src, &lines, 8, &mut raw),
                            "word" => emit(x, src, &lines, 4, &mut raw),
                            _ => {}
                        }
                        for k in 0..x.named_child_count() {
                            if let Some(c) = x.named_child(k) {
                                st.push(c);
                            }
                        }
                    }
                }
                Item::Verb(_) => {}
            }
        }
    }

    raw.sort_by_key(|(l, c, ..)| (*l, *c));
    raw.dedup_by_key(|(l, c, ..)| (*l, *c));

    // delta-encode
    let mut prev_line = 0usize;
    let mut prev_char = 0usize;
    let mut out = Vec::with_capacity(raw.len());
    for (line, col, len, ty, mods) in raw {
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { col - prev_char } else { col };
        out.push(SemanticToken {
            delta_line: delta_line as u32,
            delta_start: delta_start as u32,
            length: len as u32,
            token_type: ty,
            token_modifiers_bitset: 0,
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
    out: &mut Vec<(usize, usize, usize, u32, u32)>,
) {
    let p = lines.pos(n.start_byte());
    let len = utf16_len(node_text(n, src).trim_end_matches(['\n', '\r']));
    if len > 0 {
        out.push((p.line as usize, p.character as usize, len, ty, 0));
    }
}
