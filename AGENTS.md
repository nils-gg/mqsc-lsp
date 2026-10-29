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
  `Closed` (a workspace file — only `defs` + name sites are kept, never
  text/tree). `defs`+`sites` are computed once per content, so the
  T-17 cross-file merge and PR 7's scan cost the same to consumers. ALL
  cross-document merge/join loops live there — diagnostics' `external_defs`,
  hover's define map, definition/references/rename `sites`/`rename_sites`
  (joined on `SymbolKey` = name+namespace), and `definitions()` for
  workspace/symbol. PR 8 (watched reindex) extended that module:
  `disk_entry(path)` is the per-file entry builder,
  `collect_scan(roots, cfg)` the pure walk+parse for startup, and
  `reindex_from_events(events, cfg, roots)` + `apply_disk_updates(updates,
  cfg)` the per-event path (filters mirror the scan: `*.mqsc` only,
  excluded dirs relative to the event's root, DELETED without reading,
  anything not CREATED/DELETED re-read; the watch types are `pub(crate)`
  because `Entry` is). The server runs both scans via
  `tokio::task::spawn_blocking` (roots cached in `ws_roots`, settings
  snapshotted once into `ws_scan` — the restart-only contract covers watch
  too). Watching is **dynamic registration only** (lsp-types 0.94 has no
  server-side static capability and LSP says none exists): `initialize`
  reads the client's `didChangeWatchedFiles.dynamicRegistration` into
  `dyn_watch`, and `initialized` asks for `**/*.mqsc` AFTER the scan — a
  refused/absent watcher costs freshness, never the index. Three rules to
  preserve: **open overrides disk** (`merge_scanned` is insert-if-absent,
  `close()` re-reads the file so an unsaved buffer cannot haunt the index,
  and `apply_disk_updates` never touches Open entries — a save fires a
  watched event for the very buffer); **cap refuses growth only** (a
  `max_files` overflow keeps reindexing already-indexed files); and
  **`rename_sites` covers every entry** now that closed files are watched
  (it lost its open_only filter — do not reintroduce one, references and
  rename must keep returning the identical join). Because a merge or a
  watch apply changes what already-open documents resolve, both
  `initialized()` and `did_change_watched_files` republish all open
  documents when anything changed. `server.rs` is protocol-only on top of
  it; handlers receive `(&str, &Tree)` via `with_doc` (which returns `None`
  for closed entries — full-service needs the buffer); never re-parse in a
  handler and never hold a lock guard across an `.await` (futures must stay
  Send; helper fns go in the inherent `impl MqscServer`, not the trait
  impl). Poison-tolerant `rd/wr/mtx` accessors live with the map they
  guard. Invalidation policy: did_open/did_close and a watch apply refresh
  **all** documents (cross-file defines changed for everyone), did_change
  refreshes only itself — per-keystroke sibling refresh would be quadratic.
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
cd server && cargo test                             # 120 unit + 3 grammar-corpus tests (no CLI needed)
cd grammars/tree-sitter-mqsc && tree-sitter test    # 30 corpus tests (works with CLI 0.27.x)
tree-sitter generate --abi 14                       # only after editing grammar.js
python3 tools/lsp_smoke_test.py                     # repo root; assertive full LSP session
python3 tools/workspace_index_smoke.py              # repo root; assertive workspace scan+watch session
python3 tools/check_model.py                        # model byte-reproducibility guard
python3 tools/check_versions.py [vX.Y.Z]            # Cargo.toml = package.json (= tag) guard
node tools/extension_resolve_test.js                # extension server-lookup behavior (stubs)
```

- `lsp_smoke_test.py` is **assertive**: it checks quickfixes, script-defined
  completion names, settings-driven severity filtering, navigation
  (definition/references/rename), cross-file behavior (open/close a sibling
  document and watch hint 12 come and go), and the `MQSC_LSP_LOG` file it
  starts the server with — and exits non-zero on FAIL. Defaults to the debug
  binary (`server/target/debug/mqsc-lsp`); pass a binary path as `$1` to
  test release. It runs **whatever binary is on disk** — rebuild first, or a
  stale `target/` silently tests old code.
- `workspace_index_smoke.py` is the workspace-index companion (PR 7 + PR 8):
  it builds a throwaway workspace dir (closed `defs.mqsc`, a `sub/` file, a
  `target/` file to prove exclusion), runs the server with
  `workspaceFolders` pointing at it, and asserts hint-12 silence,
  symbol/hover/definition/references/completion across the closed files,
  the open-buffer override and the didClose disk fallback, the watcher
  registration (`**/*.mqsc` glob) and simulated create/change/delete
  `didChangeWatchedFiles` events reindexing single files (hint 12 goes and
  comes without any reopen), that a save event for an OPEN buffer causes
  no republish, and that rename now edits the closed file too. A second
  session with `workspaceIndex.enabled=false` proves the gate — including
  that no watcher is registered then, even though both sessions advertise
  `didChangeWatchedFiles.dynamicRegistration`. The client MUST answer
  server→client requests: the framer auto-responds to any message with
  both `id` and `method` (the server awaits `client/registerCapability`
  inside `initialized`). Hint 12 is asserted through `TARGET(...)` only: a
  positional `ALTER QLOCAL(UNKNOWN)` name is **not** a reference and never
  triggers 12. Both sessions wait for the `workspace scan:` log line before
  reading diagnostics and take the **last** publish, because the merge
  republishes. `lsp_smoke_test.py` needs no such answering and stays
  unchanged precisely because it initializes with `capabilities: {}` — no
  dynamic registration, so no `registerCapability` request ever arrives
  (the "client without dynamic registration" path).
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
  config beyond rustfmt defaults. `server/` is a single standalone crate.
  CI **does** exist and now runs a matrix (`.github/workflows/ci.yml`):
  the FULL gate set (fmt/clippy `-D warnings`/test/release build/both
  smokes/model+version guards/extension resolver test/`--check` CLI gate
  with exact exit codes) on rust 1.75, plus `cargo test --locked` on
  ubuntu-stable, windows-latest and macos-latest — keep every gate green;
  fmt/clippy are deliberately MSRV-only (newer clippies add lints we did
  not choose). rustfmt and clippy components are installed locally too.
- **PR 10 release/distribution facts** (all enforced, none inferable from
  the code): `release.yml` builds five MSRV-pinned binaries on their own
  runners — linux x86_64, linux aarch64 (`ubuntu-24.04-arm`), windows msvc,
  macos aarch64 + x86_64 (cross on the arm `macos-14` runner) — packages
  the VSIX (`npm ci` + vsce, `package-lock.json` is committed) and
  publishes one Release with a `SHA256SUMS`; the tag is the version gate:
  `tools/check_versions.py` fails if tag / Cargo.toml / extension
  package.json diverge, so a version bump touches all three plus
  CHANGELOG in one commit. The extension finds its server in
  `client/extension.js` `resolveServer` (explicit path honored exactly or
  failing loudly with guidance → PATH → fixed candidate list → release-
  page guidance); that candidate list is documented in three places
  (setting description, extension README, resolver error message) and
  pinned by `tools/extension_resolve_test.js` — change it in all four.
  Marketplace publishing stays deferred (publisher id is set; the VSIX on
  the Release is the distribution channel for now).
- **Windows portability rule for tests**: `Url::from_file_path("/tmp/x")`
  panics on Windows (drive-less path). Index-only documents in
  `workspace.rs` tests go through the `key_uri`/`open_text` helpers (which
  parse `file:///…` URLs), anything standing for a real file goes through
  `open_disk` with a `temp_ws` path. The CI windows job is the only
  enforcer — a literal `/tmp` string reappearing in a test turns it red.
