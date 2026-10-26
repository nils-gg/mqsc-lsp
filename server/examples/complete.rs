use tower_lsp::lsp_types::Position;
fn main() {
    let doc = "DEFINE QLOCAL(DEAD.QUEUE) REPLACE\nDEFINE CHANNEL(SYSTEM.DEF.SDR) CHLTYPE(SDR) TRPTYPE(TCP) CONNAME('9.20.4.20(1412)') SEQWRAP(9999999)\nDISPLAY QLOCAL(*) WHERE(CURRENT_QS)\nALTER QLOCAL(NEW1) MAXMSGS\nDEF QL(X)\nDEFINE QLOCAL(Q1) DESCR('It works')\nDEFINE \nDEFINE     \nALTER \nDISPLAY QUEUE \nDEF \nSET \nDEFINE Q\nDISPLAY QLOCAL \nALTER QLOCAL \nADD \nDEFINE QLOCAL(Q1) REPLACE \nDISPLAY SUB SUB1 \nDEFINE QLOCAL Q\nDEFINE QLOCAL(Q3) +\nDESCR('a')\n";
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
        (6, 7),   // after "DEFINE " -> object types
        (7, 10),  // after "DEFINE " + trailing blanks
        (8, 6),   // after "ALTER " -> object types
        (9, 14),  // after "DISPLAY QUEUE " -> name slot: DEFINEd queues + '*'
        (10, 4),  // after "DEF " -> object types
        (11, 4),  // after "SET " -> object types
        (12, 8),  // after "DEFINE Q" -> Q* object types
        (13, 14), // cursor at end of "DISPLAY QLOCAL" -> object word
        (14, 12), // cursor at end of "ALTER QLOCAL" -> object word
        (15, 4),  // after "ADD " -> object types
        (16, 26), // after "REPLACE " -> params, REPLACE dropped (used)
        (17, 17), // "DISPLAY SUB SUB1 " -> params incl. SUB (object word)
        (18, 15), // after "DEFINE QLOCAL Q" -> DEFINEd names starting Q
        (19, 20), // after "DEFINE QLOCAL(Q3) +" -> params, DESCR still offered
        (20, 0),  // start of a continuation line -> params, NOT verbs
        (20, 5),  // "DESCR" typed on a continuation line -> DESCR itself
        (20, 6),  // inside "DESCR(" on a continuation line -> group values
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
