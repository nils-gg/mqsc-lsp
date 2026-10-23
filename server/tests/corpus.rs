//! T-11: run the grammar's corpus cases through the parser the *server*
//! links, so a committed `parser.c` regression fails `cargo test` even on
//! machines without the tree-sitter CLI (or node).
//!
//! The corpus file (`grammars/tree-sitter-mqsc/test/corpus/*.txt`) uses the
//! standard tree-sitter layout: `===` separator, title, `===` separator,
//! source, `---` separator, expected S-expression. Each case must (a) parse
//! with zero ERROR/MISSING nodes and (b) produce exactly the expected tree.
//!
//! S-expression comparison mirrors what the tree-sitter CLI does: node-field
//! labels (`verb: `) and anonymous/extra tokens (`"+"`) are stripped, so the
//! comparison is over named node structure, matching the corpus convention.

use tree_sitter::Node;

fn corpus_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("grammars/tree-sitter-mqsc/test/corpus")
}

fn is_sep(line: &str, c: char) -> bool {
    line.len() >= 40 && line.chars().all(|x| x == c)
}

/// (title, source, expected-sexp) for every case in every corpus file.
fn cases() -> Vec<(String, String, String)> {
    let dir = corpus_dir();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read corpus dir {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().map(|x| x == "txt").unwrap_or(false))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            if !is_sep(lines[i], '=') {
                i += 1;
                continue;
            }
            // title up to the second '=' separator
            let t0 = i + 1;
            let mut j = t0;
            while j < lines.len() && !is_sep(lines[j], '=') {
                j += 1;
            }
            if j >= lines.len() {
                panic!("{}: dangling '=' separator without a closing pair", path.display());
            }
            let title = lines[t0..j].join(" ").trim().to_string();
            // source up to the '-' separator
            let s0 = j + 1;
            let mut k = s0;
            while k < lines.len() && !is_sep(lines[k], '-') && !is_sep(lines[k], '=') {
                k += 1;
            }
            if k >= lines.len() || !is_sep(lines[k], '-') {
                panic!("{}: case {title:?}: missing '-' separator", path.display());
            }
            let source = lines[s0..k].join("\n").trim_end().to_string();
            // expected tree up to the next '=' separator (or EOF)
            let e0 = k + 1;
            let mut e = e0;
            while e < lines.len() && !is_sep(lines[e], '=') {
                e += 1;
            }
            let expected = lines[e0..e].join("\n").trim().to_string();
            out.push((title, source, expected));
            i = e;
        }
    }
    out
}

/// Tokenise an S-expression down to '(' / ')' / node-name tokens, dropping
/// node-field labels (`verb:`) and quoted anonymous tokens (`"+"`), so the
/// comparison is structural over the same vocabulary the corpus uses.
fn norm(sexp: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    for ch in sexp.chars() {
        match ch {
            '(' | ')' => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                toks.push(ch.to_string());
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks.into_iter()
        .filter(|t| !t.starts_with('"')) // anonymous nodes ("+", "\"\"")
        .filter(|t| !(t.ends_with(':') && t[..t.len() - 1].chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')))
        .collect()
}

fn assert_clean(n: Node<'_>, title: &str, src: &str) {
    assert!(
        n.kind() != "ERROR" && !n.is_missing(),
        "corpus case {title:?} regressed: parse produced {} at {}\nsource: {src:?}",
        if n.kind() == "ERROR" { "an ERROR node" } else { "a MISSING node" },
        n.start_position(),
    );
    for i in 0..n.child_count() {
        assert_clean(n.child(i).unwrap(), title, src);
    }
}

#[test]
fn corpus_cases_are_discovered() {
    let n = cases().len();
    assert!(n >= 20, "expected the full corpus (>=20 cases), found {n}");
    // the stray-mid-line-plus case from T-9 must be among them
    assert!(cases().iter().any(|(t, _, _)| t.contains("mid-line plus")));
}

#[test]
fn corpus_parses_match_expected_trees() {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&mqsc_lsp::analysis::language())
        .expect("server links the mqsc grammar");
    let mut count = 0;
    for (title, source, expected) in cases() {
        let src = format!("{source}\n");
        let tree = parser
            .parse(&src, None)
            .unwrap_or_else(|| panic!("case {title:?}: parser returned None"));
        assert_clean(tree.root_node(), &title, &source);
        assert_eq!(
            norm(&tree.root_node().to_sexp()),
            norm(&expected),
            "corpus case {title:?}: tree mismatch"
        );
        count += 1;
    }
    assert!(count >= 20);
}

#[test]
fn language_symbols_link() {
    // build.rs compiles the committed parser.c/scanner.c; this exercises it
    let mut p = tree_sitter::Parser::new();
    p.set_language(&mqsc_lsp::analysis::language()).unwrap();
    let t = p.parse("DEFINE QLOCAL(A)\n", None).unwrap();
    assert_eq!(t.root_node().kind(), "program");
    assert!(!t.root_node().has_error());
}
