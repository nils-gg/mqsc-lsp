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
- Test suites: 49 unit tests, 3 grammar-corpus tests run through the
  server's own linked parser (regression guard without the tree-sitter
  CLI), assertive end-to-end LSP smoke harness, `tools/check_model.py`
  regeneration guard (T-10, T-11, T-12)
- GitHub Actions CI (fmt/clippy/test/release-build/smoke/model-guard on
  rustc 1.75) and a tag-triggered release workflow producing the linux
  x86-64 binary artifact (T-21, T-22)
- VS Code extension skeleton under `editors/vscode/extension/` with the
  settings schema for all 12 diagnostic codes (T-19; not yet tested — no
  node toolchain was available at authoring time)

### Changed
- Corpus false positives reduced 260,522 -> 18,865 (-92.8%) on the 324-file
  acceptance corpus: model fixes for railroad-chip parameter values (T-3),
  enum + integer parameter merges (T-7), overlay corrections for the
  remaining extractor gaps (T-2, T-4..T-8), grammar accepts mid-line `+`
  continuations (T-9)
- `documentSymbol` labels now read `DEFINE QLOCAL (NAME)` instead of a
  bare verb; `codeAction` `isPreferred` is per diagnostic, not global
- The whole crate is rustfmt-formatted and clippy-clean under
  `-D warnings`; CI enforces both

### Fixed
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
