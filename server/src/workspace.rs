//! The workspace index: one home for everything the server knows about
//! object names across documents (Phase 3 — PR 6 centralized it, PR 7
//! taught it the filesystem).
//!
//! Two entry kinds live side by side:
//! - **Open** documents (didOpen/didChange/didClose lifecycle): full text
//!   plus the cached parse tree; the editor guarantees the text matches.
//! - **Closed** workspace files found by the startup scan of the
//!   workspace roots (`collect_scan`): only the lightweight result —
//!   `Definitions` + name sites — never text, never a tree (patch plan
//!   Phase 3: "do not retain every full parse tree indefinitely").
//!
//! The override rule is absolute: an open document always wins over its
//! on-disk twin, never the other way round; closing a document falls back
//! to the DISK text (an unsaved buffer must not haunt the index).
//!
//! Consumers join exclusively on `(name, namespace)` `SymbolKey`s — the
//! invariant the correctness milestone locked down, so diagnostics,
//! navigation, hover, completion and workspace/symbol cannot disagree
//! about what a name means, whether the answer came from a buffer or a
//! closed file. The one deliberate asymmetry: **rename still edits only
//! open documents** (`rename_sites`). A closed entry's content is only as
//! fresh as the last scan — until PR 8's `didChangeWatchedFiles`
//! reindexing keeps it current, the index is trusted for read features
//! but not for mass edits.
//!
//! PR 8 (watched reindexing) is the remaining extension point: create/
//! change/delete events re-run `disk_entry` for one path, and rename
//! then graduates to all entries.

use std::collections::hash_map::Entry as MapEntry;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use lsp_types::{Location, Range, Url};

use crate::analysis::{self, Definitions, NameSite, SymbolKey};
use crate::model::Model;
use crate::settings::{WorkspaceScan, SCAN_MAX_FILE_BYTES};

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

/// Where an entry's knowledge came from.
pub(crate) enum Source {
    /// An open editor document: the editor's text is authoritative and
    /// the tree is cached for full-service requests (hover, diagnostics).
    Open {
        text: String,
        // tree_sitter::Tree is Send but not Sync, so it needs a lock for
        // shared handler access; handlers only ever use &Tree (analysis
        // never edits).
        tree: Mutex<tree_sitter::Tree>,
    },
    /// A closed workspace file, indexed from disk at scan time. Only
    /// definitions and sites survive — text and tree are dropped.
    Closed,
}

/// One document in the index. `defs` and `sites` are computed once per
/// content and shared by every consumer, which is what keeps the hint-12
/// merge, the site joins and the symbol list from ever disagreeing.
pub(crate) struct Entry {
    source: Source,
    defs: Definitions,
    sites: Vec<NameSite>,
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

/// One closed file as `collect_scan` produced it, ready to join the index.
#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub uri: Url,
    pub defs: Definitions,
    pub sites: Vec<NameSite>,
}

/// Scan tallies for logging: `found` .mqsc files under the roots,
/// `indexed` of them after the `max_files` cap (sorted paths, so the
/// subset is deterministic).
pub struct ScanOutcome {
    pub found: usize,
    pub indexed: usize,
    pub files: Vec<ScannedFile>,
}

/// Everything the index knows: every open document plus the closed
/// workspace files the startup scan found.
#[derive(Default)]
pub struct WorkspaceIndex {
    docs: RwLock<HashMap<Url, Entry>>,
}

impl WorkspaceIndex {
    /// Register (or replace, on didChange) an open document. The caller
    /// parsed the text and collected its DEFINE index; the name-site scan
    /// runs here so every consumer shares one computation per content.
    /// An open document REPLACES any closed entry for the same uri — the
    /// override rule.
    pub fn open(&self, uri: Url, text: String, tree: tree_sitter::Tree, defs: Definitions) {
        let sites = analysis::object_index(&text, &tree, Model::load()).sites;
        wr(&self.docs).insert(
            uri,
            Entry {
                source: Source::Open {
                    text,
                    tree: Mutex::new(tree),
                },
                defs,
                sites,
            },
        );
    }

    /// Drop the editor's entry (didClose). The file usually still exists
    /// on disk, so it is re-read and stays in the index as a CLOSED entry
    /// — from the DISK text, because an unsaved buffer's content must not
    /// keep silencing hint 12 after the editor let go of the file.
    /// Returns whether an open entry was removed.
    pub fn close(&self, uri: &Url) -> bool {
        let was_open = wr(&self.docs).remove(uri).is_some();
        if let Ok(path) = uri.to_file_path() {
            if let Some(entry) = disk_entry(&path) {
                // insert only if gone: a racing didOpen must not be lost
                let mut docs = wr(&self.docs);
                if let MapEntry::Vacant(slot) = docs.entry(uri.clone()) {
                    slot.insert(entry);
                }
            }
        }
        was_open
    }

    /// Merge startup-scan results; open documents (and anything already
    /// indexed) win, so this never overrides. Returns how many entries
    /// were actually added.
    pub fn merge_scanned(&self, files: Vec<ScannedFile>) -> usize {
        let mut docs = wr(&self.docs);
        let mut added = 0;
        for f in files {
            // entry API: an existing entry (an open document, or a file a
            // racing didOpen just registered) is never replaced
            if let MapEntry::Vacant(slot) = docs.entry(f.uri) {
                slot.insert(Entry {
                    source: Source::Closed,
                    defs: f.defs,
                    sites: f.sites,
                });
                added += 1;
            }
        }
        added
    }

    /// Open documents only — the ones diagnostics are published for.
    pub fn len(&self) -> usize {
        rd(&self.docs)
            .values()
            .filter(|e| matches!(e.source, Source::Open { .. }))
            .count()
    }

    pub fn is_empty(&self) -> bool {
        rd(&self.docs).is_empty()
    }

    /// Every entry, open and closed — the `ws_files=` log field.
    pub fn total(&self) -> usize {
        rd(&self.docs).len()
    }

    /// Every OPEN document uri (republish-all on settings change).
    pub fn open_uris(&self) -> Vec<Url> {
        rd(&self.docs)
            .iter()
            .filter(|(_, e)| matches!(e.source, Source::Open { .. }))
            .map(|(u, _)| u.clone())
            .collect()
    }

    /// Every OPEN uri except `skip` (the did_open/did_close
    /// refresh-others rule: the changed document was just refreshed).
    pub fn open_uris_excluding(&self, skip: &Url) -> Vec<Url> {
        self.open_uris().into_iter().filter(|u| u != skip).collect()
    }

    /// Run `f` against one OPEN document's cached text and tree. Closed
    /// entries have neither by design — full-service requests need the
    /// editor's buffer.
    pub fn with_doc<T>(
        &self,
        uri: &Url,
        f: impl FnOnce(&str, &tree_sitter::Tree) -> T,
    ) -> Option<T> {
        let docs = rd(&self.docs);
        docs.get(uri).and_then(|doc| match &doc.source {
            Source::Open { text, tree } => {
                let tree = mtx(tree);
                Some(f(text, &tree))
            }
            Source::Closed => None,
        })
    }

    /// Definitions from every OTHER entry — open siblings and closed
    /// workspace files alike (T-17 + PR 7). The result is exactly what
    /// the code-12 hint and the completion name pool see, which is why
    /// hint and completion can never disagree about what exists.
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
    /// excludes nothing). Reads the stored sites — closed files cost the
    /// same as open ones here.
    pub fn definitions(&self, excluding: Option<&Url>) -> Vec<DefAt> {
        let mut out = Vec::new();
        let docs = rd(&self.docs);
        for (u, doc) in docs.iter() {
            if excluding == Some(u) {
                continue;
            }
            out.extend(
                doc.sites
                    .iter()
                    .filter(|s| s.defines.is_some())
                    .map(|s| DefAt {
                        uri: u.clone(),
                        name: s.name.clone(),
                        word: s.word.clone(),
                        range: s.range,
                        defines: s.defines.clone(),
                    }),
            );
        }
        out
    }

    /// Every site of one symbol (name AND namespace — the same rule
    /// `analysis::ObjectScope` gives diagnostics and completion) across
    /// every entry, open or closed (T-17 + PR 7).
    /// `definitions_only: Some(true)` keeps only DEFINE sites,
    /// `Some(false)` only uses.
    pub fn sites(&self, key: &SymbolKey, definitions_only: Option<bool>) -> Vec<Location> {
        self.sites_at(key, definitions_only, false)
            .into_iter()
            .map(|s| s.loc)
            .collect()
    }

    /// All sites of the symbol with quoting and DEFINE type — what rename
    /// needs to re-quote each site and to run its same-name-different-type
    /// safety gate. OPEN ENTRIES ONLY: a closed entry's content is only as
    /// fresh as the last scan, and a mass edit must not rest on a stale
    /// guess. PR 8's watched reindexing is what graduates rename to the
    /// full index; until then references may SHOW a closed site that
    /// rename will not touch, and the refusal is honest at that boundary.
    pub fn rename_sites(&self, key: &SymbolKey) -> Vec<SiteAt> {
        self.sites_at(key, None, true)
    }

    fn sites_at(
        &self,
        key: &SymbolKey,
        definitions_only: Option<bool>,
        open_only: bool,
    ) -> Vec<SiteAt> {
        let mut out = Vec::new();
        let ns = key.scope.namespace();
        let docs = rd(&self.docs);
        for (u, doc) in docs.iter() {
            if open_only && !matches!(doc.source, Source::Open { .. }) {
                continue;
            }
            for s in doc
                .sites
                .iter()
                .filter(|s| s.name == key.name && s.scope.namespace() == ns)
            {
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

// ----------------------------------------------------------- filesystem (PR 7)

/// Index one closed file from disk: definitions + name sites, nothing
/// kept of the text or the tree. `None` when the file cannot be read, is
/// oversize or is not valid enough to parse. PR 8 reuses this for every
/// watched create/change event.
pub(crate) fn disk_entry(path: &Path) -> Option<Entry> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > SCAN_MAX_FILE_BYTES {
        return None;
    }
    disk_entry_from_bytes(&bytes)
}

fn disk_entry_from_bytes(bytes: &[u8]) -> Option<Entry> {
    // same policy as the --check CLI: a lossy decode is worth indexing,
    // undecodable garbage is not worth an entry
    let text = String::from_utf8_lossy(bytes);
    let tree = analysis::parse(&text);
    let defs = analysis::collect_definitions(&tree, text.as_bytes(), Model::load());
    let sites = analysis::object_index(&text, &tree, Model::load()).sites;
    Some(Entry {
        source: Source::Closed,
        defs,
        sites,
    })
}

/// Walk the workspace roots for `*.mqsc` and index them, honoring
/// `WorkspaceScan`'s exclusions and cap. Pure filesystem + parse: no
/// index access, so the caller runs it on a blocking thread and merges
/// the outcome afterwards. Paths are sorted before the cap, making the
/// indexed subset deterministic across runs.
pub fn collect_scan(roots: &[PathBuf], cfg: &WorkspaceScan) -> ScanOutcome {
    let mut paths: Vec<PathBuf> = Vec::new();
    for root in roots {
        // Absolute paths only: Url::from_file_path refuses relative ones,
        // and canonicalizing up front makes overlapping roots collapse onto
        // the same file paths (the dedup below relies on that).
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        walk(&root, cfg, &mut paths);
    }
    paths.sort();
    paths.dedup();
    let found = paths.len();
    let files: Vec<ScannedFile> = paths
        .into_iter()
        .take(cfg.max_files)
        .filter_map(|p| {
            let uri = Url::from_file_path(&p).ok()?;
            let bytes = std::fs::read(&p).ok()?;
            if bytes.len() > SCAN_MAX_FILE_BYTES {
                return None;
            }
            let entry = disk_entry_from_bytes(&bytes)?;
            Some(ScannedFile {
                uri,
                defs: entry.defs,
                sites: entry.sites,
            })
        })
        .collect();
    ScanOutcome {
        found,
        indexed: files.len(),
        files,
    }
}

fn walk(dir: &Path, cfg: &WorkspaceScan, out: &mut Vec<PathBuf>) {
    // iterative: a deep tree must not blow the stack of a scan thread
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                // symlinked directories are never followed (cycles);
                // excluded directories are never entered
                if !cfg.exclude.iter().any(|x| *x == name) {
                    stack.push(entry.path());
                }
            } else if name
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("mqsc"))
            {
                out.push(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::ObjectScope;
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

    /// A throwaway directory under the temp dir, cleaned before use.
    fn temp_ws(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("mqsc-lsp-ws-{tag}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn queue_key(name: &str) -> SymbolKey {
        SymbolKey {
            name: name.to_string(),
            scope: ObjectScope::QueueFamily,
        }
    }

    #[test]
    fn lifecycle_len_and_uris() {
        let (index, a, b) = fixture();
        assert_eq!(index.len(), 2);
        assert!(!index.is_empty());
        assert_eq!(index.open_uris_excluding(&a), vec![b.clone()]);
        assert!(index.close(&b));
        assert!(!index.close(&b));
        assert_eq!(index.len(), 1);
        assert_eq!(index.open_uris(), vec![a]);
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

    // ------------------------------------------------------------ PR 7 scan

    #[test]
    fn scan_finds_mqsc_skips_excluded_dirs_and_caps() {
        let dir = temp_ws("scan");
        std::fs::write(dir.join("a.mqsc"), "DEFINE QLOCAL(A)\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.mqsc"), "DEFINE QLOCAL(B)\n").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/c.mqsc"), "DEFINE QLOCAL(C)\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "DEFINE QLOCAL(T)\n").unwrap();

        let all = collect_scan(&[dir.clone()], &WorkspaceScan::default());
        assert_eq!(all.found, 2, "a + sub/b; target/ excluded, .txt ignored");
        assert_eq!(all.indexed, 2);

        let capped = collect_scan(
            std::slice::from_ref(&dir),
            &WorkspaceScan {
                max_files: 1,
                ..Default::default()
            },
        );
        assert_eq!(capped.found, 2, "found is what is there");
        assert_eq!(capped.indexed, 1, "indexed is what fits the cap");
        assert_eq!(
            capped.files[0].uri.to_file_path().unwrap(),
            dir.join("a.mqsc"),
            "sorted paths, deterministic subset"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn closed_files_join_every_read_feature() {
        let dir = temp_ws("closed");
        let disk = dir.join("other.mqsc");
        std::fs::write(&disk, "DEFINE QALIAS(AL) TARGET(DDQ)\n").unwrap();
        let index = WorkspaceIndex::default();
        let out = collect_scan(std::slice::from_ref(&dir), &WorkspaceScan::default());
        assert_eq!(index.merge_scanned(out.files), 1);
        let a = open_text(&index, "/tmp/closed-a.mqsc", "DEFINE QLOCAL(DDQ)\n");

        // hint 12 / completion: the closed DEFINE silences references
        let ext = index.external_defs(&a);
        assert!(ext.names.contains("AL"), "closed defines count");
        // workspace/symbol + hover map: closed DEFINE sites appear
        let defs = index.definitions(Some(&a));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].uri.to_file_path().unwrap(), disk);
        // definition: the closed use site is found from the queue side
        let locs = index.sites(&queue_key("DDQ"), Some(false));
        assert_eq!(locs.len(), 1, "TARGET(DDQ) inside the closed file");
        assert_eq!(locs[0].uri.to_file_path().unwrap(), disk);
        // closed entries have no with_doc — full-service needs the buffer
        assert!(index.with_doc(&defs[0].uri, |_t, _tr| ()).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_still_refuses_the_closed_side_until_pr8() {
        let dir = temp_ws("rename");
        std::fs::write(dir.join("other.mqsc"), "DEFINE QALIAS(AL) TARGET(AA)\n").unwrap();
        let index = WorkspaceIndex::default();
        index.merge_scanned(
            collect_scan(std::slice::from_ref(&dir), &WorkspaceScan::default()).files,
        );
        open_text(&index, "/tmp/rename-a.mqsc", "DEFINE QLOCAL(AA)\n");

        // references sees both sides ...
        assert_eq!(index.sites(&queue_key("AA"), None).len(), 2);
        // ... rename edits only the open one: a mass edit may not rest on
        // a scanned-then-possibly-changed file the watcher (PR 8) cannot
        // vouch for yet
        assert_eq!(index.rename_sites(&queue_key("AA")).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_overrides_disk_then_close_falls_back_to_disk() {
        let dir = temp_ws("override");
        let disk = dir.join("d.mqsc");
        std::fs::write(&disk, "DEFINE QLOCAL(DISK)\n").unwrap();
        let uri = Url::from_file_path(&disk).unwrap();
        let index = WorkspaceIndex::default();
        let other = open_text(&index, "/tmp/override-other.mqsc", "DEFINE QLOCAL(O)\n");

        // while open, the BUFFER is the truth — disk content must not leak
        open_text(&index, disk.to_str().unwrap(), "DEFINE QLOCAL(BUF)\n");
        let ext = index.external_defs(&other);
        assert!(ext.names.contains("BUF") && !ext.names.contains("DISK"));

        // didClose re-reads the file: the unsaved buffer's name is gone,
        // the on-disk name stays in the index
        assert!(index.close(&uri));
        assert_eq!(index.len(), 1, "one open document left");
        assert_eq!(index.total(), 2, "d.mqsc remained as a closed entry");
        let ext = index.external_defs(&other);
        assert!(
            ext.names.contains("DISK") && !ext.names.contains("BUF"),
            "disk, not buffer"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_scanned_never_overrides_open_or_existing() {
        let dir = temp_ws("merge");
        let disk = dir.join("d.mqsc");
        std::fs::write(&disk, "DEFINE QLOCAL(DISK)\n").unwrap();
        let uri = Url::from_file_path(&disk).unwrap();
        let index = WorkspaceIndex::default();
        open_text(&index, disk.to_str().unwrap(), "DEFINE QLOCAL(BUF)\n");
        let out = collect_scan(std::slice::from_ref(&dir), &WorkspaceScan::default());
        assert_eq!(index.merge_scanned(out.files.clone()), 0, "open entry wins");
        assert_eq!(index.merge_scanned(out.files), 0, "still one entry");
        assert_eq!(index.total(), 1);
        let seen = index.with_doc(&uri, |text, _t| text.to_string()).unwrap();
        assert!(seen.contains("BUF"), "with_doc still sees the buffer");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn close_of_missing_file_drops_the_entry() {
        let index = WorkspaceIndex::default();
        let gone = "/tmp/mqsc-lsp-nonexistent-should-not-exist.mqsc";
        open_text(&index, gone, "DEFINE QLOCAL(X)\n");
        assert!(index.close(&Url::from_file_path(gone).unwrap()));
        assert_eq!(index.total(), 0, "no disk file to fall back to");
    }
}
