use std::io::Read;
use std::time::Instant;
use tower_lsp::lsp_types::Url;
fn main() {
    // Lossy decode: real dmpmqcfg dumps contain the odd non-UTF-8 byte
    // (DESCR text written on Windows boxes); a probe must never panic.
    // An unreadable stdin is an IO error, not a crash: exit 2 like the
    // --check CLI's IO contract, never the 101 a panic would print
    // (REVIEW T2).
    let mut buf = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
        eprintln!("probe: cannot read stdin: {e}");
        std::process::exit(2);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    eprintln!("probing {} bytes", text.len());
    let now = Instant::now();
    let tree = mqsc_lsp::analysis::parse(&text);
    eprintln!("parsed in {:?}", now.elapsed());
    let m = mqsc_lsp::model::Model::load();
    let uri: Url = "file:///t.mqsc".parse().unwrap();
    let diags = mqsc_lsp::analysis::analyze(
        &text,
        &tree,
        m,
        &uri,
        &mqsc_lsp::analysis::Definitions::default(),
    );
    eprintln!(
        "analyzed in {:?}: {} diagnostics",
        now.elapsed(),
        diags.len()
    );
    for d in &diags {
        let code = match &d.code {
            Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => n.to_string(),
            Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(),
            None => String::new(),
        };
        eprintln!(
            "  L{} sev{:?} [{}] {}",
            d.range.start.line, d.severity, code, d.message
        );
    }
}
