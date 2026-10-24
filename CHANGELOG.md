# Changelog

All notable changes to mqsc-lsp are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the version is
tracked in `server/Cargo.toml` and `editors/vscode/extension/package.json`
and is released by pushing a `vX.Y.Z` tag (see `.github/workflows/release.yml`).

## [Unreleased]

### Added
- Cross-file object knowledge (T-17): hint 12, completion, hover,
  definition/references/rename now span all open documents; a name DEFINEd
  in a sibling script no longer triggers the undefined-object hint
- Navigation: `textDocument/definition`, `references` and `rename` for
  object names (rename keeps each site's quoting style and rewrites every
  open document); `workspace/symbol` lists all defined objects by type
- Hover on object names: which `DEFINE` created it (line, object type,
  other open document), use counts, "this is the definition"
- Model overlay (`model/mqsc-overlay.json`): curated model corrections
  applied at load time while the base model stays the byte-reproducible
  extractor output (T-2)
- `--check` CLI mode: `file:line:col: severity[code]: message`, exit 1 on
  errors, `--diag CODE=SEVERITY` / `--settings file.json` overrides (T-15)
- Optional file logging via `--log-file` / `MQSC_LSP_LOG` plus per-request
  panic catching: a panicking handler now answers `-32603` instead of
  killing the connection (T-14)
- Test suites: 53 unit tests, 3 grammar-corpus tests run through the
  server's own linked parser (regression guard without the tree-sitter
  CLI), assertive end-to-end LSP smoke harness, `tools/check_model.py`
  regeneration guard (T-10, T-11, T-12)
- GitHub Actions CI (fmt/clippy/test/release-build/smoke/model-guard/
  `--check` CLI gate with exact exit codes on rustc 1.75) and a
  tag-triggered release workflow producing the linux x86-64 binary
  artifact (T-21, T-22)
- VS Code extension skeleton under `editors/vscode/extension/` with the
  settings schema for all 12 diagnostic codes (T-19; not yet tested — no
  node toolchain was available at authoring time)

### Changed
- Corpus false positives reduced 260,522 -> 19,613 (-92.5%) on the 324-file
  acceptance corpus: model fixes for railroad-chip parameter values (T-3),
  enum + integer parameter merges (T-7), overlay corrections for the
  remaining extractor gaps (T-2, T-4..T-8), grammar accepts mid-line `+`
  continuations (T-9)
- `documentSymbol` labels now read `DEFINE QLOCAL (NAME)` instead of a
  bare verb; `codeAction` `isPreferred` is per diagnostic, not global
- The whole crate is rustfmt-formatted and clippy-clean under
  `-D warnings`; CI enforces both

### Fixed
- Analysis no longer panics when a byte offset computed as `start + 1`
  lands inside a multi-byte character. Reachable trigger: a line over
  2048 characters starting with a non-ASCII character — `probe`/`--check`
  exited 101, and in the LSP the panic unwound through the shared document
  locks and poisoned them permanently (every later request answered
  `-32603 PoisonError`). `Lines::pos` now snaps to the char boundary and
  the shared locks recover instead of poisoning (REVIEW.md C1/M1)
- Semantic token lengths never span a line break: a quoted string using a
  `+`/`-` continuation emits one token per line segment
- `--check`: bad `--settings` file (missing/invalid JSON) now exits 2
  (usage/IO) instead of 1, so scripts can tell it apart from "error
  diagnostics found"; unknown codes and bad severity values warn on stderr
  instead of being silently ignored; repeated `-` warns and reads stdin
  once
- CI's `--check` gate asserts exact exit codes (1 and 0) instead of
  "anything nonzero", so a panicking binary can no longer pass it
- VS Code extension: `vscode-languageclient@^9` is now a declared
  dependency (the VSIX was unpackageable without it), settings sync uses
  `synchronize.configurationSection` instead of a manual notification
  that could race VS Code's own forwarding, and the inert file-watcher
  subscription is gone
- Editor examples: nvim example requires 0.10 (where `vim.filetype.add`
  exists); helix example drops the bogus block-comment config and the
  unsupported path-based grammar source; tmLanguage verb/object lists
  rebuilt from the model (no more fabricated `QMODE`/`QIN`/`QINIT`,
  `QMGR` and the missing verbs/objects added, `-` continuations
  highlighted); `wordPattern` now matches digit-led words
- Tools: `corpus_measure.py` reports a `--check` producer failure (rc>=2)
  instead of measuring the truncated output, flattens tabs in `--dump`,
  and its docstring states the real exit behavior; the smoke harness
  asserts a clean server exit and no panic echoes on stderr, and removes
  its log file; `check_model.py` tolerates a nameless param in the diff;
  `extract_model.py`: `^NOT` prefix-strip no longer mangles `NORMAL`/
  `NOTICE`, a duplicate VERB+OBJ warns, dead regex removed
- Docs: AGENTS/README toolchain claims corrected (the tree-sitter CLI and
  rustup do exist; README now says so), CI step lists include the
  `--check` gate, test counts current
- Non-UTF-8 bytes in real dumps (stray characters in `DESCR` text) no
  longer abort the `probe` example or make `--check` skip the whole file:
  both lint a lossy decode (a corpus file's 748 hints had silently
  vanished from measurements because of the panic)
- Hint 12 message wording follows the cross-file view ("this script or the
  other open documents")
- Quickfix `isPreferred`, quoted-name rename/rehover spans, and the
  code-12 `arms()` gate for per-object-type dump scripts

## [0.1.0] - never released

### Added
- tree-sitter MQSC grammar (structure only; comments, continuations,
  quoting), command model extracted from the IBM Script (MQSC) Command
  Reference, tower-lsp server with diagnostics (12 codes), quickfixes,
  completion, hover, folding, semantic tokens, same-file DEFINE knowledge,
  severity settings, nvim/vscode/helix configuration examples
