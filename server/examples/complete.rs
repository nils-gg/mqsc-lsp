use tower_lsp::lsp_types::Position;
fn main() {
    let doc = "DEFINE QLOCAL(DEAD.QUEUE) REPLACE\nDEFINE CHANNEL(SYSTEM.DEF.SDR) CHLTYPE(SDR) TRPTYPE(TCP) CONNAME('9.20.4.20(1412)') SEQWRAP(9999999)\nDISPLAY QLOCAL(*) WHERE(CURRENT_QS)\nALTER QLOCAL(NEW1) MAXMSGS\nDEF QL(X)\nDEFINE QLOCAL(Q1) DESCR('It works')\n";
    for (line, ch) in [
        (0, 0),
        (0, 7),
        (0, 14),
        (0, 27),
        (0, 32),
        (1, 31),
        (2, 8),
        (3, 6),
        (3, 13),
        (3, 19),
        (3, 26),
        (4, 0),
        (4, 4),
        (4, 7),
        (5, 7),
        (5, 13),
        (5, 18),
        (5, 21),
        (5, 32),
        (5, 40),
    ] {
        let tree = mqsc_lsp::analysis::parse(doc);
        let m = mqsc_lsp::model::Model::load();
        let pos = Position {
            line,
            character: ch,
        };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mqsc_lsp::analysis::completions(
                doc,
                &tree,
                m,
                pos,
                true,
                &mqsc_lsp::analysis::Definitions::default(),
            )
        }));
        match res {
            Ok(items) => println!(
                "({line},{ch}) -> {} items: {}",
                items.len(),
                items
                    .iter()
                    .take(8)
                    .map(|i| i.label.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Err(_) => println!("({line},{ch}) -> PANIC"),
        }
    }
}
