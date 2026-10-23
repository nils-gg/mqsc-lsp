use std::time::Instant;
use tower_lsp::lsp_types::Url;
fn main() {
    let text = std::io::read_to_string(std::io::stdin()).unwrap();
    eprintln!("probing {} bytes", text.len());
    let now = Instant::now();
    let tree = mqsc_lsp::analysis::parse(&text);
    eprintln!("parsed in {:?}", now.elapsed());
    let m = mqsc_lsp::model::Model::load();
    let uri: Url = "file:///t.mqsc".parse().unwrap();
    let diags = mqsc_lsp::analysis::analyze(&text, &tree, m, &uri, &mqsc_lsp::analysis::Definitions::default());
    eprintln!("analyzed in {:?}: {} diagnostics", now.elapsed(), diags.len());
    for d in &diags {
        let code = match &d.code {
            Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => n.to_string(),
            Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(),
            None => String::new(),
        };
        eprintln!("  L{} sev{:?} [{}] {}", d.range.start.line, d.severity, code, d.message);
    }
}
