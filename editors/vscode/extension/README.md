# mqsc-lsp VS Code extension

Client extension for the MQSC language server. Plain JavaScript — no
compile step, `vscode-languageclient` (declared in `package.json`) is the
only dependency.

**Status: packaged and resolution-tested; no live VS Code session yet.**
`vsce package` runs clean and `tools/extension_resolve_test.js` drives the
activation path (server lookup, guidance) against a stubbed VS Code host
in CI, but nothing here has been executed inside a real VS Code window.

## What you get

- `.mqsc` file association with language id `mqsc`
- TextMate grammar fallback (`syntaxes/`) so files are readable before the
  server answers; with the server running, its semantic tokens take over
- Full LSP wiring: diagnostics with quickfixes, completion, hover,
  definition/references/rename (across open files **and** the workspace's
  closed `*.mqsc` files — the server indexes them at startup and keeps
  them current through a file watcher it registers with VS Code; rename
  rewrites all of it), document and workspace
  symbols, folding, semantic tokens
- Server lookup without configuration: `mqsc.server.path` when set to a
  real command/path is honored exactly; the default auto mode searches
  `PATH`, then `~/.local/bin`, `/usr/local/bin`, `/opt/homebrew/bin`
  (or `%LOCALAPPDATA%\Programs\mqsc-lsp` on Windows), and shows a link to
  the GitHub Releases when nothing is found instead of failing silently
- Settings UI for the binary path, the server log file (writes
  `MQSC_LSP_LOG`), the MQ target (`mqsc.targetVersion` / `mqsc.platform`,
  which enable diagnostic 15 NOT_IN_TARGET; empty by default = no
  availability checks), the workspace index
  (`mqsc.workspaceIndex.enabled` / `maxFiles` / `exclude` — read once at
  startup, so a change needs a window reload) and a severity override per
  diagnostic code (`default|error|warning|information|hint|off`), applied live via
  `workspace/didChangeConfiguration`

## What the extension does NOT do

- It does not bundle or download the `mqsc-lsp` binary — VSIX size stays
  small. It *finds* one (see above); install a prebuilt release binary
  from the GitHub Releases (extract it onto your `PATH` or into
  `~/.local/bin`) or build it (`cd server && cargo build --release`,
  MSRV 1.75) and point `mqsc.server.path` at the result. Download-on-
  first-use is a plausible future step, deliberately not taken yet.
- It is not published to a marketplace yet. The `publisher` id
  (`nils-gg`) is set and the manifest carries the repository/metadata
  vsce requires, so the VSIX the Release workflow attaches is one
  `vsce publish` away from a listing once the marketplace publisher
  exists.

## Build and install (on a machine with node)

```sh
cd editors/vscode/extension
npm ci                                       # exact versions from package-lock.json
npx @vscode/vsce package                     # → mqsc-lsp-vscode-<version>.vsix
code --install-extension mqsc-lsp-vscode-<version>.vsix
```

The Release workflow does exactly this on every `vX.Y.Z` tag and attaches
the VSIX to the GitHub Release, so installing that file plus a matching
release binary is the full product.
