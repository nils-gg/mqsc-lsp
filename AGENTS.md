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
  not model data. Don't "fix" it by keying off `ValueKind::Name`. The scope
  a ref parameter resolves in goes through `ref_scope`, which also applies
  the z/OS `TARGTYPE` value gate (`TARGTYPE(TOPIC)` makes a queue alias's
  `TARGET` a TOPIC reference; skipping this gate flags 2,328 valid topic
  aliases on the dmpmqcfg corpus).
- Name resolution is **(name, namespace)**, never bare name: every queue
  type shares one MQ namespace, other object classes have their own. The
  rule lives in `analysis::ObjectScope` (+ `type_namespace`); `NameSite`
  carries its scope, and definition/references/rename/hover/code-12/
  completion all join through `ObjectIndex::sites_in` /
  `ObjectScope::accepts_type` so they cannot disagree about what a name
  means. Rename refuses (does not mass-edit) when one namespace holds
  same-named definitions of two different types. `Definitions::by_obj`
  registers a name under **every** type that DEFINEs it (the flat `names`
  set must not gate it — a QLOCAL and a CHANNEL named alike both need
  listing, or code-12 and completion lose the second type).
- `workspace.rs` owns the workspace index: one `Entry { source, defs, sites }`
  per document, where `source` is `Open { text, tree: Mutex }` (editor
  document; `Tree` is Send-but-not-Sync, hence the per-tree `Mutex`) or
  `Closed` (a scanned workspace file — only `defs` + name sites are kept,
  never text/tree). `defs`+`sites` are computed once per content, so the
  T-17 cross-file merge and PR 7's scan cost the same to consumers. ALL
  cross-document merge/join loops live there — diagnostics' `external_defs`,
  hover's define map, definition/references/rename `sites`/`rename_sites`
  (joined on `SymbolKey` = name+namespace), and `definitions()` for
  workspace/symbol. PR 8 (watched reindex) extends that module, not
  `server.rs`; `disk_entry(path)` is the per-file entry builder it needs,
  `collect_scan(roots, cfg)` is the pure walk+parse the server runs via
  `tokio::task::spawn_blocking` in `initialized` (roots come from
  `workspace_folders`/`root_uri`, cached in `ws_roots` because the settings
  that gate the scan only exist at initialize). Two rules to preserve:
  **open overrides disk** (`merge_scanned` is insert-if-absent, and
  `close()` re-reads the file so an unsaved buffer cannot haunt the index),
  and **`rename_sites` is Open-only until PR 8** (a mass edit may not rest
  on content that is only as fresh as the last scan; references may show a
  closed site rename will not touch). Because the merge changes what
  already-open documents resolve, `initialized()` republishes all open
  documents when it merges. `server.rs` is protocol-only on top of it;
  handlers receive `(&str, &Tree)` via `with_doc` (which returns `None` for
  closed entries — full-service needs the buffer); never re-parse in a
  handler and never hold a lock guard across an `.await` (futures must stay
  Send). Poison-tolerant `rd/wr/mtx` accessors live with the map they guard.
  Invalidation policy: did_open/did_close refresh **all** documents
  (cross-file defines changed for everyone), did_change refreshes only
  itself — per-keystroke sibling refresh would be quadratic.
- `model/mqsc-overlay.json` is likewise `include_str!`-embedded and merged
  over the base model at load time (`Model::load`); the base JSON must stay
  byte-identical to `extract_model.py` output (`tools/check_model.py`
  guards it; overlay `add_params` REPLACE base params — remove overlay
  entries whenever the extractor itself gets fixed). Its `name_max`
  section (object type → max positional-name chars: 48, channels 1024) is
  read by `Model::name_max_len` to drive code 13 `NAME_TOO_LONG`, and its
  `mandatory` section (per DEFINE object type: rules demanding one of
  `any`, optionally value-gated by `when.param` ∈ `values` / ∉
  `unless_values`) drives code 14 `MISSING_MANDATORY` — like
  `object_ref_param`, curated heuristics, since the reference states name
  limits and "this parameter is required" prose only in text the extractor
  cannot capture. Code 14 never fires on SYSTEM.* names (hint-12
  convention) and the table is tuned so the dmpmqcfg corpus stays clean
  apart from genuinely dangling definitions. A third curated section,
  `availability` (per-parameter `platform`/`since`/`until`/`obj` rules, see
  `model::AvailRule`) drives code 15 `NOT_IN_TARGET` — but unlike 13/14 it
  is **settings-gated**: `settings::Target` (`mqsc.targetVersion` /
  `mqsc.platform`, parsed by `Target::from_settings`, decided by
  `Target::gate` since→until→platform) must be configured or the whole
  table is never consulted; the corpus must stay byte-identical with no
  target. `Model::availability_for` prefers obj-restricted rules and takes
  **canonical** object names (its `norm_syn` only uppercases — synonym
  resolution is the caller's job, pass `Command::object`). Hover/completion
  show the facts unconditionally via `doc_for` ("Availability
  (curated)").
- `settings::WorkspaceScan` (`mqsc.workspaceIndex.enabled` / `maxFiles` /
  `exclude`, same nested-or-flat lookup and per-key fallback as `Target`)
  gates the PR 7 startup scan: default on, 2000 files, `.git`/`target`/
  `node_modules`/`dist`/`build` excluded, 4 MiB per-file ceiling
  (`SCAN_MAX_FILE_BYTES`). It is read **once**, in `initialized()` — a
  `didChangeConfiguration` touching these keys changes nothing until
  restart (documented in README and in the extension's setting
  descriptions). The CLI never scans: `--check` output must stay
  byte-identical to the baseline.
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
cd server && cargo test                             # 115 unit + 3 grammar-corpus tests (no CLI needed)
cd grammars/tree-sitter-mqsc && tree-sitter test    # 30 corpus tests (works with CLI 0.27.x)
tree-sitter generate --abi 14                       # only after editing grammar.js
python3 tools/lsp_smoke_test.py                     # repo root; assertive full LSP session
python3 tools/workspace_index_smoke.py              # repo root; assertive workspace-scan session
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
- `workspace_index_smoke.py` is the PR 7 companion: it builds a throwaway
  workspace dir (closed `defs.mqsc`, a `sub/` file, a `target/` file to
  prove exclusion), runs the server with `workspaceFolders` pointing at it,
  and asserts hint-12 silence, symbol/hover/definition/references/completion
  across the closed files, the open-buffer override and the didClose
  disk fallback — then a second session with `workspaceIndex.enabled=false`
  proves the gate. Hint 12 is asserted through `TARGET(...)` only: a
  positional `ALTER QLOCAL(UNKNOWN)` name is **not** a reference and never
  triggers 12. Both sessions wait for the `workspace scan:` log line before
  reading diagnostics and take the **last** publish, because the merge
  republishes.
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
