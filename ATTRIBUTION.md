# Attribution

## IBM MQSC reference material

The semantic model that powers this project's diagnostics, hover text and
completion (`model/mqsc-model.json`) was extracted — by
[`tools/extract_model.py`](tools/extract_model.py) — from the IBM
publication *"Script (MQSC) Command Reference"* (a PDF conversion dated 2011
describing the MQSC commands of the WebSphere MQ 7.x era).

- That document is **© International Business Machines Corporation** and is
  **not redistributed in this repository**. The source PDF and its markdown
  conversions are deliberately excluded via `.gitignore`. To regenerate the
  model, obtain your own copy of the reference and save the markdown
  conversion as `MQSC_ScriptCommandReference.md` in the repository root, then
  run `python3 tools/extract_model.py`.
- `model/mqsc-model.json` embeds short descriptive passages taken from that
  reference, reproduced here to keep the tool's hover/completion text accurate.
  If you would prefer a build with no IBM-derived prose, delete
  `model/mqsc-model.json` and run the extractor against your own copy — or
  replace the extracted description fields with your own text; the server
  degrades gracefully (hover simply omits the description lines).
- This project is an independent implementation. It is **not sponsored,
  endorsed, certified or otherwise approved by IBM**.

**IBM, WebSphere and MQ are trademarks of International Business Machines
Corporation.** Other names may be trademarks of their respective owners.
Mention of MQSC commands or values in this repository does not imply any
affiliation with or endorsement by IBM.

## Third-party software

The server is built on open-source libraries (their licenses apply to those
components, which remain the copyright of their respective authors):

| Component | License |
| --- | --- |
| [tree-sitter](https://tree-sitter.github.io/) (Rust crate + CLI) and `tree-sitter-language` | MIT |
| [tower-lsp](https://github.com/eyalkalderon/tower-lsp) | MIT |
| [lsp-types](https://github.com/gluon-lang/lsp-types) | MIT |
| [tokio](https://tokio.rs/) | MIT |
| [serde](https://serde.rs/), serde_json | MIT or Apache-2.0 |
| [anyhow](https://github.com/dtolnay/anyhow) | MIT or Apache-2.0 |
| [cc](https://github.com/rust-lang/cc-rs) | MIT OR Apache-2.0 |

See `server/Cargo.lock` for the exact set of transitive dependencies and
their licenses.

## Everything else

The grammar (`grammars/tree-sitter-mqsc/`), server source, extractor, test
tooling and documentation in this repository are licensed under the
[MIT License](LICENSE) (c) 2026 Nils Gerstner — except where explicitly noted
above.
