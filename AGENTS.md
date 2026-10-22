# AGENTS.md — mqsc-lsp

LSP server for IBM MQ MQSC scripts: tree-sitter grammar (structure only) + a
command model extracted from the IBM reference (semantics). See README.md for
the full picture; this file only lists what is easy to get wrong.

## Architecture facts you can't infer from filenames

- **Grammar = syntax only** (`verb` + flat word/group list). Which word is the
  object type, a parameter, or a value is resolved against the model in
  `server/src/analysis.rs`. Validation behavior belongs in the server, not
  `grammar.js`.
- `model/mqsc-model.json` is **embedded at compile time** via `include_str!` in
  `server/src/model.rs`. Editing it requires a `cargo build` to take effect.
- `server/build.rs` compiles `grammars/tree-sitter-mqsc/src/{parser.c,scanner.c}`
  directly with `cc`. Those generated files are **committed**; after editing
  `grammar.js` you must run `tree-sitter generate --abi 14` before cargo picks
  the change up. Keep ABI 14 — the `tree-sitter` crate is pinned to 0.24.x.
- `tools/extract_model.py` regenerates the model from
  `MQSC_ScriptCommandReference.md` at the repo root. That file is IBM
  copyrighted and **gitignored** (a local copy exists in this checkout).
  Never commit the reference `.md`/`.pdf` files.

## Commands

```sh
cd server && cargo build --release                  # the only real build step
cd grammars/tree-sitter-mqsc && tree-sitter test    # 23 corpus tests (works with CLI 0.27.x)
tree-sitter generate --abi 14                       # only after editing grammar.js
python3 tools/lsp_smoke_test.py                     # run from repo root; full LSP session over stdio
```

- `lsp_smoke_test.py` defaults to the **debug** binary
  (`server/target/debug/mqsc-lsp`); pass a path as `$1` to test release.
- Fast iteration without the protocol, from `server/`:
  `echo 'ALTER QLOCAL(NEW1) MAXMSGS' | cargo run --example probe` —
  `probe` reads **stdin** and ignores extra arguments.
  `complete` and `fold` use hardcoded fixture text inside the example files.

## Toolchain quirks (load-bearing)

- Machine has **rustc 1.75, no rustup**. If a build fails with *"requires
  rustc X"* / *"edition2024"*, downgrade the offending transitive crate, e.g.
  `cargo update -p idna_adapter --precise 1.2.0` — do not upgrade rustc.
- The `=` version pins in `server/Cargo.toml` (`idna_adapter`, `hashbrown`,
  `indexmap`, `zerofrom`, `zerofrom-derive`, `tokio`) exist only to satisfy
  rustc 1.75. Don't bump them or run a broad `cargo update`.
- No cargo workspace, no npm/node toolchain for the grammar, no CI, no
  formatter/linter config. `server/` is a single standalone crate.
