//! mqsc-lsp — a language server for IBM MQ MQSC scripts.
//!
//! Based on a tree-sitter grammar (grammars/tree-sitter-mqsc) and a command
//! model extracted from the IBM "Script (MQSC) Command Reference".

use mqsc_lsp::server::MqscServer;

use std::net::IpAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() -> Result<()> {
    let mut tcp: Option<(IpAddr, u16)> = None;
    let mut check = false;
    let mut check_files: Vec<String> = Vec::new();
    let mut diag_overrides: Vec<(String, String)> = Vec::new();
    let mut settings_file: Option<String> = None;
    let mut log_file: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tcp" => {
                let spec = args.next().context("expected [host:]port after --tcp")?;
                tcp = Some(parse_listen(&spec)?);
            }
            "--check" => check = true,
            "--diag" => {
                let kv = args.next().context("expected CODE=SEVERITY after --diag")?;
                let (k, v) = kv.split_once('=').with_context(|| {
                    format!("--diag expects CODE=SEVERITY (e.g. UNDEFINED_OBJECT=off), got {kv}")
                })?;
                diag_overrides.push((k.to_string(), v.to_string()));
            }
            "--settings" => {
                settings_file = Some(args.next().context("expected JSON file after --settings")?);
            }
            "--log-file" => {
                log_file = Some(args.next().context("expected file path after --log-file")?);
            }
            "--version" | "-V" => {
                println!("mqsc-lsp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "mqsc-lsp {} — MQSC language server (tree-sitter)\n\n\
                     USAGE:\n    mqsc-lsp                       serve over stdin/stdout (LSP stdio)\n    \
                     mqsc-lsp --tcp [host:]port     serve over TCP (one session per connection)\n    \
                     mqsc-lsp --check FILE...       lint files ('-' = stdin), print\n    \
                                                   'file:line:col: severity[code]: message',\n    \
                                                   exit 1 on any error-severity finding\n\n\
                     LINT OPTIONS (with --check):\n    \
                     --diag CODE=SEVERITY           per-code override (number or name; e.g.\n    \
                                                   UNDEFINED_OBJECT=off); repeatable\n    \
                     --settings FILE.json           same overrides as a JSON settings file\n    \
                                                   ({{\"diagnostics\": {{...}}}} shape)\n\n\
                     SERVER OPTIONS:\n    \
                     --log-file FILE                append request timings, parse/analyze\n    \
                                                    durations and panics to FILE (also\n    \
                                                    $MQSC_LSP_LOG; default: no logging)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            other => {
                if check && (other == "-" || !other.starts_with('-')) {
                    check_files.push(other.to_string());
                } else {
                    eprintln!("unexpected argument: {other} (try --help)");
                    std::process::exit(2);
                }
            }
        }
    }

    if check {
        if check_files.is_empty() {
            eprintln!("mqsc-lsp --check: no input files (use '-' for stdin; try --help)");
            std::process::exit(2);
        }
        let settings = match mqsc_lsp::check::settings_from_overrides(&diag_overrides) {
            Some(v) => Some(v),
            None => match settings_file.as_deref() {
                Some(p) => {
                    match std::fs::read_to_string(p)
                        .map_err(|e| format!("cannot read settings {p}: {e}"))
                        .and_then(|t| {
                            serde_json::from_str::<serde_json::Value>(&t)
                                .map_err(|e| format!("settings {p} is not valid JSON: {e}"))
                        }) {
                        Ok(v) => Some(v),
                        Err(msg) => {
                            // A settings problem is a usage/IO problem: exit 2,
                            // never 1 ("error diagnostics found") — scripts must
                            // be able to tell the two apart (REVIEW.md minor).
                            eprintln!("mqsc-lsp: {msg}");
                            std::process::exit(2);
                        }
                    }
                }
                None => None,
            },
        };
        std::process::exit(mqsc_lsp::check::run(&check_files, settings.as_ref()));
    }

    // Server-mode observability (T-14): --log-file wins over MQSC_LSP_LOG;
    // with neither set the server stays completely silent.
    let log_target =
        log_file.or_else(|| std::env::var("MQSC_LSP_LOG").ok().filter(|s| !s.is_empty()));
    if let Some(p) = &log_target {
        mqsc_lsp::telemetry::init(std::path::Path::new(p))
            .with_context(|| format!("cannot open log file {p}"))?;
    }

    match tcp {
        None => serve_stdio().await,
        Some((host, port)) => serve_tcp(host, port).await,
    }
}

fn parse_listen(spec: &str) -> Result<(IpAddr, u16)> {
    if let Some((h, p)) = spec.split_once(':') {
        Ok((h.parse()?, p.parse()?))
    } else {
        Ok(("127.0.0.1".parse()?, spec.parse()?))
    }
}

async fn serve_stdio() -> Result<()> {
    let (service, socket) = LspService::new(MqscServer::new);
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(mqsc_lsp::telemetry::Observed::new(service))
        .await;
    Ok(())
}

async fn serve_tcp(host: IpAddr, port: u16) -> Result<()> {
    let listener = TcpListener::bind((host, port))
        .await
        .with_context(|| format!("cannot listen on {host}:{port}"))?;
    eprintln!("mqsc-lsp listening on {host}:{port}");
    loop {
        let (stream, peer) = listener.accept().await?;
        eprintln!("connection from {peer}");
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(stream);
            let (service, socket) = LspService::new(MqscServer::new);
            Server::new(read, write, socket)
                .serve(mqsc_lsp::telemetry::Observed::new(service))
                .await;
        });
    }
}
