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

Prerequisites: tree-sitter CLI **0.26.x**, a C compiler, `python3`, and Rust
**1.75** (the pinned toolchain here — see the pinning note below).

```sh
# 1. Grammar (already generated; regenerate only after editing grammar.js)
cd grammars/tree-sitter-mqsc
tree-sitter generate --abi 14      # regenerate parser.c / scanner bindings
tree-sitter test                   # 23 corpus tests

# 2. Model (already extracted and committed; rerun only after editing the
#    extractor AND placing your own copy of the reference markdown at the
#    repository root — it is IBM documentation and not distributed here)
python3 tools/extract_model.py     # rewrites model/mqsc-model.json

# 3. Server
cd server
cargo build --release              # target/release/mqsc-lsp
```

**MSRV pinning:** this machine's toolchain is rustc 1.75 without rustup. If
`cargo check` fails with *"requires rustc X"* / *"edition2024"*, downgrade the
offending transitive crate, e.g. `cargo update -p idna_adapter --precise 1.2.0`.
The direct pins in `Cargo.toml` (tree-sitter `=0.24.7`, tokio `=1.44.2`, …) are
load-bearing; do not bump them casually.

## Using

```sh
server/target/release/mqsc-lsp                # stdio (what editors launch)
server/target/release/mqsc-lsp --tcp 9257     # TCP; one LSP session per connection
server/target/release/mqsc-lsp --version
```

Point your editor at the binary and associate `*.mqsc` with language id
`mqsc`. See [`editors/`](editors/) for VS Code / Neovim / Helix examples.

### Features

| Capability | Behaviour |
| --- | --- |
| Diagnostics | pushed on open/change — see code table below |
| Hover | verb/object → command card (description, synonyms, attribute list); parameter → kind, values, default, reference text; displayable attributes too |
| Completion | verbs on fresh lines, object types after a verb, parameter names (used ones dropped, `NAME(` inserted for value params), enum/flag values inside groups, prefix filtering, docs in the detail pane |
| documentSymbol | one entry per command |
| foldingRange | multi-line continuation chains + comment blocks |
| documentHighlight | same-word highlights |
| semanticTokens | verb / object / parameter / value / string / number / wildcard / comment classification (survives theme changes) |
| Sync | full-text; `initialize` advertises hover, completion (`(`, `,`, `:` triggers), symbols, folding, highlights, semantic tokens |

### Diagnostic codes

| Code | Severity | Meaning |
| --- | --- | --- |
| 1 | Error | parse error (missing element / unexpected token) |
| 2 | Error | unknown verb, with `did you mean` suggestions |
| 3 | Error | unknown object type for the verb (`DEFINE XYZ` → suggests `DEFINE QLOCAL`…) |
| 4 | Error | word is not a parameter/attribute of the resolved command (`MAXMSGS` → "did you mean MAXMSGL?") |
| 5 | Warning | value not in the parameter's enum, empty value list, value shape mismatch |
| 6 | Error | parameter used twice in one command |
| 7 | Error | a parameter word starts a line — missing `+`/`-` continuation on the previous line |
| 8 | Warning | lexical issues: tabs, lines > 2048 chars |
| 9 | Warning | a *single* required parameter missing (suppressed for multi-alternative commands like DEFINE CHANNEL) |
| 10 | Warning | DEFINE/ALTER/DELETE without the object name |
| 11 | Warning | `?` wildcard outside DISPLAY patterns / wildcard in a non-wildcardable value |

## Testing

```sh
cd grammars/tree-sitter-mqsc && tree-sitter test    # grammar corpus (23 tests)
python3 tools/lsp_smoke_test.py                      # full LSP session: diagnostics, hover,
                                                     # completion, symbols, folding, tokens
cd server
cargo run --example probe -- 0                       # analyze one line without the protocol
cargo run --example complete                         # completion behaviour matrix
cargo run --example fold                             # folding ranges
```

`tools/lsp_smoke_test.py` drives the real binary over stdio and prints what
each request returned; run it from the repository root.

## Known limitations

- **Continuation chains are permissive**: a `+`/`-` chain continues across
  blank and comment lines (the grammar treats them as transparent). If a
  parameter word starts a line and the previous command was *complete*, the
  server instead reports the missing `+`/`-` (code 7).
- **MQ 9.x-only constructs are out of scope**: `WHERE`/`SELECT`/`FROM`
  clauses on DISPLAY are accepted as plain parameters (the reference predates
  them), but they are not validated. The reference is also not z/OS-current:
  `RUN`/`REPLACE` `REPLMSG`/`REPLSTR` parameter families are not in the model.
- **Model gaps are warnings, never errors.** The extractor recovers some
  commands from railroad diagrams only (`recovered: true` params exist), and
  a few parameter descriptions are missing; hover/completion degrade
  gracefully there.
- **No cross-object knowledge**: the server is a script checker, not a queue
  manager — it cannot know whether `TGTQ(X)` exists.
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
