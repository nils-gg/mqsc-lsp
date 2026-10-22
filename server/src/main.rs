//! mqsc-lsp — a language server for IBM MQ MQSC scripts.
//!
//! Based on a tree-sitter grammar (grammars/tree-sitter-mqsc) and a command
//! model extracted from the IBM "Script (MQSC) Command Reference".

use mqsc_lsp::server::MqscServer;

use std::net::IpAddr;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tower_lsp::{LspService, Server};



#[tokio::main]
async fn main() -> Result<()> {
    let mut tcp: Option<(IpAddr, u16)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tcp" => {
                let spec = args.next().context("expected [host:]port after --tcp")?;
                tcp = Some(parse_listen(&spec)?);
            }
            "--version" | "-V" => {
                println!("mqsc-lsp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "mqsc-lsp {} — MQSC language server (tree-sitter)\n\n\
                     USAGE:\n    mqsc-lsp                  serve over stdin/stdout (LSP stdio)\n    \
                     mqsc-lsp --tcp [host:]port   serve over TCP (one session per connection)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            other => {
                eprintln!("unexpected argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
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
    let (service, socket) = LspService::new(|client| MqscServer::new(client));
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
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
            let (service, socket) = LspService::new(|client| MqscServer::new(client));
            Server::new(read, write, socket).serve(service).await;
        });
    }
}
