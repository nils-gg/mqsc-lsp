use std::path::PathBuf;

fn main() {
    let grammar =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../grammars/tree-sitter-mqsc/src");

    let mut build = cc::Build::new();
    build
        .std("c99")
        .include(&grammar)
        .file(grammar.join("parser.c"))
        .file(grammar.join("scanner.c"));

    println!("cargo:rerun-if-changed=../grammars/tree-sitter-mqsc/src/parser.c");
    println!("cargo:rerun-if-changed=../grammars/tree-sitter-mqsc/src/scanner.c");
    println!("cargo:rerun-if-changed=../grammars/tree-sitter-mqsc/grammar.js");

    build.compile("tree-sitter-mqsc");
}
