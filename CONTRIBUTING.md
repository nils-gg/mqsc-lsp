# Contributing

Small PRs are very welcome. The whole gate set runs locally and in CI —
if it is green here, it is green there:

```sh
cd server && cargo build --release && cargo test   # unit + grammar-corpus tests
cd .. && python3 tools/lsp_smoke_test.py           # assertive full LSP session
python3 tools/workspace_index_smoke.py             # workspace scan/watch session
python3 tools/check_model.py                       # model byte-reproducibility
python3 tools/check_versions.py                    # version agreement
node tools/extension_resolve_test.js               # extension server lookup
node tools/extension_config_test.js                # extension settings plumbing
```

Notes that save time:

- The toolchain is pinned to **rustc 1.75** (see README, "MSRV pinning") —
  do not bump the `=` version pins or run a broad `cargo update`.
- `grammars/tree-sitter-mqsc/src/parser.c` and `scanner.c` are **committed
  generated files**: after editing `grammar.js`, run
  `tree-sitter generate --abi 14` before `cargo build`.
- The command model (`model/mqsc-model.json`) is generated and
  byte-guarded; behavior changes belong in the server or, for model
  corrections, in `model/mqsc-overlay.json` — with evidence (live parser
  output or real-world scripts) if a diagnostic changes.
- If your PR touches user-visible behavior, add or adjust a test and a
  README line; `CHANGELOG.md` gets one line under `[Unreleased]`.
