# mqsc-lsp VS Code extension

Minimal client extension for the MQSC language server. Plain JavaScript —
no compile step, `vscode-languageclient` is the only dependency.

**Status: skeleton, untested.** It was written on a machine without a
node/npm toolchain, so nothing here was executed or packaged; expect to
fix small issues on first run. The pieces that require no node to verify
(both JSON manifests) are valid JSON and follow the published contribution
schemas.

## What you get

- `.mqsc` file association with language id `mqsc`
- TextMate grammar fallback (`syntaxes/`) so files are readable before the
  server answers; with the server running, its semantic tokens take over
- Full LSP wiring: diagnostics with quickfixes, completion, hover,
  definition/references/rename (across open files), document and workspace
  symbols, folding, semantic tokens
- Settings UI for the binary path, the server log file (writes
  `MQSC_LSP_LOG`) and a severity override per diagnostic code
  (`default|error|warning|information|hint|off`), applied live via
  `workspace/didChangeConfiguration`

## What the extension does NOT do

- It does not bundle or download the `mqsc-lsp` binary. Build it
  (`cd server && cargo build --release`, MSRV 1.75) and set
  `mqsc.server.path` to `.../server/target/release/mqsc-lsp`, or install a
  release binary and leave the default `mqsc-lsp`.
- No `package-lock.json` — resolve current `vscode-languageclient` at
  install time.

## Build and install (on a machine with node)

```sh
cd editors/vscode/extension
npm install --save vscode-languageclient   # creates the lock file
npx @vscode/vsce package                   # → mqsc-lsp-vscode-0.1.0.vsix
code --install-extension mqsc-lsp-vscode-0.1.0.vsix
```

Set the publisher id in `package.json` before any marketplace publish.
