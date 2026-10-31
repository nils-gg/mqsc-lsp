//! tower-lsp server implementation for MQSC.
//!
//! Cross-document knowledge lives in [`crate::workspace`]; this module is
//! only the LSP protocol layer on top of it (PR 6 centralization).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use lsp_types::*;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::analysis;
use crate::model::Model;
use crate::settings::{
    apply_diagnostic_settings, diagnostic_settings, Navigation, Target, WorkspaceScan,
};
use crate::workspace::{self, rd, wr, WorkspaceIndex};

pub struct MqscServer {
    pub client: Client,
    /// Everything known about object names across documents: open ones
    /// plus, since PR 7, closed `*.mqsc` files found under the workspace
    /// roots behind the same API.
    index: WorkspaceIndex,
    /// Client settings, see README "Configuration":
    /// `{"diagnostics": {"CODE_NAME_OR_NUMBER": "error"|"warning"|"information"|"hint"|"off"}}`
    settings: RwLock<Option<serde_json::Value>>,
    /// Whether the client resolves snippet placeholders in completions.
    snippet_support: AtomicBool,
    /// Filesystem roots of the workspace (from `initialize`), remembered
    /// for the PR 7 closed-file scan that runs in `initialized` (the scan
    /// needs the client's settings, which only exist after initialize).
    ws_roots: RwLock<Vec<PathBuf>>,
    /// The scan/watch settings as they were at `initialize`. PR 8's
    /// watched-file reindexing consults the same snapshot, so the whole
    /// workspace index honors one restart-only contract.
    ws_scan: RwLock<WorkspaceScan>,
    /// Whether the client accepts dynamic registration of
    /// `workspace/didChangeWatchedFiles` — the only way a server can ask
    /// for file events (LSP has no static server-side file watching).
    dyn_watch: AtomicBool,
}

impl MqscServer {
    pub fn new(client: Client) -> Self {
        MqscServer {
            client,
            index: WorkspaceIndex::default(),
            settings: RwLock::new(None),
            snippet_support: AtomicBool::new(false),
            ws_roots: RwLock::new(Vec::new()),
            ws_scan: RwLock::new(WorkspaceScan::default()),
            dyn_watch: AtomicBool::new(false),
        }
    }

    /// Publish diagnostics for an open document, applying severity overrides.
    /// Uses the document's cached parse tree (callers must have stored it).
    /// Diagnostics see the DEFINEs of every OTHER entry in the index too —
    /// open siblings and scanned workspace files alike (T-17 + PR 7): a
    /// name defined anywhere else stops the code-12 hint.
    async fn refresh(&self, uri: &Url) {
        let diags = {
            let extra = self.index.external_defs(uri);
            let open_docs = self.index.len();
            let ws_files = self.index.total();
            let cfg = rd(&self.settings);
            let target = cfg.as_ref().map(Target::from_settings).unwrap_or_default();
            let built = self.index.with_doc(uri, |text, tree| {
                let t0 = std::time::Instant::now();
                let mut diags =
                    analysis::analyze_with_target(text, tree, Model::load(), uri, &extra, &target);
                crate::tlog!(
                    "analyze {uri} len={} diags={} open_docs={} ws_files={} in {}ms",
                    text.len(),
                    diags.len(),
                    open_docs,
                    ws_files,
                    t0.elapsed().as_millis()
                );
                if let Some(map) = cfg.as_ref().and_then(diagnostic_settings) {
                    apply_diagnostic_settings(&mut diags, map);
                }
                diags
            });
            match built {
                Some(diags) => diags,
                None => return,
            }
        }; // settings guard dropped before the await
        self.client
            .publish_diagnostics(uri.clone(), diags, None)
            .await;
    }

    /// Cross-file definitions for hover: name -> (file display name,
    /// 0-based line, object type), from the other documents in the index.
    fn external_defs_map(&self, uri: &Url) -> analysis::ExternalDefs {
        let mut map: analysis::ExternalDefs = Default::default();
        for d in self.index.definitions(Some(uri)) {
            map.entry(d.name).or_default().push((
                display_file(&d.uri),
                d.range.start.line,
                d.defines.unwrap_or_else(|| "OBJECT".into()),
            ));
        }
        map
    }

    /// Re-publish diagnostics for every open document except `skip`. Called
    /// on did_open/did_close (T-17): the set of cross-file definitions just
    /// changed for every other document. Not called on did_change — that
    /// would be O(open documents) per keystroke.
    async fn refresh_others(&self, skip: &Url) {
        for u in self.index.open_uris_excluding(skip) {
            self.refresh(&u).await;
        }
    }

    /// Ask the client to watch `**/*.mqsc` and report events to
    /// `workspace/didChangeWatchedFiles` (PR 8). The spec offers no static
    /// server-side file watching, so this request is the only route; a client
    /// that refuses simply leaves the entries as scanned.
    async fn register_file_watcher(&self) {
        let watchers = vec![FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/*.mqsc".into()),
            // kind omitted = create | change | delete, which is all three we handle
            kind: None,
        }];
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let reg = Registration {
            id: "mqsc-workspace-files".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(options).ok(),
        };
        match self.client.register_capability(vec![reg]).await {
            Ok(()) => crate::tlog!("watched files: registered **/*.mqsc with the client"),
            Err(e) => crate::tlog!(
                "watched files: client refused registration ({e}) \
                 — closed files stay as scanned until the server restarts"
            ),
        }
    }

    /// `mqsc.crossFileNavigation` (see `settings::Navigation`): whether
    /// `definition` and `rename` may touch other documents. Read live on
    /// every request like the severities — the scope of an edit is exactly
    /// what the user is configuring, so it must react immediately.
    fn nav_cross_file(&self) -> bool {
        match rd(&self.settings).as_ref() {
            Some(cfg) => Navigation::from_settings(cfg).cross_file,
            None => false,
        }
    }

    fn with_doc<T>(
        &self,
        uri: &Url,
        f: impl FnOnce(&str, &tree_sitter::Tree) -> RpcResult<T>,
    ) -> RpcResult<T> {
        match self.index.with_doc(uri, f) {
            Some(result) => result,
            None => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "document not open: {uri}"
            ))),
        }
    }
}

/// Last path segment of a URI (or the whole path when it has none), for
/// user-visible "defined in file X" text.
fn display_file(uri: &Url) -> String {
    let p = uri.path();
    match p.rsplit('/').next() {
        Some(seg) if !seg.is_empty() => seg.to_string(),
        _ => p.to_string(),
    }
}

/// Replacement text for one rename site: quoted sites stay quoted; a name
/// that no longer fits the grammar's bare token syntax must be written as a
/// 'string'. Bare forms: the word token `[A-Za-z_#$@%/.][-A-Za-z0-9_#$@%/.]*`
/// or a pure integer (`QLOCAL(123)` names a queue — runmqsc proves it), so
/// digit-initial MIXED names like `1BAD` still take quotes while a numeric
/// site renamed to `456` stays bare.
fn rename_text(new_name: &str, site_quoted: bool) -> String {
    let bare_ok = match new_name.chars().next() {
        Some(c) if c.is_ascii_digit() => new_name.chars().all(|c| c.is_ascii_digit()),
        Some(c) if c.is_ascii_alphabetic() || "_#$@%/.".contains(c) => new_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_#$@%/.-".contains(c)),
        _ => false,
    };
    if site_quoted || !bare_ok {
        format!("'{}'", new_name.replace('\'', "''"))
    } else {
        new_name.to_string()
    }
}

/// The name the rename caller meant, before it reaches `rename_text`:
/// whitespace that came with a selection is trimmed, and a name still
/// wrapped in `'quotes'` is unwrapped. That wrapper arrives from clients
/// that skip the prepareRename gate (e.g. helix without `rename-support
/// prepare-support`) and echo their own quote-included selection back as
/// the new name; unwrapping here keeps quoted sites from being rewritten
/// as `''NAME''`. Returns None when nothing of a name is left.
fn rename_target(raw: &str) -> Option<String> {
    let name = analysis::unquote_name(raw.trim());
    (!name.trim().is_empty()).then_some(name)
}

/// prepareRename answer for one site: editors select the returned range
/// verbatim, so a quoted site reports the name INSIDE the quotes — the
/// whole token is what the rename edits later replace (they rewrite the
/// quoting), but selecting `'NAME'` with quotes is the wrong selection.
/// The placeholder is the name as written, unquoted.
fn prepare_site(s: &analysis::NameSite) -> PrepareRenameResponse {
    let range = if s.quoted && s.range.start.line == s.range.end.line {
        Range::new(
            Position::new(s.range.start.line, s.range.start.character + 1),
            Position::new(s.range.end.line, s.range.end.character.saturating_sub(1)),
        )
    } else {
        s.range
    };
    PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: s.word.clone(),
    }
}

#[async_trait]
impl LanguageServer for MqscServer {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        // warm the model so the first request is fast
        let _ = Model::load();

        if params.initialization_options.is_some() {
            *wr(&self.settings) = params.initialization_options.clone();
        }
        let snippet = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.snippet_support)
            .unwrap_or(false);
        self.snippet_support.store(snippet, Ordering::Relaxed);

        // PR 7: remember where the workspace lives on disk so initialized()
        // can scan it for closed *.mqsc files. workspace_folders win; the
        // deprecated rootUri is the fallback for old clients. Both must
        // resolve to local paths — remote schemes (vscode-remote, ssh) are
        // skipped, the server cannot read them.
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(folders) = params.workspace_folders.as_ref() {
            for f in folders {
                if let Ok(p) = f.uri.to_file_path() {
                    roots.push(p);
                }
            }
        }
        if roots.is_empty() {
            if let Some(root) = params.root_uri.as_ref().and_then(|u| u.to_file_path().ok()) {
                roots.push(root);
            }
        }
        *wr(&self.ws_roots) = roots;

        // PR 7/8 contract: the workspace index is configured once, here.
        // Settings changes later must not silently change what is indexed
        // or watched mid-session (documented as restart-only).
        let scan = params
            .initialization_options
            .as_ref()
            .map(WorkspaceScan::from_settings)
            .unwrap_or_default();
        crate::tlog!(
            "workspace index settings: enabled={} max_files={} exclude={:?}",
            scan.enabled,
            scan.max_files,
            scan.exclude
        );
        *wr(&self.ws_scan) = scan;
        // LSP 3.17 is explicit that file changes "from the server side" have
        // no static configuration: the server has to register dynamically.
        self.dyn_watch.store(
            params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|w| w.did_change_watched_files.as_ref())
                .and_then(|w| w.dynamic_registration)
                .unwrap_or(false),
            Ordering::Relaxed,
        );

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "mqsc-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    // " " so the logical-next-word lists (object types
                    // after a verb, names after the object word, params
                    // after the name) open as soon as a token is committed;
                    // "(", "," and ":" open value completion inside groups.
                    trigger_characters: Some(vec![" ".into(), "(".into(), ",".into(), ":".into()]),
                    resolve_provider: Some(false),
                    completion_item: Some(CompletionOptionsCompletionItem {
                        label_details_support: Some(true),
                    }),
                    ..Default::default()
                }),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // Options (not Left(true)) so prepareProvider is advertised:
                // clients then gate rename through prepare_rename below,
                // which is what keeps the prompt off verbs and the editor
                // selection honest around 'quoted' names.
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: analysis::TOKEN_TYPES
                                    .iter()
                                    .map(|t| lsp_types::SemanticTokenType::from(*t))
                                    .collect(),
                                token_modifiers: analysis::TOKEN_MODIFIERS
                                    .iter()
                                    .map(|m| lsp_types::SemanticTokenModifier::from(*m))
                                    .collect(),
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(true),
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        crate::tlog!("client initialized — ready");
        self.client
            .log_message(MessageType::INFO, "MQSC language server ready")
            .await;

        // PR 7: index the workspace's closed *.mqsc files. The walk +
        // parse is blocking work (fs reads + tree-sitter parses), so it
        // runs on tokio's blocking pool — the LocalSet must never wait on
        // a 300-file scan. Settings are read before the await; no lock
        // guard may cross it (futures must stay Send).
        let roots = rd(&self.ws_roots).clone();
        let scan = rd(&self.ws_scan).clone();
        if !scan.enabled || roots.is_empty() {
            crate::tlog!(
                "workspace scan skipped (enabled={}, roots={})",
                scan.enabled,
                roots.len()
            );
            return; // index off: nothing to index, and nothing to watch either
        }
        let max_files = scan.max_files;
        let t0 = std::time::Instant::now();
        let outcome =
            match tokio::task::spawn_blocking(move || workspace::collect_scan(&roots, &scan)).await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    crate::tlog!("workspace scan failed to run: {e}");
                    return;
                }
            };
        let added = self.index.merge_scanned(outcome.files);
        crate::tlog!(
            "workspace scan: found={} indexed={} merged={} in {}ms",
            outcome.found,
            outcome.indexed,
            added,
            t0.elapsed().as_millis()
        );
        // The scan changes what every ALREADY OPEN document can resolve,
        // and files opened during startup (before the scan finished) were
        // published without it — republish so hint 12 cannot survive on a
        // name the workspace just defined. Same rule as did_open's
        // refresh-all, for the same reason.
        if added > 0 {
            for u in self.index.open_uris() {
                self.refresh(&u).await;
            }
        }
        if outcome.found > outcome.indexed {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!(
                        "mqsc-lsp: indexed {}/{} *.mqsc files (mqsc.workspaceIndex.maxFiles = {}); \
                         some workspace files are not visible to cross-file features",
                        outcome.indexed, outcome.found, max_files
                    ),
                )
                .await;
        }

        // PR 8: ask the client to tell us when those files change. Registered
        // AFTER the scan on purpose — a client that ignores the request then
        // costs us nothing but freshness, never the index itself. The window
        // between the walk passing a directory and the watcher starting is
        // self-healing: any later change to such a file reaches us, and a
        // file that never changes was already read from disk if the walk had
        // not reached it yet.
        if self.dyn_watch.load(Ordering::Relaxed) {
            self.register_file_watcher().await;
        } else {
            crate::tlog!(
                "watched files: client has no dynamic registration for didChangeWatchedFiles \
                 — closed files stay as scanned until the server restarts"
            );
        }
    }

    /// PR 8: keep the index current. One file per event (never a rescan), the
    /// read + parse off the LocalSet, and open documents untouched — saving a
    /// file fires an event for it even though `didChange` already delivered
    /// that content, and the buffer may hold more than the disk does.
    async fn did_change_watched_files(&self, p: DidChangeWatchedFilesParams) {
        let kinds = p
            .changes
            .iter()
            .map(|c| match c.typ {
                t if t == FileChangeType::CREATED => "created",
                t if t == FileChangeType::DELETED => "deleted",
                _ => "changed",
            })
            .collect::<Vec<_>>()
            .join(",");
        crate::tlog!(
            "didChangeWatchedFiles: {} events [{kinds}]",
            p.changes.len()
        );
        let cfg = rd(&self.ws_scan).clone();
        if !cfg.enabled || p.changes.is_empty() {
            return;
        }
        let roots = rd(&self.ws_roots).clone();
        let changes = p.changes;
        let cfg_for_scan = cfg.clone();
        let t0 = std::time::Instant::now();
        let updates = match tokio::task::spawn_blocking(move || {
            workspace::reindex_from_events(&changes, &cfg_for_scan, &roots)
        })
        .await
        {
            Ok(updates) => updates,
            Err(e) => {
                crate::tlog!("watched reindex failed to run: {e}");
                return;
            }
        };
        let applied = self.index.apply_disk_updates(updates, &cfg);
        crate::tlog!(
            "watched reindex: applied={} capped={} ws_files={} in {}ms",
            applied.changed,
            applied.capped,
            self.index.total(),
            t0.elapsed().as_millis()
        );
        if applied.changed == 0 {
            return;
        }
        // the change is visible to every OTHER document's references, so the
        // republish rule is did_open's again
        for u in self.index.open_uris() {
            self.refresh(&u).await;
        }
    }

    async fn shutdown(&self) -> RpcResult<()> {
        crate::tlog!("shutdown requested");
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        crate::tlog!("didChangeConfiguration: settings {}", params.settings);
        *wr(&self.settings) = if params.settings.is_null() {
            None
        } else {
            Some(params.settings)
        };
        // severities may have changed: republish everything we know about
        let uris = self.index.open_uris();
        for uri in uris {
            self.refresh(&uri).await;
        }
    }

    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = p.text_document.text;
        let t0 = std::time::Instant::now();
        let tree = analysis::parse(&text);
        let defs = analysis::collect_definitions(&tree, text.as_bytes(), Model::load());
        crate::tlog!(
            "didOpen {uri} len={} defines={} parse={}ms",
            text.len(),
            defs.names.len(),
            t0.elapsed().as_millis()
        );
        self.index.open(uri.clone(), text, tree, defs);
        self.refresh(&uri).await;
        // T-17: this document's DEFINEs change the hints of every other
        // open document too; opening is rare enough to refresh all of them.
        self.refresh_others(&uri).await;
    }

    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        let version = p.text_document.version;
        let text = match p.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };
        let t0 = std::time::Instant::now();
        let tree = analysis::parse(&text);
        let defs = analysis::collect_definitions(&tree, text.as_bytes(), Model::load());
        crate::tlog!(
            "didChange {uri} v{version} len={} defines={} parse={}ms",
            text.len(),
            defs.names.len(),
            t0.elapsed().as_millis()
        );
        self.index.open(uri.clone(), text, tree, defs);
        // Sibling documents are deliberately NOT re-analysed on every keystroke
        // (O(open documents) per change); their cross-file hints refresh on
        // their own next change and on open/close events.
        self.refresh(&uri).await;
    }

    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        crate::tlog!("didClose {}", p.text_document.uri);
        self.index.close(&p.text_document.uri);
        // The remaining documents lost (or, PR 7: swap) their external
        // definitions — the file usually stays in the index as a CLOSED
        // entry read back from disk, so a sibling that referenced the
        // on-disk DEFINE keeps seeing it.
        self.refresh_others(&p.text_document.uri).await;
    }

    async fn hover(&self, p: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = &p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let ext = self.external_defs_map(uri);
        self.with_doc(uri, |text, tree| {
            Ok(analysis::hover(text, tree, Model::load(), pos, &ext))
        })
    }

    async fn completion(&self, p: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = &p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let snippet = self.snippet_support.load(Ordering::Relaxed);
        let extra = self.index.external_defs(uri);
        self.with_doc(uri, |text, tree| {
            let items = analysis::completions(text, tree, Model::load(), pos, snippet, &extra);
            Ok(Some(CompletionResponse::Array(items)))
        })
    }

    /// Quickfixes for the typo diagnostics: analyze() attaches suggested
    /// replacement strings to `Diagnostic.data`, the client echoes them back
    /// here, and each one becomes a "Change to `X`" edit over the diagnostic
    /// range itself.
    async fn code_action(&self, p: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = CodeActionResponse::new();
        if let Some(only) = &p.context.only {
            let quickfix = CodeActionKind::QUICKFIX;
            if !only.is_empty()
                && !only
                    .iter()
                    .any(|k| k.as_str() == quickfix.as_str() || k.as_str().starts_with("quickfix."))
            {
                return Ok(Some(actions));
            }
        }
        for d in p
            .context
            .diagnostics
            .iter()
            .filter(|d| d.source.as_deref() == Some("mqsc"))
        {
            let fixes = match d
                .data
                .as_ref()
                .and_then(|v| v.get("quickfixes"))
                .and_then(|v| v.as_array())
            {
                Some(a) => a,
                None => continue,
            };
            // Each diagnostic gets its first suggestion marked preferred, so
            // "quick fix" (alt+enter -> first) picks the nearest replacement
            // for THAT diagnostic, not just for the first one reported.
            let mut first_for_this_diag = true;
            for f in fixes {
                let Some(new_text) = f.as_str() else { continue };
                let mut changes = HashMap::new();
                changes.insert(
                    p.text_document.uri.clone(),
                    vec![TextEdit {
                        range: d.range,
                        new_text: new_text.to_string(),
                    }],
                );
                let preferred = first_for_this_diag;
                first_for_this_diag = false;
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Change to `{new_text}`"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![d.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    is_preferred: Some(preferred),
                    ..Default::default()
                }));
            }
        }
        Ok(Some(actions))
    }

    async fn document_symbol(
        &self,
        p: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text, tree| {
            Ok(Some(DocumentSymbolResponse::Nested(analysis::symbols(
                text, tree,
            ))))
        })
    }

    async fn folding_range(&self, p: FoldingRangeParams) -> RpcResult<Option<Vec<FoldingRange>>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text, tree| Ok(Some(analysis::folding(text, tree))))
    }

    async fn document_highlight(
        &self,
        p: DocumentHighlightParams,
    ) -> RpcResult<Option<Vec<DocumentHighlight>>> {
        let uri = &p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        self.with_doc(uri, |text, tree| {
            Ok(Some(analysis::highlights(text, tree, pos)))
        })
    }

    /// T-16/T-17: jump from any use of an object name to its DEFINE, inside
    /// the object's namespace (a QUEUE and a same-named CHANNEL never share
    /// a definition list). Duplicate names inside one namespace (a queue
    /// defined both here and in a sibling) all come back as locations,
    /// sorted for a stable pick list. Scope: by default the jump stays in
    /// the edited document (`mqsc.crossFileNavigation=false` — the DEFINE
    /// must live in this file); the setting restores workspace-wide jumps.
    async fn goto_definition(
        &self,
        p: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let key = self.with_doc(&uri, |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            Ok(idx.symbol_at(pos))
        })?;
        let Some(key) = key else { return Ok(None) };
        let mut locs = if self.nav_cross_file() {
            self.index.sites(&key, Some(true))
        } else {
            self.index.sites_in_file(&key, Some(true), &uri)
        };
        if locs.is_empty() {
            return Ok(None);
        }
        locs.sort_by_key(|l| {
            (
                l.uri.to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        });
        let response = if locs.len() == 1 {
            GotoDefinitionResponse::Scalar(locs.pop().unwrap())
        } else {
            GotoDefinitionResponse::Array(locs)
        };
        Ok(Some(response))
    }

    async fn references(&self, p: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let key = self.with_doc(&uri, |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            Ok(idx.symbol_at(pos))
        })?;
        let Some(key) = key else { return Ok(None) };
        // every site of the symbol; the client's includeDeclaration flag
        // decides whether the DEFINE sites belong to the answer
        let definitions_only = if p.context.include_declaration {
            None
        } else {
            Some(false)
        };
        let mut locs = self.index.sites(&key, definitions_only);
        locs.sort_by_key(|l| {
            (
                l.uri.to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        });
        Ok(Some(locs))
    }

    /// textDocument/prepareRename — the rename gate and its exact range.
    /// Clients call this before prompting, so words that are not object
    /// names (the DEFINE verb, the QLOCAL object-type word, plain values
    /// like DESCR('...')) never open a rename box at all, and for a
    /// 'quoted' name the returned range covers the name without quotes.
    async fn prepare_rename(
        &self,
        p: TextDocumentPositionParams,
    ) -> RpcResult<Option<PrepareRenameResponse>> {
        let uri = &p.text_document.uri;
        let pos = p.position;
        self.with_doc(uri, |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            Ok(idx.site_at(pos).map(prepare_site))
        })
    }

    /// Rename every occurrence of the object under the cursor — name AND
    /// namespace, so a QUEUE and a same-named CHANNEL are separate rename
    /// targets — keeping each site's quoting style (quoted sites stay
    /// quoted; a name that no longer fits the bare-word syntax is quoted
    /// everywhere). Scope: by default only the edited document is touched
    /// (`mqsc.crossFileNavigation=false`); the setting restores the
    /// all-documents mass edit.
    ///
    /// Safety gate: the target object must resolve uniquely. If the name is
    /// DEFINEd under more than one object type inside the namespace (a
    /// QLOCAL and a QALIAS of the same name — which real MQ rejects, but a
    /// broken script can contain), a rename cannot tell which object every
    /// reference means, and refuses rather than rewriting every spelling.
    /// The gate sees exactly the scoped sites: same-file mode judges the
    /// current document, cross-file mode the whole index.
    async fn rename(&self, p: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        // Normalize the caller's name first: see `rename_target` (quoted
        // selections from clients that skipped prepareRename would else
        // double-quote every quoted site).
        let new_name = match rename_target(&p.new_name) {
            Some(n) => n,
            None => {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "rename needs a non-empty new name",
                ))
            }
        };
        let key = self.with_doc(&uri, |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            Ok(idx.symbol_at(pos))
        })?;
        let Some(key) = key else {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "rename needs the cursor on an object name",
            ));
        };
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut def_types: Vec<String> = Vec::new();
        let sites = if self.nav_cross_file() {
            self.index.rename_sites(&key)
        } else {
            self.index.rename_sites_in_file(&key, &uri)
        };
        for site in sites {
            if let Some(t) = &site.defines {
                if !def_types.contains(t) {
                    def_types.push(t.clone());
                }
            }
            changes
                .entry(site.loc.uri.clone())
                .or_default()
                .push(TextEdit {
                    range: site.loc.range,
                    new_text: rename_text(&new_name, site.quoted),
                });
        }
        if def_types.len() > 1 {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "'{}' is DEFINEd as {} within the rename's scope — the rename \
                 cannot tell which object the shared name refers to; rename \
                 from a definition after disambiguating the spelling",
                key.name,
                def_types.join(" and ")
            )));
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    /// T-20: every object DEFINEd in every open document, grouped in the
    /// client's outline by their object type (container_name).
    /// (tower-lsp names the `workspace/symbol` handler `symbol`.)
    /// `deprecated` stays None: lsp-types 0.94 requires the field in the
    /// struct literal even though tags supersede it.
    #[allow(deprecated)]
    async fn symbol(&self, p: WorkspaceSymbolParams) -> RpcResult<Option<Vec<SymbolInformation>>> {
        let query = p.query.to_ascii_uppercase();
        let out = self
            .index
            .definitions(None)
            .into_iter()
            .filter(|d| query.is_empty() || d.name.contains(&query))
            .map(|d| SymbolInformation {
                name: d.word,
                kind: SymbolKind::METHOD, // matches the documentSymbol kinds
                tags: None,
                deprecated: None,
                location: Location {
                    uri: d.uri,
                    range: d.range,
                },
                container_name: d.defines,
            })
            .collect();
        Ok(Some(out))
    }

    async fn semantic_tokens_full(
        &self,
        p: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text, tree| {
            Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: analysis::semantic_tokens(text, tree),
            })))
        })
    }

    async fn semantic_tokens_range(
        &self,
        p: SemanticTokensRangeParams,
    ) -> RpcResult<Option<SemanticTokensRangeResult>> {
        let uri = &p.text_document.uri;
        let range = p.range;
        self.with_doc(uri, |text, tree| {
            Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: analysis::semantic_tokens_range(text, tree, range),
            })))
        })
    }
}

// ---------------------------------------------------------------- tests (T-20)

#[cfg(test)]
mod tests {
    use super::{rename_target, rename_text};
    use lsp_types::PrepareRenameResponse;

    #[test]
    fn quoted_sites_stay_quoted() {
        assert_eq!(rename_text("NEW1", true), "'NEW1'");
    }

    #[test]
    fn rename_target_unwraps_client_quotes_and_trims() {
        // plain input passes through
        assert_eq!(rename_target("NEW1").as_deref(), Some("NEW1"));
        // a client that skipped prepareRename may echo the selection with
        // quotes: the name must NOT become ''NEW1'' at quoted sites
        assert_eq!(rename_target("'NEW1'").as_deref(), Some("NEW1"));
        // doubled inner quotes unescape like any MQSC string
        assert_eq!(rename_target("'IT''S'").as_deref(), Some("IT'S"));
        // selection whitespace is trimmed off the name
        assert_eq!(rename_target("  NEW1  ").as_deref(), Some("NEW1"));
        assert_eq!(rename_target(" 'NEW1' ").as_deref(), Some("NEW1"));
        // nothing of a name left: rejected
        assert_eq!(rename_target(""), None);
        assert_eq!(rename_target("   "), None);
        assert_eq!(rename_target("''"), None);
    }

    #[test]
    fn bare_sites_follow_grammar_syntax() {
        assert_eq!(rename_text("NEW1", false), "NEW1");
        assert_eq!(rename_text("NEW NAME", false), "'NEW NAME'"); // spaces need quotes
        assert_eq!(rename_text("1BAD", false), "'1BAD'"); // digit+letter is no bare token
        assert_eq!(rename_text("456", false), "456"); // all digits = the integer name token
        assert_eq!(rename_text("IT'S", false), "'IT''S'"); // MQSC doubles embedded quotes
        assert_eq!(rename_text("", true), "''"); // degenerate but never bare
    }

    fn site_at_range(quoted: bool) -> crate::analysis::NameSite {
        crate::analysis::NameSite {
            name: "SHOP.FRONT".into(),
            word: "SHOP.FRONT".into(),
            // the token `SHOP.FRONT` written 'quoted' at columns 14..26
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 14),
                lsp_types::Position::new(0, 26),
            ),
            quoted,
            defines: None,
            scope: crate::analysis::ObjectScope::QueueFamily,
        }
    }

    #[test]
    fn prepare_range_of_quoted_site_drops_the_quotes() {
        match super::prepare_site(&site_at_range(true)) {
            PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
                assert_eq!(
                    range,
                    lsp_types::Range::new(
                        lsp_types::Position::new(0, 15),
                        lsp_types::Position::new(0, 25)
                    )
                );
                assert_eq!(placeholder, "SHOP.FRONT");
            }
            other => panic!("expected range+placeholder, got {other:?}"),
        }
    }

    #[test]
    fn prepare_range_of_bare_site_is_the_token() {
        match super::prepare_site(&site_at_range(false)) {
            PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
                assert_eq!(
                    range,
                    lsp_types::Range::new(
                        lsp_types::Position::new(0, 14),
                        lsp_types::Position::new(0, 26)
                    )
                );
                assert_eq!(placeholder, "SHOP.FRONT");
            }
            other => panic!("expected range+placeholder, got {other:?}"),
        }
    }
}
