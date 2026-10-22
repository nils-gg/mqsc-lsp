fn main() {
    let text = "* comment block\n* second line\nDEFINE QLOCAL(Q1) DESCR('multi') +\n  PUT(ENABLED) GET(ENABLED)\nDISPLAY QLOCAL(*)\n\n* lonely\nDEFINE QALIAS(A) TGTQ(B)\n";
    let tree = mqsc_lsp::analysis::parse(text);
    for f in mqsc_lsp::analysis::folding(text, &tree) {
        println!("fold {}..{} {:?}", f.start_line, f.end_line, f.kind);
    }
}
