# Changelog

All notable changes to mqsc-lsp are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the version is
tracked in `server/Cargo.toml` and `editors/vscode/extension/package.json`
and is released by pushing a `vX.Y.Z` tag (see `.github/workflows/release.yml`).

## [Unreleased]

### Changed
- **Definition and rename now default to the edited file.** Go-to-definition
  jumps only to `DEFINE`s inside the document under the cursor, and Rename
  Symbol rewrites only that document — a rename can no longer mass-edit
  workspace files you never opened. The new
  `mqsc.crossFileNavigation` setting (boolean, default `false`, read live
  per request) restores the previous workspace-wide behavior for both.
  References, hover, completion and the undefined-object hint (12) are
  unchanged: they still see every open and indexed workspace file — they
  answer "what exists in this project", not "what should this edit touch".
  The rename safety gate (a name `DEFINE`d under two types refuses) now
  judges exactly the sites in scope: the current document by default, the
  whole index with the setting on.

### Fixed
- Fabricated parameter values removed: the 2011 reference renders a
  DISPLAY page's displayable attributes as a dump of bare `[ WORD ]`
  selections, and the extractor read each run of that dump as the enum of
  whatever parameter preceded it — so `DISPLAY QMGR SSLEV` "accepted"
  SSLFIPS, SSLKEYR, … (the next attributes on the page), 53 parameters
  across DISPLAY QMGR/CONN/CHSTATUS/QSTATUS/CHANNEL in total. Every one of
  these words is a bare attribute selector in the live MQ 10.0.1
  railroads, so a new `flag_params` overlay operation demotes them to
  kind=flag (bare word, no value group — `DISPLAY QMGR SSLEV(DISABLED)`
  now reports the unexpected-group error real MQ raises) while keeping
  the genuine descriptions for hover. Genuine enums on the same pages
  survive untouched — QSGDISP, TRPTYPE, PUTAUT and the ALTER QMGR value
  lists were checked against the live queue manager (SSLEV(DISABLED),
  SCHINIT(QMGR), IPADDRV(IPV4), SUITEB(NONE) are all real MP 10.0.1
  spellings). Two new guards: `no_parameter_enumerates_its_own_siblings`
  pins the artifact fingerprint (≥6-value enums may not enumerate their
  own command's parameters), and `pooled_attribute_words_load_as_flags`
  pins the demotions plus the surviving genuine ALTER QMGR enums.
  Completion now offers these words as bare keywords instead of `WORD(`
  groups with invented value snippets. Diagnostics are untouched —
  corpus probe byte-identical at 2653.
- Hover texts no longer truncate. `DEFINE QLOCAL MAXDEPTH` showed "…but
  your" and similar cards ended mid-word ("…does not limit the lis").
  Three extractor causes, fixed at the source: the 2011 print flow wraps
  footnote prose across blank lines and the collector stopped at the blank
  (49 of 316 footnotes dangled; they now join across blanks-only gaps
  while the sentence is unfinished, 7 source-page-break residue stays);
  the long-description cap was a hard 500-char cut (now 1000 chars,
  rewound to a sentence end); and the pages' platform-support tables
  printed INTO the prose flow — including fake `## CR` headings that stole
  the next paragraph — polluting descriptions with
  "i5/OS® UNIX systems Windows z/OS® n m l k j" debris (now culled; the
  genuine two-letter filter-operator docs `## GE`/`## LK`/`## CT` survive,
  told apart by what follows them). 212 parameter descriptions recovered
  lost sentences. Three guards pin it:
  `hover_text_carries_no_platform_table_residue`,
  `clamped_descriptions_end_at_a_sentence`,
  `wrapped_footnote_continuations_are_joined`. Diagnostics are untouched
  — corpus probe byte-identical at 2653.

### Added
- Hover now has full documentation coverage: every parameter and every
  displayable attribute in the merged model carries a description. Two new
  curated overlay sections make that possible — `param_docs` adds hover
  text for the 96 names the 2011 reference leaves undocumented (official
  reference prose where the extraction missed it, oracle/DDG-confirmed
  wording for modern parameters like `IMGSCHED`/`ACTVCONO`/`AMQPKA`/
  `IGNSTATE`, and an honest "the IBM reference carries no explanation"
  note for the genuinely unexplainable legacy residue), and
  `generic_values` explains the value words shared across many parameters
  (`DISABLED` → "the feature this parameter controls is turned off") —
  used only inside enum-valued parameters, so a word in a string group is
  never treated as vocabulary. The hover value chain is now: scoped
  `value_docs` → generic vocabulary → a plain integer note for numeric
  parameters, and 17 more scoped `value_docs` rules cover the statistics
  levels (MONQ/MONCHL/MONACLS/STATACLS/STATCHL), LDAP AUTHINFO
  (CHCKLOCL/ADOPTCTX/AUTHORMD), SET POLICY, SET AUTHREC, QSGDISP, USERSRC
  and PROPCTL spellings the model actually carries. Display attributes
  normalize to bare words at model load (`PORT ( port-number )` → `PORT`),
  finally matching hover and completion. Four guard tests pin it all:
  parameter and attribute coverage, generic-word existence in the model,
  and the bare-word attr form. Diagnostics are untouched — corpus probe
  byte-identical at 2653.

### Added
- Hovering a parameter VALUE explains it: a fifth curated overlay
  section, `value_docs` (`param` + object-scoped `{VALUE: meaning}`),
  appends the value's descriptive name to the parameter card —
  `CCSID(1208)` says "UTF-8 (Unicode)", `CHLTYPE(SDR)` "sender channel",
  `TYPE(BLOCKUSER)` the CHLAUTH record meaning, and so on: 20 rules /
  83 values across CCSID (incl. 65535 = automatic), CHLTYPE, TRPTYPE,
  DEFBIND, DEFTYPE, TYPE (CHLAUTH/QUEUE/TOPIC/SECURITY), ACTION
  (CHLAUTH/POLICY/COMMIT-BACKOUT), CHCKCLNT, AUTHTYPE, CLROUTE and the
  topic ASPARENT family. Object scoping separates same-spelled values;
  an anti-fabrication guard pins every curated value to a value the
  model actually accepts. Hovering a value word now correctly shows the
  OWNING parameter's card (value words used to be misread as parameter
  names and produced nothing). Informational only — no diagnostics.
- Command-level availability (diagnostic 15 grows up): a fourth curated
  overlay section, `command_availability`, flags whole `VERB OBJECT`
  commands that the configured `mqsc.targetVersion`/`mqsc.platform` does
  not have — the CFSTRUCT/BUFFPOOL/PSID/STGCLASS/TRACE/CHINIT/CMDSERV/…
  z/OS families, `MOVE QLOCAL`, `START`/`STOP QMGR`, and the
  `since: 9.0` floor of the CHLAUTH family. The 52 rules are the live
  MQ 10.0.1 oracle's own verdict (`tools/mq_oracle_check.py`: the pairs
  distributed runmqsc's command lists lack are z/OS-only), with the same
  settings gate as the parameter rules — with no target configured
  nothing is ever consulted (the corpus stays byte-identical). Hover's
  command card shows the curated facts regardless of target, and a guard
  test insists every rule targets a command the model can resolve.
- The model is now oracle-verified against a live IBM MQ 10.0.1
  installation (campaign B1–B7, see README): the overlay gained the 11
  commands 10.0.1 accepts but the 2011 reference missed (`DISPLAY
  TCLUSTER`/`APSTATUS`/`AUTHREC`/`AUTHSERV`/`ENTAUTH`, `PURGE CHANNEL`,
  `SET`/`DELETE`/`DISPLAY POLICY`, `DELETE COMMINFO`/`AUTHREC`) plus the
  `SERVCOMP` and `ENFORCE`/`TOLERATE` parameters, corrected
  false-flagging enums and value kinds (`DEFPRESP`, the MSGDATA-family
  `Enum(["* ,"])`, `CLUSNL`/`CLUSTER` as name strings, oracle values for
  `DEFBIND`/`COMPMSG`/`SSLCAUTH`/…), and lost entries the parser
  disproves (fabricated `SET CHLAUTH ACTION(DELETE)`, TOPIC
  `ASPARENT`/`NOLIMIT` phantoms, `ALTER SUB` PSCLR* family, …). The
  server's static `VERBS` list also gained `PURGE` and `MODIFY` — it
  silently dropped commands under unknown verbs, which had killed the
  overlay's own `MODIFY CHLAUTH`; loading now warns and a test pins the
  contract.
- Release engineering (Phase 5 / PR 10): the Release workflow now builds
  the server on every shipped platform with the pinned MSRV toolchain —
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-pc-windows-msvc` (zip), `x86_64-apple-darwin` and
  `aarch64-apple-darwin` — smoke-tests each native artifact it can run,
  packages the VS Code extension (`npm ci` + vsce, committed
  `package-lock.json`) and publishes everything as one GitHub Release
  with a single `SHA256SUMS`; the first job is a version gate
  (`tools/check_versions.py`) failing the release unless the tag,
  `server/Cargo.toml` and the extension `package.json` agree, and CI runs
  the same agreement check on every push. `rust-version = "1.75"` is now
  declared in `Cargo.toml`
- The VS Code extension finds its server instead of dying silently:
  `mqsc.server.path` (default `mqsc-lsp` = auto) honors an explicit
  command/path exactly — a wrong one fails loudly with a reason — auto
  mode searches `PATH`, then the common install directories
  (`~/.local/bin`, `/usr/local/bin`, `/opt/homebrew/bin`,
  `%LOCALAPPDATA%\Programs\mqsc-lsp`), and a miss shows a warning that
  links to the releases page or the setting instead of launching a
  phantom command; changing the path mid-session now rebuilds the client
  (the old code restarted the old baked-in command). No bundling, no
  downloading — deliberate (VSIX size); `publisher` and `repository` are
  real, so the packaged VSIX is marketplace-ready later
  (`tools/extension_resolve_test.js` pins all five lookup scenarios
  against a stubbed VS Code host, no dependencies)

- Watched-file reindexing of the workspace index: after the startup scan the
  server asks the client (via `client/registerCapability`) to report changes
  to `**/*.mqsc`, and each `workspace/didChangeWatchedFiles` event reindexes
  exactly the file it names — a new file's `DEFINE` silences hint 12 in every
  open document without a reopen, a changed file's names are replaced, a
  deleted file's names stop resolving. Events for excluded directories and
  non-`*.mqsc` paths are filtered out, `maxFiles` still refuses growth only,
  and an event for an open document never overwrites the editor's buffer
  (saving a file fires one, but `didChange` already delivered that content).
  File watching has no static server-side configuration in LSP, so a client
  without dynamic registration for `workspace/didChangeWatchedFiles` simply
  keeps closed files as scanned until the server restarts (logged, not fatal)
  (`tools/workspace_index_smoke.py` now answers the registration request and
  drives created/changed/deleted/save-of-open-buffer events)
- Workspace indexing of closed `*.mqsc` files: after `initialize` the
  server walks the workspace roots (once, on a background thread) and
  remembers every `DEFINE` and name site of each file it finds — without
  retaining any text or parse tree — so the file you have open resolves
  names against the scripts you do not have open. Hint 12 goes quiet for a
  name a workspace file defines (with the "in this script or another file"
  wording), hover names the defining file, goto-definition jumps into it,
  references list its sites, completion offers its names and
  workspace/symbol lists its objects. Configured by
  `mqsc.workspaceIndex.enabled` (default on), `mqsc.workspaceIndex.maxFiles`
  (default 2000, taken in sorted order; the server warns when files had to
  be left out) and `mqsc.workspaceIndex.exclude` (directory names skipped
  at any depth; `.git`, `target`, `node_modules`, `dist`, `build` by
  default); files over 4 MiB are skipped and the whole scan outcome is
  reported in the log. An open document always overrides its on-disk entry,
  and closing one falls back to the file on disk rather than the unsaved
  buffer (`tools/workspace_index_smoke.py` covers both sessions, on and off)

### Changed
- The CCSID hover text now names the most-used codes: `ALTER QMGR`
  CCSID (appended to the reference description) and the curated
  `DEFINE`/`ALTER COMMINFO` CCSID explain 1208 = UTF-8, 819 =
  ISO 8859-1/Latin-1, 437 = US English, 1252 = Windows Latin-1, 850 =
  Multilingual Latin-1, 37 = EBCDIC US (z/OS host), 943 = Japanese and
  1386 = Simplified Chinese (GBK).
- CI is a matrix now: the full gate set (fmt, clippy `-D warnings`, both
  smoke sessions, model/version guards, extension resolver test, exact
  `--check` exit codes) stays on the pinned 1.75, and `cargo test
  --locked` additionally runs on ubuntu-stable, windows-latest (msvc)
  and macos-latest — MSRV-rot and platform regressions surface on push.
  Test URIs no longer hardcode `/tmp/...` paths (`Url::from_file_path`
  panics on drive-less paths under Windows); index-only documents use
  platform-neutral `file:///` keys and disk-backed ones real temp paths
- Rename now rewrites every site of a symbol across the whole workspace,
  closed files included. It used to stop at open documents because a scanned
  entry was only as fresh as the last scan; with the watcher above keeping
  closed entries current, a mass edit may rest on them, and `references` can
  no longer show a site rename will not touch
- Internal (no behavior change): all cross-document knowledge moved from
  `server.rs` into a new `workspace::WorkspaceIndex` module — the open
  document map, the poison-tolerant lock accessors, and every merge/join
  loop (diagnostics' sibling DEFINE merge, the cross-file hover map,
  definition/references/rename site joins, workspace/symbol enumeration)
  now share one API (`external_defs`, `definitions`, `sites`,
  `rename_sites`) that joins exclusively on `(name, namespace)`
  `SymbolKey`s. This is the centralization step of workspace indexing:
  closed on-disk `*.mqsc` files (with open documents always overriding
  their on-disk entry) and watched reindexing will extend this module.
  Verified behavior-free: dmpmqcfg corpus byte-identical, smoke suite
  unchanged, six new index unit tests cover the lifecycle, the
  namespace-split join, quoting-carrying rename sites and replacement
  semantics

### Added
- MQ version/platform targeting (diagnostic 15 `NOT_IN_TARGET`): the new
  settings `mqsc.targetVersion` (`"9"`, `"9.4"`, patch levels read as
  their minor release) and `mqsc.platform` (`"distributed"` / `"zos"`)
  declare what the scripts target; a parameter whose curated availability
  (`availability` section of `model/mqsc-overlay.json`, like `name_max`
  and `mandatory`) contradicts the target — introduced after
  `targetVersion`, removed in it, or belonging to the other platform —
  is flagged where it is used ("BACKOUT is available from MQ 9.1…",
  "BOQNAME is only available on z/OS…"). With no target configured the
  check is entirely dormant (corpus output byte-identical); hover and
  completion annotate the curated facts unconditionally. A curated
  parameter the 2011-based model does not know stops being an unknown
  parameter (code 4) once the configured target provides it. The `--check`
  CLI honors the target through `--settings`

### Fixed
- `textDocument/references` now honors the client's
  `context.includeDeclaration` flag: with `false` the DEFINE sites stay
  out of the result (previously every site was always returned on the
  assumption that clients filter it themselves — they don't, they trust
  the server), so "Find all references" from a declaration-only object
  correctly reports nothing and a use site lists exactly the uses
- Hint 12's existence test is namespace-qualified with the same
  `ObjectScope` rule navigation resolves on: a `TARGET` looking for a
  queue is no longer satisfied by a same-named CHANNEL, and a `LIKE` on a
  CHANNEL is no longer satisfied by a same-named QLOCAL. The z/OS
  `TARGTYPE` value gate is honored both ways — a `TARGTYPE(TOPIC)` alias
  points its `TARGET` at the TOPIC namespace (topic-publishing dumps stay
  clean), while an alias without the gate pointing at a topic-only name is
  now flagged as the dangling queue reference it is (17 genuine
  mis-classified targets surfaced on the dmpmqcfg corpus; zero hints
  disappeared). Completion's offered-name pool and the quickfix
  suggestions follow the same scope. A name `DEFINE`d under several object
  types is now registered under every type it was defined as (the flat
  name set used to hide all but the first)
- Object-name resolution is now (name, namespace)-aware everywhere: a
  QLOCAL and a same-named CHANNEL are two objects, and completion,
  definition, references, hover and rename all resolve through one
  shared rule (queue types share MQ's one queue namespace; every other
  class has its own). Before, navigation conflated spellings —
  goto-definition from `TARGET(FOO)` could land on `DEFINE CHANNEL(FOO)`,
  and a rename of the queue rewrote the channel's sites too. A rename
  whose name resolves ambiguously inside one namespace (same spelling
  DEFINEd under two object types) now refuses with an explicit error
  instead of editing every matching spelling
- `semanticTokens/range` responses are document-relative like full
  responses (the deltas the client accumulates start at the document
  origin, not at the requested range start), and the filter returns every
  token that OVERLAPS the range instead of only fully-contained ones —
  a token crossing the range border is the client's to clip. Before, a
  range starting anywhere other than exactly on the first token shifted
  or dropped tokens, so VS Code highlighted the wrong spans after a
  ranged refresh. `tokens/range` and `tokens/full` now report each token
  at the same absolute position (unit-tested both ways)

### Added
- Mandatory-property checking (diagnostic code 14 `MISSING_MANDATORY`,
  warning): a `DEFINE` whose object type has a required parameter the
  reference marks mandatory, but the railroad diagrams bracket, is now
  flagged — `TARGET` on QALIAS, `RNAME`/`RQMNAME` on QREMOTE, `CHLTYPE`
  (plus `XMITQ` for sender/server channels) on CHANNEL, `TOPICSTR` on
  TOPIC, the topic source and `DEST` on SUB, `BUFFERS` on BUFFPOOL. The
  rules live in a curated `mandatory` section of `model/mqsc-overlay.json`
  with value-gated `when`/`unless_values` clauses; `LIKE` counts as
  satisfying everywhere a template copies the attribute, `SYSTEM.*`
  definitions are never flagged (the hint-12 convention), and the
  documented intentional-blank forms (cluster alias qremotes, generic
  qremotes with `XMITQ(' ')`) are exempt. Tuned against the 324-file
  z/OS dmpmqcfg corpus: the only hits left are 21 live dangling
  `DEFINE QALIAS` objects with no `TARGET` at all; the reference's
  CONNAME "requirement" was falsified there (1280 accepted definitions
  without it) and is deliberately not enforced. Re-severitisable like
  every other code (`"MISSING_MANDATORY"` / `"14"`)
- Recorded feature demo: `docs/demo.cast` (published at
  https://asciinema.org/a/Yy8RsmzpxK5fRazY), a ~40 s tour through real
  Neovim driven against the real server — inline diagnostics, a quickfix
  code action, defined-name completion, definition hover, rename across
  references, quickfix uses, folds, and the dangling-`-`-at-EOF handling.
  `tools/record_demo.sh` re-records it; every step asserts the tmux pane
  (or the saved file) before advancing, so a plain run doubles as an
  end-to-end UI test
- Name-length checking (diagnostic code 13 `NAME_TOO_LONG`, warning):
  DEFINE/ALTER/DELETE object names are measured (unquoted, unescaped,
  pattern names exempt) against per-type caps — 48 characters for
  queues/processes/topics/listeners/…, 1024 for channels. The caps live in
  a curated `name_max` section of `model/mqsc-overlay.json` because the
  2011 reference records them only in prose the extractor cannot capture;
  object types without a curated cap are never flagged. Re-severitisable
  like every other code (`"NAME_TOO_LONG"` / `"13"`)
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
- Test suites: 56 unit tests, 3 grammar-corpus tests run through the
  server's own linked parser (regression guard without the tree-sitter
  CLI), assertive end-to-end LSP smoke harness, `tools/check_model.py`
  regeneration guard (T-10, T-11, T-12)
- GitHub Actions CI (fmt/clippy/test/release-build/smoke/model-guard/
  `--check` CLI gate with exact exit codes on rustc 1.75) and a
  tag-triggered release workflow producing the linux x86-64 binary
  artifact (T-21, T-22)
- VS Code extension skeleton under `editors/vscode/extension/` with the
  settings schema for all 13 diagnostic codes (T-19; not yet tested — no
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
- Rename is now gated through `textDocument/prepareRename` (advertised
  with `prepareProvider: true`). With the previous bare `RenameProvider:
  true` clients could not ask whether the cursor sits on a renameable
  name or where that name starts, so editors guessed the word range
  around `'QUOTED.NAME'` — the rename selection came with the surrounding
  quotes — and a rename prompt opened on words that are not names (the
  `DEFINE` verb, the object-type word `QLOCAL`), only to error after the
  user had typed a new name. `prepare_rename` now answers from the same
  `ObjectIndex` that definition/references use: `null` anywhere else
  (Neovim says "Nothing to rename" instead of prompting), and at a name
  site the quote-free range plus the name as placeholder — quoted sites
  keep their quoting because the rename edits still replace the whole
  token
- A `+`/`-` continuation marker with nothing to continue (the rest of the
  file is blank lines, comments or EOF) no longer derails error recovery:
  `DEFINE QLOCAL(A) -\n<EOF>` used to report a phantom `Error[4] "Unexpected
  '*'"` on a nonexistent line (and `Error[1]` when the dash was the file's
  last byte). The scanner now lexes such a marker as a visible
  `dangling_continuation` token and the server reports one honest
  `Error[7] MISSING_CONTINUATION` at the marker itself; the shape is pinned
  by new corpus cases (REVIEW.md deferred appendix)
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
