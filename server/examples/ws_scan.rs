// Throwaway perf probe for the PR 7 workspace scan: walk a directory tree
// the way the server does at initialized() and report the timing, with a
// breakdown of the per-file passes.
//   cargo run --release --example ws_scan [dir ...]
use mqsc_lsp::analysis;
use mqsc_lsp::model::Model;
use mqsc_lsp::settings::WorkspaceScan;
use mqsc_lsp::workspace::collect_scan;

fn main() {
    let roots: Vec<std::path::PathBuf> = std::env::args()
        .skip(1)
        .map(std::path::PathBuf::from)
        .collect();
    let roots = if roots.is_empty() {
        vec![std::path::PathBuf::from("../dmpmqcfg")]
    } else {
        roots
    };
    let cfg = WorkspaceScan::default();
    let t0 = std::time::Instant::now();
    let out = collect_scan(&roots, &cfg);
    let ms = t0.elapsed().as_millis();
    let sites: usize = out.files.iter().map(|f| f.sites.len()).sum();
    let defs: usize = out.files.iter().map(|f| f.defs.names.len()).sum();
    println!(
        "roots={:?} found={} indexed={} in {}ms (walk+parse) sites={} defined_names={}",
        roots, out.found, out.indexed, ms, sites, defs
    );

    // per-pass breakdown over the same files
    let mut read = 0u128;
    let mut parse = 0u128;
    let mut collect = 0u128;
    let mut index = 0u128;
    let mut bytes_total = 0usize;
    for f in &out.files {
        let p = f.uri.to_file_path().unwrap();
        let t = std::time::Instant::now();
        let bytes = std::fs::read(&p).unwrap();
        read += t.elapsed().as_micros();
        let text = String::from_utf8_lossy(&bytes);
        bytes_total += bytes.len();
        let t = std::time::Instant::now();
        let tree = analysis::parse(&text);
        parse += t.elapsed().as_micros();
        let t = std::time::Instant::now();
        let _defs = analysis::collect_definitions(&tree, text.as_bytes(), Model::load());
        collect += t.elapsed().as_micros();
        let t = std::time::Instant::now();
        let _idx = analysis::object_index(&text, &tree, Model::load());
        index += t.elapsed().as_micros();
    }
    println!(
        "breakdown: read={}ms parse={}ms collect_definitions={}ms object_index={}ms bytes={}MB",
        read / 1000,
        parse / 1000,
        collect / 1000,
        index / 1000,
        bytes_total / 1024 / 1024
    );

    // retained-cost check: the whole point of keeping only defs+sites is
    // memory, so report where the process actually stands (Linux)
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                println!("retained: {line} (peak ≈ same order as index size; baseline without a scan is ~9MB)");
                break;
            }
        }
    }
}
