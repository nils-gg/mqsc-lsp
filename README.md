# mqsc-lsp — a language server for IBM MQ MQSC scripts

An LSP server for the IBM MQ Script (MQSC) command language, built on a
purpose-written **tree-sitter grammar** plus a **semantic model extracted
from the IBM MQSC Command Reference** (the 2011 "Script (MQSC) Command
Reference" PDF, converted to markdown and parsed by
[`tools/extract_model.py`](tools/extract_model.py)).

It gives MQSC scripts — the scripts fed to `runmqsc` / `CSQIBP00` /
AMQMQSC, files typically named `*.mqsc` — the editor smarts they never had:
real diagnostics with typo suggestions, hover documentation straight from the
reference, context-aware completion, symbols, folding, and semantic tokens.

## Repository layout

```
grammars/tree-sitter-mqsc/   tree-sitter grammar + external scanner + corpus tests + highlight queries
tools/extract_model.py        extracts the command model from the reference markdown
tools/lsp_smoke_test.py       protocol-level end-to-end test client
model/mqsc-model.json         the extracted model (128 commands, 1336 params, docs, attrs, synonyms)
server/                       Rust LSP server (tower-lsp + tree-sitter), embeds the model
  src/model.rs                model load/lookup, synonym maps, Levenshtein suggestions
  src/analysis.rs             diagnostics, hover, completion, symbols, folding, tokens
  src/server.rs               tower-lsp LanguageServer implementation
  examples/                   analysis probes: probe.rs, complete.rs, fold.rs
examples/sample.mqsc          end-to-end validation script
editors/                      editor configuration examples (vscode/ nvim/ helix/)
```

> **Note:** the source reference (`Script (MQSC) Command Reference`, PDF and
> markdown) is IBM copyrighted documentation and is *not* included in this
> repository — see [ATTRIBUTION.md](ATTRIBUTION.md). Supply your own copy when
> (and only when) you want to regenerate the model.

## How it is put together

The **grammar models structure only**: an MQSC command parses as a verb plus a
flat list of words and parenthesized argument groups (`verb`, `word`,
`argument`, `value`, `string`, `wildcard` nodes). Which word is the object
type, which is a keyword parameter, and what value each parameter takes
cannot be expressed in LALR(1) — so the **server resolves roles against the
model** at analysis time. The external scanner owns the tricky lexical bits:
`*`-in-column-1 comments, `+`/`-` end-of-line continuations, and `'...'`
strings with `''` escapes and continuations.

`tools/extract_model.py` mines the dotted-decimal syntax diagrams (plus
railroad-only recovery) embedded in the reference's markdown, producing
`model/mqsc-model.json`: verbs, object types and synonyms (`DEF QL` →
`DEFINE QLOCAL`), per-command parameters with kinds (flag/enum/integer/
string/name), displayable attribute lists, footnotes and parameter
descriptions. The server embeds that JSON at compile time (`include_str!`),
so the binary is standalone.

## Building

Prerequisites: tree-sitter CLI **0.27.x** (verified on 0.27.1), a C compiler, `python3`, and Rust
**1.75** (the pinned toolchain here — see the pinning note below).

```sh
# 1. Grammar (already generated; regenerate only after editing grammar.js)
cd grammars/tree-sitter-mqsc
tree-sitter generate --abi 14      # regenerate parser.c / scanner bindings
tree-sitter test                   # 30 corpus tests (cargo test runs the same
                                   # corpus through the server's parser, no CLI needed)

# 2. Model (already extracted and committed; rerun only after editing the
#    extractor AND placing your own copy of the reference markdown at the
#    repository root — it is IBM documentation and not distributed here)
python3 tools/extract_model.py     # rewrites model/mqsc-model.json

# 3. Server
cd server
cargo build --release              # target/release/mqsc-lsp
```

No prebuilt binaries are published yet: `.github/workflows/release.yml`
packages the linux x86-64 binary into a GitHub Release **when a `v*` tag is
pushed** — until the first tag exists, build from source as shown above.

**MSRV pinning:** the toolchain pinned here is rustc 1.75 (installed per-user
via rustup, `~/.cargo/bin`). If
`cargo check` fails with *"requires rustc X"* / *"edition2024"*, downgrade the
offending transitive crate, e.g. `cargo update -p idna_adapter --precise 1.2.0`.
The `=` pins in `Cargo.toml` (`idna_adapter`, `hashbrown`, `indexmap`,
`zerofrom`, `zerofrom-derive`) exist only for rustc 1.75; `tree-sitter` is
`"0.24"` (ABI 14, matching the committed `parser.c`). Do not bump the pins
casually or run a broad `cargo update`.

## Using

```sh
server/target/release/mqsc-lsp                # stdio (what editors launch)
server/target/release/mqsc-lsp --tcp 9257     # TCP; one LSP session per connection
                     #   (no auth and no idle timeout — local/LAN dev use only)
server/target/release/mqsc-lsp --check FILE... # lint without an editor:
                     #   file:line:col: severity[code]: message, exit 1 on errors
                     #   ('-' = stdin; --diag CODE=SEVERITY / --settings file.json)
server/target/release/mqsc-lsp --log-file /tmp/mqsc-lsp.log
                     # optional server tracing (also $MQSC_LSP_LOG): request
                     # timings, parse/analyze durations, panics — silent by default
server/target/release/mqsc-lsp --version
```

Point your editor at the binary and associate `*.mqsc` with language id
`mqsc`. See [`editors/`](editors/) for VS Code / Neovim / Helix examples.

### Features

| Capability | Behaviour |
| --- | --- |
| Diagnostics | pushed on open/change — see code table below; every code is configurable (see Configuration) |
| Hover | verb/object → command card (description, synonyms, attribute list); parameter → kind, values, default, reference text; object name → its `DEFINE` (line, object type — also in another open document), use count, "this is the definition" |
| Completion | verbs on fresh lines, object types after a verb, parameter names (used ones dropped; snippet clients get `NAME(${1:…})` with the cursor inside the group), enum/flag values inside groups, object names `DEFINE`d in the script **or any other open document**, prefix filtering, docs in the detail pane |
| codeAction | one-click "Change to `X`" quickfix for every typed suggestion (codes 2/3/4/5), preferred flag per diagnostic |
| definition / references / rename | object names: jump to the `DEFINE` (across open documents), list all sites, rename every site in every open document keeping quoted/bare style |
| documentSymbol | one entry per command, labelled `DEFINE QLOCAL (NAME)` |
| workspace/symbol | every object `DEFINE`d in any open document, grouped by object type |
| foldingRange | multi-line continuation chains + comment blocks |
| documentHighlight | same-word highlights |
| semanticTokens | verb / object / parameter / value / string / number / wildcard / comment classification (survives theme changes); full and range requests; enum values equal to the parameter default carry the `default` modifier |
| Sync | full-text with a cached parse tree reused by every request; `initialize` advertises hover, completion (`(`, `,`, `:` triggers), code actions, symbols, folding, highlights, semantic tokens, definition, references, rename, workspace symbols |

### Diagnostic codes

| Code | Severity | Meaning |
| --- | --- | --- |
| 1 | Error | parse error (missing element / unexpected token) |
| 2 | Error | unknown verb, with `did you mean` suggestions |
| 3 | Error | unknown object type for the verb (`DEFINE XYZ` → suggests `DEFINE QLOCAL`…) |
| 4 | Error | word is not a parameter/attribute of the resolved command (`MAXMSGS` → "did you mean MAXMSGL?") |
| 5 | Warning | value not in the parameter's enum, empty value list, value shape mismatch |
| 6 | Error | parameter used twice in one command |
| 7 | Error | command looks truncated: a parameter word starts a line (missing `+`/`-` continuation on the previous line), or the file ends right after a `+`/`-` continuation marker with no continuation line after it |
| 8 | Warning | lexical issues: tabs, lines > 2048 chars |
| 9 | Warning | a *single* required parameter missing (suppressed for multi-alternative commands like DEFINE CHANNEL) |
| 10 | Warning | DEFINE/ALTER/DELETE without the object name |
| 11 | Warning | `?` wildcard outside DISPLAY patterns / wildcard in a non-wildcardable value |
| 12 | Hint | object name referenced by a queue/object parameter (`TARGET`, `TGTQ`, `XMITQ`, `LIKE`, …) is never `DEFINE`d in this script or any other open document |
| 13 | Warning | DEFINE/ALTER/DELETE object name exceeds its maximum length — 48 characters for queues, processes, topics, listeners…, 1024 for channels; only object types with a curated limit in `model/mqsc-overlay.json` (`name_max`) are checked, pattern names with `*`/`+`/`?` are exempt |

### Configuration

Every code can be re-severitised or switched off. Settings are read from
`initializationOptions` and updated by `workspace/didChangeConfiguration`;
both shapes are accepted:

```json
{ "diagnostics": { "UNDEFINED_OBJECT": "off", "BAD_VALUE": "error" } }
{ "mqsc": { "diagnostics": { "UNDEFINED_OBJECT": "off" } } }
```

Keys are the code number (`"12"`) or its name (`PARSE_ERROR`,
`UNKNOWN_VERB`, `UNKNOWN_OBJECT`, `UNKNOWN_PARAM`, `BAD_VALUE`,
`DUPLICATE_PARAM`, `MISSING_CONTINUATION`, `LEXICAL`, `MISSING_REQUIRED`,
`MISSING_NAME`, `WILDCARD`, `UNDEFINED_OBJECT`, `NAME_TOO_LONG`). Values: `error`,
`warning`, `information`, `hint`, `off`. On a settings change, diagnostics
for all open documents are republished immediately.

### Object knowledge (across open documents)

The server knows what the open documents define and uses it three ways:
completion inside object-name groups and reference parameters offers the
names `DEFINE`d anywhere among the open documents (order-independent),
hint 12 marks references that **no open document** defines, and
definition/references/rename/hover navigate between them. That hint is a
deliberate heuristic: `SYSTEM.*` names are never flagged, a set of scripts
without any `DEFINE` is never flagged, and only the curated
object-reference parameters (queue aliases' `TARGET`, channels' `XMITQ`,
`LIKE`, dynamic-queue `MODEL`, the `MOVE` queue parameters …) are checked,
because the 2011 model types them all as plain strings. Invalidation:
opening or closing a document re-publishes every other document's
diagnostics; typing only re-analyses the edited document (per keystroke,
re-analysing all siblings would be quadratic). Closed documents count
again once you reopen them; the server never touches the filesystem.

## Testing

```sh
cd server
cargo test                                       # 58 unit tests + 3 grammar-corpus tests
                                                 # (corpus runs through the committed
                                                 # parser.c — no tree-sitter CLI needed)
cd ..
python3 tools/lsp_smoke_test.py                  # full assertive LSP session (stdio):
                                                 # diagnostics, hover, completion, symbols,
                                                 # folding, tokens, navigation, cross-file,
                                                 # settings, server log — exit != 0 on FAIL
python3 tools/check_model.py                     # model is byte-identical to extractor output
                                                 # (skips cleanly without the IBM reference)
cd server
echo 'ALTER QLOCAL(NEW1) MAXMSGS' | cargo run --example probe   # analyze text from stdin, no protocol
cargo run --example complete                         # completion behaviour matrix
cargo run --example fold                             # folding ranges
```

`.github/workflows/ci.yml` runs exactly this set on rustc 1.75 (fmt and
clippy `-D warnings` too), plus an end-to-end `--check` CLI gate with exact
exit-code assertions, with the smoke session against the **release**
binary.

`tools/lsp_smoke_test.py` drives the real binary over stdio and prints what
each request returned; run it from the repository root. Beyond printing, it
asserts the regression-sensitive behaviours (quickfix suggestions, names
`DEFINE`d in the script, settings-driven diagnostic filtering) and exits
non-zero when a check fails.

## Known limitations

- **Continuation chains are permissive**: a `+`/`-` chain continues across
  blank and comment lines (the grammar treats them as transparent). If a
  parameter word starts a line and the previous command was *complete*, the
  server instead reports the missing `+`/`-` (code 7). And if no content
  line follows the marker at all — the file ends, with only blanks or
  comments after it — the command is truncated: the parser lexes the marker
  as a visible `dangling_continuation` token and the server reports the
  same code 7 at the marker.
- **MQ 9.x-only constructs are out of scope**: `WHERE`/`SELECT`/`FROM`
  clauses on DISPLAY are accepted as plain parameters (the reference predates
  them), but they are not validated. The reference is also not z/OS-current:
  `RUN`/`REPLACE` `REPLMSG`/`REPLSTR` parameter families are not in the model.
- **Model gaps are warnings, never errors.** The extractor recovers some
  commands from railroad diagrams only (`recovered: true` params exist), and
  a few parameter descriptions are missing; hover/completion degrade
  gracefully there.
- **Knowledge spans open documents, not the filesystem**: the server is a
  script checker, not a queue manager. Objects living in files that are not
  open — or in the queue manager itself — are invisible (which is exactly
  why hint 12 is only a hint). A `workspace/didChangeWatchedFiles` scan of
  `*.mqsc` is the natural next step.
- **False positives are measured, not eliminated**: on a 324-file
  `dmpmqcfg` acceptance corpus (260k diagnostics before the model work),
  the count sits at 19,613 after the model/overlay/grammar fixes (-92.5%);
  codes 3/5/9 are at zero and the remainder is 19,578 × hint 12 by design
  (cross-file references without the sibling open, MQ 9.x parameters).
  Track regressions with `tools/corpus_measure.py` (non-UTF-8 files are
  linted through a lossy decode, never skipped).
- The process exits when stdin is closed after the `exit` notification
  (standard tower-lsp lifecycle); TCP sessions end with their connection.

## License

Source code, grammar and tooling in this repository are licensed under the
[MIT License](LICENSE) (c) 2026 Nils Gerstner.

`model/mqsc-model.json` contains command descriptions and other data derived
from IBM's *Script (MQSC) Command Reference*, which is © IBM and not
redistributed here. Read [ATTRIBUTION.md](ATTRIBUTION.md) for exactly what is
under MIT, what is IBM-derived, and how to rebuild the model from your own
copy of the reference. IBM, WebSphere and MQ are trademarks of International
Business Machines Corporation; this project is not affiliated with or
endorsed by IBM.
