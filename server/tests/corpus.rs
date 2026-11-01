//! T-11: run the grammar's corpus cases through the parser the *server*
//! links, so a committed `parser.c` regression fails `cargo test` even on
//! machines without the tree-sitter CLI (or node).
//!
//! The corpus file (`grammars/tree-sitter-mqsc/test/corpus/*.txt`) uses the
//! standard tree-sitter layout: `===` separator, title, `===` separator,
//! source, `---` separator, expected S-expression. Two modes, picked by the
//! expected tree itself: a case whose expected tree contains ERROR or
//! MISSING nodes is a REPAIR case (G1: unclosed groups, stray `)`,
//! newline-bounded strings — the error-recovery contracts the server's
//! code-1 syntax-error messages read) and is only compared structurally;
//! every other case must additionally parse with zero ERROR/MISSING nodes.
//!
//! S-expression comparison mirrors what the tree-sitter CLI does: node-field
//! labels (`verb: `) and anonymous/extra tokens (`"+"`) are stripped, so the
//! comparison is over named node structure, matching the corpus convention.
//! MISSING nodes — anonymous in the tree — are rendered as `(MISSING ")"`)
//! exactly like the corpus writes them.

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
                panic!(
                    "{}: dangling '=' separator without a closing pair",
                    path.display()
                );
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
        .filter(|t| {
            !(t.ends_with(':')
                && t[..t.len() - 1]
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-'))
        })
        .collect()
}

fn assert_clean(n: Node<'_>, title: &str, src: &str) {
    assert!(
        n.kind() != "ERROR" && !n.is_missing(),
        "corpus case {title:?} regressed: parse produced {} at {}\nsource: {src:?}",
        if n.kind() == "ERROR" {
            "an ERROR node"
        } else {
            "a MISSING node"
        },
        n.start_position(),
    );
    for i in 0..n.child_count() {
        assert_clean(n.child(i).unwrap(), title, src);
    }
}

/// Render a tree to an S-expression under the same conventions `norm`
/// compares by: named nodes keep their kind (no field labels), anonymous
/// tokens are skipped — except MISSING ones, which are the whole point of
/// the repair cases and are written `(MISSING ")")` like the corpus does.
/// (The stock `to_sexp` prints neither the missing flag nor a stable form,
/// so repair cases could not pin the inserted token through `norm`.)
fn render(n: Node<'_>, out: &mut String) {
    if n.is_missing() {
        // anonymous in the tree, yet it must speak: it IS the repair
        out.push_str(&format!("(MISSING \"{}\")", n.kind()));
        return;
    }
    if !n.is_named() {
        return; // anonymous `(`, `)`, `+` …: stripped by `norm` anyway
    }
    let mut kids = String::new();
    for i in 0..n.child_count() {
        let c = n.child(i).unwrap();
        let mut s = String::new();
        render(c, &mut s);
        if !s.is_empty() {
            kids.push(' ');
            kids.push_str(&s);
        }
    }
    out.push('(');
    out.push_str(n.kind());
    out.push_str(&kids);
    out.push(')');
}

fn render_sexp(n: Node<'_>) -> String {
    let mut s = String::new();
    render(n, &mut s);
    s
}

/// Cases committed in `test/corpus/*.txt` as of the 2026-10-31 review.
/// Discovery once floor-asserted `>= 20`, so a moved/emptied corpus dir
/// (0 cases found) still "passed" 2/3 of the contract; a bare `>= N` with
/// no relation to reality also let cases silently disappear (REVIEW T5).
/// Bump this WHEN adding cases; the parse test below walks every one.
/// G1 added the robustness.txt repair cases plus the F2 binding case.
const CORPUS_CASES: usize = 36;

#[test]
fn corpus_cases_are_discovered() {
    let n = cases().len();
    assert!(
        n >= CORPUS_CASES,
        "expected the full corpus (>={CORPUS_CASES} cases), found {n}"
    );
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
        // G1 repair mode: an expected tree that carries ERROR/MISSING nodes
        // pins the recovery shape, so the clean-parse rule must not apply.
        let repair = expected.contains("ERROR") || expected.contains("MISSING");
        let src = format!("{source}\n");
        let tree = parser
            .parse(&src, None)
            .unwrap_or_else(|| panic!("case {title:?}: parser returned None"));
        if !repair {
            assert_clean(tree.root_node(), &title, &source);
        }
        assert_eq!(
            norm(&render_sexp(tree.root_node())),
            norm(&expected),
            "corpus case {title:?}: tree mismatch"
        );
        count += 1;
    }
    assert!(
        count >= CORPUS_CASES,
        "only {count} of the corpus's {CORPUS_CASES}+ cases were parsed"
    );
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
