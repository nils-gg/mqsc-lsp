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
- **Quickfix flow spans files**: `Diags::push_fixes` (analysis.rs) attaches
  `{"quickfixes":[...]}` to `Diagnostic.data`; `code_action` (server.rs) only
  replays what the client echoes back. A diagnostic with suggestions must go
  through `push_fixes` to get actions — `push` yields none.
- The model's `name` value kind is **not** "object reference" — `TARGET`,
  `XMITQ`, `LIKE` etc. are typed plain `string`. The real object-reference
  parameters (driving hint 12 and defined-name completion) live in the
  curated `object_ref_param` table in analysis.rs — a documented heuristic,
  not model data. Don't "fix" it by keying off `ValueKind::Name`.
- `server.rs` caches one `Doc { text, tree, defs }` per open document
  (`Tree` is Send-but-not-Sync, hence the per-tree `Mutex`; `defs` is the
  doc's own DEFINE index, making the T-17 cross-file view a cheap merge).
  Handlers receive `(&str, &Tree)` via `with_doc`; never re-parse in a
  handler and never hold a lock guard across an `.await` (futures must stay
  Send). Invalidation policy: did_open/did_close refresh **all** documents
  (cross-file defines changed for everyone), did_change refreshes only
  itself — per-keystroke sibling refresh would be quadratic.
- `model/mqsc-overlay.json` is likewise `include_str!`-embedded and merged
  over the base model at load time (`Model::load`); the base JSON must stay
  byte-identical to `extract_model.py` output (`tools/check_model.py`
  guards it; overlay `add_params` REPLACE base params — remove overlay
  entries whenever the extractor itself gets fixed). Its `name_max`
  section (object type → max positional-name chars: 48, channels 1024) is
  read by `Model::name_max_len` to drive code 13 `NAME_TOO_LONG` — like
  `object_ref_param`, a curated heuristic, since the reference states name
  limits only in prose the extractor cannot capture.
- `telemetry.rs` is silent unless `--log-file`/`MQSC_LSP_LOG` is set; in
  stdio mode nothing but LSP traffic may ever reach stdout. The `Observed`
  tower wrapper catches handler panics into `-32603` responses — handlers
  must not add their own `catch_unwind`.
- `tools/extract_model.py` regenerates the model from
  `MQSC_ScriptCommandReference.md` at the repo root. That file (plus the
  `-rag.md` twin and `.pdf`) is IBM copyrighted and **gitignored**; a local
  copy exists in this checkout. Never commit it.

## Commands

```sh
cd server && cargo build --release                  # the only real build step
cd server && cargo test                             # 68 unit + 3 grammar-corpus tests (no CLI needed)
cd grammars/tree-sitter-mqsc && tree-sitter test    # 30 corpus tests (works with CLI 0.27.x)
tree-sitter generate --abi 14                       # only after editing grammar.js
python3 tools/lsp_smoke_test.py                     # repo root; assertive full LSP session
python3 tools/check_model.py                        # model byte-reproducibility guard
```

- `lsp_smoke_test.py` is **assertive**: it checks quickfixes, script-defined
  completion names, settings-driven severity filtering, navigation
  (definition/references/rename), cross-file behavior (open/close a sibling
  document and watch hint 12 come and go), and the `MQSC_LSP_LOG` file it
  starts the server with — and exits non-zero on FAIL. Defaults to the debug
  binary (`server/target/debug/mqsc-lsp`); pass a binary path as `$1` to
  test release. It runs **whatever binary is on disk** — rebuild first, or a
  stale `target/` silently tests old code.
- The **tree-sitter CLI 0.27.1 is installed** (`~/.local/bin/tree-sitter`;
  node/npm exist under `/usr/bin`), so `tree-sitter test` works. But never
  run a bare `tree-sitter generate` here: without `--abi 14` it rewrites the
  committed `parser.c` to a newer ABI. Server-side work never needs the CLI
  at all: cargo compiles the committed `parser.c`/`scanner.c` directly.
- Fast iteration without the protocol, from `server/`:
  `echo 'ALTER QLOCAL(NEW1) MAXMSGS' | cargo run --example probe` —
  `probe` reads **stdin** and ignores extra arguments.
  `complete` and `fold` use hardcoded fixture text inside the example files.
- Real-world regression corpus (sibling checkout): `/home/nige/Code/esql-lsp/
  dmpmqcfg/*.mqsc`. `probe` must show no panics and stable diagnostic counts.

## Model regeneration quirks

- The extractor is deterministic. A healthy run prints
  `params: 1336 (undocumented: 64), docs: 690` with 128 commands. A large
  jump in either number is a parser regression, not progress.
- The 64 remaining undocumented params (e.g. `LIKE`, z/OS `TOQLOCAL`) have
  genuinely missing text in the PDF→markdown conversion — the source has
  bare `## NAME` headings with no body. Don't chase them.
- Page-footer debris (`Parent topic:`, `Page N of`, `---`, `<!-- Page -->`)
  appears **inside** doc regions and the content continues after it. Skip it
  (the junk filter does); never `break` on "Parent topic" — that truncates
  every parameter-description section at its first page boundary.

## Toolchain quirks (load-bearing)

- **MSRV is rustc 1.75.** If a build fails with *"requires rustc X"* /
  *"edition2024"*, downgrade the offending transitive crate, e.g.
  `cargo update -p idna_adapter --precise 1.2.0` — never upgrade rustc.
  The `=` version pins in `server/Cargo.toml` (`idna_adapter`, `hashbrown`,
  `indexmap`, `zerofrom`, `zerofrom-derive`) exist only for rustc 1.75;
  don't bump them or run a broad `cargo update`.
- Rust is installed **per-user via rustup** (`~/.cargo/bin` is on PATH via
  `~/.profile`): rustc/cargo **1.75.0**, the repo's MSRV. Keep the default
  toolchain at 1.75 — if a build hits pin-related errors, apply the
  downgrade recipe above instead of installing a newer toolchain.
- No cargo workspace, no npm package scaffolding for the grammar (the
  tree-sitter CLI is installed directly, see above), no formatter
  config beyond rustfmt defaults, no extension build tooling. `server/` is a
  single standalone crate. CI **does** exist now (`.github/workflows/ci.yml`,
  rust 1.75 pinned: fmt/clippy `-D warnings`/test/release build/smoke/
  model-guard/`--check` CLI gate with exact exit codes) — keep its gates
  green; rustfmt and clippy components are installed locally too.
