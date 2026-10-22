//! tower-lsp server implementation for MQSC.

use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::Result;
use lsp_types::*;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::analysis;
use crate::model::Model;

pub struct MqscServer {
    pub client: Client,
    pub docs: RwLock<HashMap<Url, String>>,
}

impl MqscServer {
    pub fn new(client: Client) -> Self {
        MqscServer { client, docs: RwLock::new(HashMap::new()) }
    }

    async fn refresh(&self, uri: &Url, text: &str) {
        let tree = analysis::parse(text);
        let diags = analysis::analyze(text, &tree, Model::load(), uri);
        self.client
            .publish_diagnostics(uri.clone(), diags, None)
            .await;
    }

    fn with_doc<T>(&self, uri: &Url, f: impl FnOnce(&str) -> RpcResult<T>) -> RpcResult<T> {
        let docs = self.docs.read().unwrap();
        match docs.get(uri) {
            Some(text) => f(text),
            None => Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "document not open: {uri}"
            ))),
        }
    }
}

#[async_trait]
impl LanguageServer for MqscServer {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
        // warm the model so the first request is fast
        let _ = Model::load();
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
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: analysis::TOKEN_TYPES.iter().map(|t| lsp_types::SemanticTokenType::from(*t)).collect(),
                                token_modifiers: vec![],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
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

    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = p.text_document.text;
        self.docs.write().unwrap().insert(uri.clone(), text.clone());
        self.refresh(&uri, &text).await;
    }

    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        let text = match p.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };
        self.docs.write().unwrap().insert(uri.clone(), text.clone());
        self.refresh(&uri, &text).await;
    }

    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.docs.write().unwrap().remove(&p.text_document.uri);
    }

    async fn hover(&self, p: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = &p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            Ok(analysis::hover(text, &tree, Model::load(), pos))
        })
    }

    async fn completion(&self, p: CompletionParams) -> RpcResult<Option<CompletionResponse>> {
        let uri = &p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            let items = analysis::completions(text, &tree, Model::load(), pos);
            Ok(Some(CompletionResponse::Array(items)))
        })
    }

    async fn document_symbol(
        &self,
        p: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            Ok(Some(DocumentSymbolResponse::Nested(analysis::symbols(
                text, &tree,
            ))))
        })
    }

    async fn folding_range(&self, p: FoldingRangeParams) -> RpcResult<Option<Vec<FoldingRange>>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            Ok(Some(analysis::folding(text, &tree)))
        })
    }

    async fn document_highlight(
        &self,
        p: DocumentHighlightParams,
    ) -> RpcResult<Option<Vec<DocumentHighlight>>> {
        let uri = &p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            Ok(Some(analysis::highlights(text, &tree, pos)))
        })
    }

    async fn semantic_tokens_full(
        &self,
        p: SemanticTokensParams,
    ) -> RpcResult<Option<SemanticTokensResult>> {
        let uri = &p.text_document.uri;
        self.with_doc(uri, |text| {
            let tree = analysis::parse(text);
            Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: analysis::semantic_tokens(text, &tree),
            })))
        })
    }
}
