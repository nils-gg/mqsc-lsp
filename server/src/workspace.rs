//! The workspace index: one home for everything the server knows about
//! object names across documents (Phase 3, PR 6 — the pure-centralization
//! step of the patch plan).
//!
//! Before this module, the merge/join loops behind diagnostics (T-17),
//! hover, completion, definition, references, rename and workspace/symbol
//! each lived separately in `server.rs` and re-implemented "iterate the
//! open documents". They are now the same code, and the closed-file index
//! (PR 7: on-disk `*.mqsc` with the open-document override) plus watched
//! reindexing (PR 8) have exactly one place to grow into.
//!
//! The invariant this module protects is the one the correctness milestone
//! locked down: every feature that resolves an MQ object joins on
//! (name, namespace) through `analysis::SymbolKey` / `ObjectScope`, so
//! diagnostics, navigation and completion can never disagree about what a
//! name means. Nothing here adds behavior — every loop is the one that
//! `server.rs` ran before, moved.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use lsp_types::{Location, Range, Url};

use crate::analysis::{self, Definitions, SymbolKey};
use crate::model::Model;

// Poison-tolerant lock access (REVIEW.md M1, moved with the map it
// guards). A panic inside a handler unwinds through whatever guards it
// held and would otherwise poison the shared locks permanently — every
// later request then answers PoisonError forever (a zombie server).
// Nothing guarded here has an invariant a panic can corrupt: analysis
// only reads the tree, and map operations are single atomic
// inserts/removes — so every access recovers via into_inner.
// Keep this in sync with telemetry.rs's `Observed`: that layer turns the
// panic into a -32603 response; these accessors stop it from spreading.
pub(crate) fn rd<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}
pub(crate) fn wr<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}
fn mtx<T>(l: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// A document opened in the editor: its text and the parse tree built for
/// that text. All requests reuse the cached tree instead of re-parsing.
/// `defs` is this document's own DEFINE index, cached so the cross-file
/// workspace view (T-17) is a cheap merge.
///
/// PR 7 adds sibling entries for closed workspace files whose text no
/// longer matches `text` — an open document always overrides its on-disk
/// entry, never the other way round.
struct OpenDoc {
    text: String,
    // tree_sitter::Tree is Send but not Sync, so it needs a lock for shared
    // handler access; handlers only ever use &Tree (analysis never edits).
    tree: Mutex<tree_sitter::Tree>,
    defs: Definitions,
}

/// One name site with its document attached — the cross-document shape of
/// `analysis::NameSite`. Navigation filters to locations; rename needs the
/// quoting and the DEFINE type too.
pub struct SiteAt {
    pub loc: Location,
    /// written in `'quoted'` form (rename must re-quote such sites)
    pub quoted: bool,
    /// canonical model object type for a DEFINE site; `None` for uses
    pub defines: Option<String>,
}

/// A DEFINE with its document attached — workspace/symbol and the
/// cross-file hover map answer from these.
pub struct DefAt {
    pub uri: Url,
    /// uppercased, normalised name — the join key
    pub name: String,
    /// the name text as written (quotes removed)
    pub word: String,
    pub range: Range,
    pub defines: Option<String>,
}

/// Every document the index knows about. Today that is exactly the open
/// documents (the didOpen/didChange/didClose lifecycle keeps the map);
/// closed workspace files join in PR 7 behind the open-document override.
#[derive(Default)]
pub struct WorkspaceIndex {
    docs: RwLock<HashMap<Url, OpenDoc>>,
}

impl WorkspaceIndex {
    /// Register (or replace, on didChange) an open document. The caller
    /// parsed the text and collected its DEFINE index — the same data the
    /// old `server.rs` insert did.
    pub fn open(&self, uri: Url, text: String, tree: tree_sitter::Tree, defs: Definitions) {
        wr(&self.docs).insert(
            uri,
            OpenDoc {
                text,
                tree: Mutex::new(tree),
                defs,
            },
        );
    }

    /// Drop a document (didClose); returns whether it was open.
    pub fn close(&self, uri: &Url) -> bool {
        wr(&self.docs).remove(uri).is_some()
    }

    /// Total documents in the index — the `open_docs=` field of the
    /// analyze log line.
    pub fn len(&self) -> usize {
        rd(&self.docs).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every registered uri (republish-all on settings change).
    pub fn uris(&self) -> Vec<Url> {
        rd(&self.docs).keys().cloned().collect()
    }

    /// Every registered uri except `skip` (the did_open/did_close
    /// refresh-others rule: the changed document was just refreshed).
    pub fn uris_excluding(&self, skip: &Url) -> Vec<Url> {
        rd(&self.docs)
            .keys()
            .filter(|u| *u != skip)
            .cloned()
            .collect()
    }

    /// Run `f` against one open document's cached text and tree.
    pub fn with_doc<T>(
        &self,
        uri: &Url,
        f: impl FnOnce(&str, &tree_sitter::Tree) -> T,
    ) -> Option<T> {
        let docs = rd(&self.docs);
        docs.get(uri).map(|doc| {
            let tree = mtx(&doc.tree);
            f(&doc.text, &tree)
        })
    }

    /// Definitions from all documents except `uri` (cached per doc, so
    /// this merge is cheap). The result is exactly what the code-12 hint
    /// and completion name pool see, which is why the hint and completion
    /// can never disagree about what exists.
    pub fn external_defs(&self, uri: &Url) -> Definitions {
        let mut merged = Definitions::default();
        for (u, doc) in rd(&self.docs).iter() {
            if u != uri {
                merged.add_external(&doc.defs);
            }
        }
        merged
    }

    /// Every DEFINE site in the index, optionally excluding one document
    /// (the cross-file hover map excludes the reader; workspace/symbol
    /// excludes nothing). Rebuilds each document's cheap name-site scan —
    /// definitions are read from the tree, never cached a second time.
    pub fn definitions(&self, excluding: Option<&Url>) -> Vec<DefAt> {
        let mut out = Vec::new();
        let docs = rd(&self.docs);
        for (u, doc) in docs.iter() {
            if excluding == Some(u) {
                continue;
            }
            let tree = mtx(&doc.tree);
            let idx = analysis::object_index(&doc.text, &tree, Model::load());
            out.extend(idx.definitions().map(|s| DefAt {
                uri: u.clone(),
                name: s.name.clone(),
                word: s.word.clone(),
                range: s.range,
                defines: s.defines.clone(),
            }));
        }
        out
    }

    /// Every site of one symbol (name AND namespace — the same rule
    /// `analysis::ObjectScope` gives diagnostics and completion) across
    /// all documents (T-17). `definitions_only: Some(true)` keeps only
    /// DEFINE sites, `Some(false)` only uses.
    pub fn sites(&self, key: &SymbolKey, definitions_only: Option<bool>) -> Vec<Location> {
        self.sites_at(key, definitions_only)
            .into_iter()
            .map(|s| s.loc)
            .collect()
    }

    /// All sites of the symbol with quoting and DEFINE type — what rename
    /// needs to re-quote each site and to run its same-name-different-type
    /// safety gate.
    pub fn rename_sites(&self, key: &SymbolKey) -> Vec<SiteAt> {
        self.sites_at(key, None)
    }

    fn sites_at(&self, key: &SymbolKey, definitions_only: Option<bool>) -> Vec<SiteAt> {
        let mut out = Vec::new();
        let ns = key.scope.namespace();
        let docs = rd(&self.docs);
        for (u, doc) in docs.iter() {
            let tree = mtx(&doc.tree);
            let idx = analysis::object_index(&doc.text, &tree, Model::load());
            for s in idx.sites_in(&key.name, ns) {
                let keep = match definitions_only {
                    Some(true) => s.defines.is_some(),
                    Some(false) => s.defines.is_none(),
                    None => true,
                };
                if keep {
                    out.push(SiteAt {
                        loc: Location {
                            uri: u.clone(),
                            range: s.range,
                        },
                        quoted: s.quoted,
                        defines: s.defines.clone(),
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;

    fn open_text(index: &WorkspaceIndex, path: &str, text: &str) -> Url {
        let uri = Url::from_file_path(path).unwrap();
        let tree = analysis::parse(text);
        let defs = analysis::collect_definitions(&tree, text.as_bytes(), Model::load());
        index.open(uri.clone(), text.to_string(), tree, defs);
        uri
    }

    /// Two scripts: `a` defines a queue, `b` defines an alias that
    /// references it and its own channel of the same name as the queue —
    /// the namespace split the whole index joins on.
    fn fixture() -> (WorkspaceIndex, Url, Url) {
        let index = WorkspaceIndex::default();
        let a = open_text(&index, "/tmp/a.mqsc", "DEFINE QLOCAL(AA)\n");
        let b = open_text(
            &index,
            "/tmp/b.mqsc",
            "DEFINE QALIAS(AL) TARGET(AA)\nDEFINE CHANNEL(AA) CHLTYPE(SDR) TRPTYPE(TCP)\n",
        );
        (index, a, b)
    }

    fn key_at(index: &WorkspaceIndex, uri: &Url, line: u32, ch: u32) -> SymbolKey {
        index
            .with_doc(uri, |text, tree| {
                analysis::object_index(text, tree, Model::load())
                    .symbol_at(Position {
                        line,
                        character: ch,
                    })
                    .expect("no site at position")
            })
            .unwrap()
    }

    #[test]
    fn lifecycle_len_and_uris() {
        let (index, a, b) = fixture();
        assert_eq!(index.len(), 2);
        assert!(!index.is_empty());
        assert_eq!(index.uris_excluding(&a), vec![b.clone()]);
        assert!(index.close(&b));
        assert!(!index.close(&b));
        assert_eq!(index.len(), 1);
        assert_eq!(index.uris(), vec![a]);
    }

    #[test]
    fn external_defs_merge_excludes_self_and_marks_the_widening() {
        let (index, a, b) = fixture();
        let from_b = index.external_defs(&a);
        assert!(from_b.names.contains("AL"), "alias of b visible to a");
        assert!(from_b.names.contains("AA"), "channel of b visible to a");
        assert!(from_b.from_other_files, "code-12 wording widens");
        let from_a = index.external_defs(&b);
        assert!(from_a.names.contains("AA") && !from_a.names.contains("AL"));
    }

    #[test]
    fn sites_join_on_name_and_namespace() {
        let (index, a, b) = fixture();
        // the TARGET(AA) reference in b — queue namespace
        let q = key_at(&index, &b, 0, 25);
        assert_eq!(q.name, "AA");
        // the positional CHANNEL name — channel namespace, same spelling
        let c = key_at(&index, &b, 1, 16);
        assert_eq!(c.name, "AA");
        assert_ne!(q.scope.namespace(), c.scope.namespace());

        let defs = index.sites(&q, Some(true));
        assert_eq!(defs.len(), 1, "queue namespace finds only the QLOCAL");
        assert_eq!(defs[0].uri, a);
        assert_eq!(index.sites(&q, Some(false)).len(), 1, "the TARGET use");
        assert_eq!(index.sites(&q, None).len(), 2);

        let cdefs = index.sites(&c, Some(true));
        assert_eq!(cdefs.len(), 1);
        assert_eq!(cdefs[0].uri, b);
        assert_eq!(cdefs[0].range.start.line, 1);
    }

    #[test]
    fn rename_sites_carry_quoting_and_define_types() {
        let (index, a, b) = fixture();
        let q = key_at(&index, &b, 0, 25);
        let sites = index.rename_sites(&q);
        assert_eq!(sites.len(), 2);
        let def: Vec<_> = sites.iter().filter_map(|s| s.defines.clone()).collect();
        assert_eq!(def, vec!["QLOCAL".to_string()], "exactly one DEFINE site");
        assert!(sites.iter().all(|s| !s.quoted));
        // the DEFINE site is in a, the use in b
        let mut uris: Vec<_> = sites.iter().map(|s| s.loc.uri.clone()).collect();
        uris.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(uris, want);
    }

    #[test]
    fn definitions_excluding_powers_hover_and_symbols() {
        let (index, a, b) = fixture();
        let all = index.definitions(None);
        assert_eq!(all.len(), 3, "QLOCAL + QALIAS + CHANNEL");
        let for_hover = index.definitions(Some(&b));
        assert_eq!(
            for_hover
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            vec!["AA"],
            "b's hover sees only a's DEFINE"
        );
        assert_eq!(for_hover[0].uri, a);
        assert_eq!(for_hover[0].defines.as_deref(), Some("QLOCAL"));
        // hover never sees the reader's own defines, symbol always does
        assert!(index.definitions(Some(&a)).iter().all(|d| d.uri == b));
    }

    #[test]
    fn reopening_replaces_the_entry() {
        let (index, _a, _b) = fixture();
        open_text(&index, "/tmp/a.mqsc", "DEFINE QLOCAL(AA2)\n");
        assert_eq!(index.len(), 2, "replace, not duplicate");
        let for_a = index.external_defs(&Url::from_file_path("/tmp/b.mqsc").unwrap());
        assert!(for_a.names.contains("AA2"), "didChange is visible at once");
        assert!(!for_a.names.contains("AA"), "the old define is gone");
        assert!(for_a.from_other_files, "a contributes defines");
    }
}
