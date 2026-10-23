//! tower-lsp server implementation for MQSC.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use lsp_types::*;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::analysis;
use crate::model::Model;
use crate::settings::{apply_diagnostic_settings, diagnostic_settings};

/// A document opened in the editor: its text and the parse tree built for
/// that text. All requests reuse the cached tree instead of re-parsing.
struct Doc {
    text: String,
    // tree_sitter::Tree is Send but not Sync, so it needs a lock for shared
    // handler access; handlers only ever use &Tree (analysis never edits).
    tree: Mutex<tree_sitter::Tree>,
}

pub struct MqscServer {
    pub client: Client,
    docs: RwLock<HashMap<Url, Doc>>,
    /// Client settings, see README "Configuration":
    /// `{"diagnostics": {"CODE_NAME_OR_NUMBER": "error"|"warning"|"information"|"hint"|"off"}}`
    settings: RwLock<Option<serde_json::Value>>,
    /// Whether the client resolves snippet placeholders in completions.
    snippet_support: AtomicBool,
}

impl MqscServer {
    pub fn new(client: Client) -> Self {
        MqscServer {
            client,
            docs: RwLock::new(HashMap::new()),
            settings: RwLock::new(None),
            snippet_support: AtomicBool::new(false),
        }
    }

    /// Publish diagnostics for an open document, applying severity overrides.
    /// Uses the document's cached parse tree (callers must have stored it).
    async fn refresh(&self, uri: &Url) {
        let diags = {
            let docs = self.docs.read().unwrap();
            match docs.get(uri) {
                Some(doc) => {
                    let tree = doc.tree.lock().unwrap();
                    let mut diags = analysis::analyze(&doc.text, &tree, Model::load(), uri);
                    let cfg = self.settings.read().unwrap();
                    if let Some(map) = cfg.as_ref().and_then(diagnostic_settings) {
                        apply_diagnostic_settings(&mut diags, map);
                    }
                    diags
                }
                None => return,
            }
        }; // guards dropped before the await
        self.client.publish_diagnostics(uri.clone(), diags, None).await;
    }

    fn with_doc<T>(
        &self,
        uri: &Url,
        f: impl FnOnce(&str, &tree_sitter::Tree) -> RpcResult<T>,
    ) -> RpcResult<T> {
        let docs = self.docs.read().unwrap();
        match docs.get(uri) {
            Some(doc) => {
                let tree = doc.tree.lock().unwrap();
                f(&doc.text, &tree)
            }
            None => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "document not open: {uri}"
            ))),
        }
    }
}

/// Replacement text for one rename site: quoted sites stay quoted; a name
/// that no longer fits the grammar's bare-word token syntax
/// (`[A-Za-z_#$@%/.][-A-Za-z0-9_#$@%/.]*`) must be written as a 'string'.
fn rename_text(new_name: &str, site_quoted: bool) -> String {
    let bare_ok = match new_name.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || "_#$@%/.".contains(c) => {
            new_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_#$@%/.-".contains(c))
        }
        _ => false,
    };
    if site_quoted || !bare_ok {
        format!("'{}'", new_name.replace('\'', "''"))
    } else {
        new_name.to_string()
    }
}

#[async_trait]
impl LanguageServer for MqscServer {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        // warm the model so the first request is fast
        let _ = Model::load();

        if params.initialization_options.is_some() {
            *self.settings.write().unwrap() = params.initialization_options.clone();
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
                    trigger_characters: Some(vec!["(".into(), ",".into(), ":".into()]),
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
                rename_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: analysis::TOKEN_TYPES.iter().map(|t| lsp_types::SemanticTokenType::from(*t)).collect(),
                                token_modifiers: analysis::TOKEN_MODIFIERS.iter().map(|m| lsp_types::SemanticTokenModifier::from(*m)).collect(),
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
        self.client
            .log_message(MessageType::INFO, "MQSC language server ready")
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        *self.settings.write().unwrap() =
            if params.settings.is_null() { None } else { Some(params.settings) };
        // severities may have changed: republish everything we know about
        let uris: Vec<Url> = self.docs.read().unwrap().keys().cloned().collect();
        for uri in uris {
            self.refresh(&uri).await;
        }
    }

    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = p.text_document.text;
        let tree = analysis::parse(&text);
        self.docs
            .write()
            .unwrap()
            .insert(uri.clone(), Doc { text, tree: Mutex::new(tree) });
        self.refresh(&uri).await;
    }

    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = match p.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };
        let tree = analysis::parse(&text);
        self.docs
            .write()
            .unwrap()
            .insert(uri.clone(), Doc { text, tree: Mutex::new(tree) });
        self.refresh(&uri).await;
    }

    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.docs.write().unwrap().remove(&p.text_document.uri);
    }

    async fn hover(&self, p: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = &p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        self.with_doc(uri, |text, tree| {
            Ok(analysis::hover(text, tree, Model::load(), pos))
        })
    }

    async fn completion(&self, p: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = &p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let snippet = self.snippet_support.load(Ordering::Relaxed);
        self.with_doc(uri, |text, tree| {
            let items = analysis::completions(text, tree, Model::load(), pos, snippet);
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
        for d in p.context.diagnostics.iter().filter(|d| d.source.as_deref() == Some("mqsc")) {
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
                    vec![TextEdit { range: d.range, new_text: new_text.to_string() }],
                );
                let preferred = first_for_this_diag;
                first_for_this_diag = false;
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Change to `{new_text}`"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![d.clone()]),
                    edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
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
        self.with_doc(uri, |text, tree| Ok(Some(analysis::highlights(text, tree, pos))))
    }

    /// T-16: jump from any use of an object name to its DEFINE. Duplicate
    /// names (same name under several object types exist in real dumps) all
    /// come back as locations.
    async fn goto_definition(
        &self,
        p: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let uri2 = uri.clone();
        self.with_doc(&uri, move |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            let Some(site) = idx.site_at(pos) else { return Ok(None) };
            let defs = idx.definitions_of(&site.name);
            match defs.len() {
                0 => Ok(None),
                1 => Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: uri2,
                    range: defs[0].range,
                }))),
                _ => Ok(Some(GotoDefinitionResponse::Array(
                    defs.iter()
                        .map(|d| Location { uri: uri2.clone(), range: d.range })
                        .collect(),
                ))),
            }
        })
    }

    async fn references(&self, p: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let uri2 = uri.clone();
        self.with_doc(&uri, move |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            let Some(site) = idx.site_at(pos) else { return Ok(None) };
            // definition first (clients honour includeDeclaration themselves)
            Ok(Some(
                idx.sites_of(&site.name)
                    .map(|s| Location { uri: uri2.clone(), range: s.range })
                    .collect(),
            ))
        })
    }

    /// Rename every occurrence of the object name under the cursor, keeping
    /// each site's quoting style (quoted sites stay quoted; a name that no
    /// longer fits the bare-word syntax is quoted everywhere).
    async fn rename(&self, p: RenameParams) -> RpcResult<Option<WorkspaceEdit>> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let new_name = p.new_name;
        let uri2 = uri.clone();
        if new_name.is_empty() {
            return Err(tower_lsp::jsonrpc::Error::invalid_params("new name is empty"));
        }
        self.with_doc(&uri, move |text, tree| {
            let idx = analysis::object_index(text, tree, Model::load());
            let Some(site) = idx.site_at(pos) else {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "rename needs the cursor on an object name",
                ));
            };
            let edits: Vec<TextEdit> = idx
                .sites_of(&site.name)
                .map(|s| TextEdit {
                    range: s.range,
                    new_text: rename_text(&new_name, s.quoted),
                })
                .collect();
            let mut changes = HashMap::new();
            changes.insert(uri2, edits);
            Ok(Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }))
        })
    }

    /// T-20: every object DEFINEd in every open document, grouped in the
    /// client's outline by their object type (container_name).
    /// (tower-lsp names the `workspace/symbol` handler `symbol`.)
    async fn symbol(
        &self,
        p: WorkspaceSymbolParams,
    ) -> RpcResult<Option<Vec<SymbolInformation>>> {
        let query = p.query.to_ascii_uppercase();
        let mut out = Vec::new();
        let docs = self.docs.read().unwrap();
        for (uri, doc) in docs.iter() {
            let tree = doc.tree.lock().unwrap();
            let idx = analysis::object_index(&doc.text, &tree, Model::load());
            for s in idx.definitions() {
                if !query.is_empty() && !s.name.contains(&query) {
                    continue;
                }
                out.push(SymbolInformation {
                    name: s.word.clone(),
                    kind: SymbolKind::METHOD, // matches the documentSymbol kinds
                    tags: None,
                    deprecated: None,
                    location: Location { uri: uri.clone(), range: s.range },
                    container_name: s.defines.clone(),
                });
            }
        }
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
    use super::rename_text;

    #[test]
    fn quoted_sites_stay_quoted() {
        assert_eq!(rename_text("NEW1", true), "'NEW1'");
    }

    #[test]
    fn bare_sites_follow_grammar_syntax() {
        assert_eq!(rename_text("NEW1", false), "NEW1");
        assert_eq!(rename_text("NEW NAME", false), "'NEW NAME'"); // spaces need quotes
        assert_eq!(rename_text("1BAD", false), "'1BAD'"); // conservative: quote digit start
        assert_eq!(rename_text("IT'S", false), "'IT''S'"); // MQSC doubles embedded quotes
        assert_eq!(rename_text("", true), "''"); // degenerate but never bare
    }
}
