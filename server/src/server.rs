//! tower-lsp server implementation for MQSC.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use lsp_types::*;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::analysis;
use crate::model::Model;

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
                    self.apply_settings(&mut diags);
                    diags
                }
                None => return,
            }
        }; // guards dropped before the await
        self.client.publish_diagnostics(uri.clone(), diags, None).await;
    }

    /// Apply `settings.diagnostics` per-code overrides: a severity string
    /// replaces the published severity, `"off"` drops the diagnostic.
    /// Accepts both `{"diagnostics": ...}` and the `{"mqsc": {...}}` section
    /// shape (see README "Configuration").
    fn apply_settings(&self, diags: &mut Vec<Diagnostic>) {
        let cfg = self.settings.read().unwrap();
        let root = match cfg.as_ref() {
            Some(v) => v,
            None => return,
        };
        let map = match root
            .get("mqsc")
            .and_then(|m| m.get("diagnostics"))
            .or_else(|| root.get("diagnostics"))
        {
            Some(m) if m.is_object() => m,
            _ => return,
        };
        diags.retain_mut(|d| {
            let code = match d.code {
                Some(NumberOrString::Number(c)) => c,
                _ => return true,
            };
            let val = match map.get(analysis::diag_name(code)).or_else(|| map.get(&code.to_string()))
            {
                Some(v) => v,
                None => return true,
            };
            match val.as_str() {
                Some(s) => match s.to_ascii_lowercase().as_str() {
                    "off" | "none" | "false" => false,
                    "error" => {
                        d.severity = Some(DiagnosticSeverity::ERROR);
                        true
                    }
                    "warning" | "warn" => {
                        d.severity = Some(DiagnosticSeverity::WARNING);
                        true
                    }
                    "information" | "info" => {
                        d.severity = Some(DiagnosticSeverity::INFORMATION);
                        true
                    }
                    "hint" => {
                        d.severity = Some(DiagnosticSeverity::HINT);
                        true
                    }
                    _ => true, // unknown value: keep default
                },
                None => true,
            }
        });
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
            for f in fixes {
                let Some(new_text) = f.as_str() else { continue };
                let mut changes = HashMap::new();
                changes.insert(
                    p.text_document.uri.clone(),
                    vec![TextEdit { range: d.range, new_text: new_text.to_string() }],
                );
                let preferred = actions.is_empty();
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
