// Minimal VS Code client for mqsc-lsp (T-19). Plain CommonJS so the
// extension runs without a build step; the only dependency is
// vscode-languageclient (declared in package.json). The server binary is
// NOT bundled — point mqsc.server.path at a build (`cargo build --release`
// in server/) or an installed release.
const vscode = require('vscode');
const { LanguageClient } = require('vscode-languageclient/node');

let client;

const DIAG_CODES = [
	'PARSE_ERROR', 'UNKNOWN_VERB', 'UNKNOWN_OBJECT', 'UNKNOWN_PARAM',
	'BAD_VALUE', 'DUPLICATE_PARAM', 'MISSING_CONTINUATION', 'LEXICAL',
	'MISSING_REQUIRED', 'MISSING_NAME', 'WILDCARD', 'UNDEFINED_OBJECT',
	'NAME_TOO_LONG', 'MISSING_MANDATORY', 'NOT_IN_TARGET',
];

/** The `mqsc.diagnostics.*` settings minus the "default" entries, in the
 *  shape the server reads from initializationOptions / didChangeConfiguration. */
function diagnosticsSettings(cfg) {
	const out = {};
	for (const code of DIAG_CODES) {
		const sev = cfg.get(['diagnostics', code]);
		if (sev && sev !== 'default') {
			out[code] = sev;
		}
	}
	return out;
}

/** The `mqsc.workspaceIndex.*` settings, in the shape the server reads.
 *  PR 7: the server scans the workspace once during `initialized`, so a
 *  change here only takes effect after a restart (the server logs the
 *  outcome either way). */
function workspaceIndexSettings(cfg) {
	const out = {};
	for (const key of ['enabled', 'maxFiles', 'exclude']) {
		const v = cfg.get(['workspaceIndex', key]);
		if (v !== undefined && v !== null) {
			out[key] = v;
		}
	}
	return out;
}

function serverOptions(cfg) {
	const command = cfg.get('server.path') || 'mqsc-lsp';
	const env = Object.assign({}, process.env);
	const logFile = cfg.get('server.logFile');
	if (logFile) {
		env.MQSC_LSP_LOG = logFile; // T-14 server logging
	} else {
		delete env.MQSC_LSP_LOG;
	}
	const options = { env };
	// stdio is the default transport; the deprecated TransportKind enum is
	// gone in vscode-languageclient v9, so just omit it.
	return {
		run: { command, options },
		debug: { command, options },
	};
}

function activate(context) {
	const cfg = vscode.workspace.getConfiguration('mqsc');
	const initOptions = { diagnostics: diagnosticsSettings(cfg) };
	// target version/platform (diagnostic 15): the server ignores empty or
	// unparseable values, so pass them through when set
	for (const key of ['targetVersion', 'platform']) {
		const v = cfg.get(key);
		if (v) {
			initOptions[key] = v;
		}
	}
	// workspace scan of closed *.mqsc files (read once at initialize;
	// changing any mqsc.workspaceIndex.* key needs a window reload)
	const wsIndex = workspaceIndexSettings(cfg);
	if (Object.keys(wsIndex).length) {
		initOptions.workspaceIndex = wsIndex;
	}

	client = new LanguageClient(
		'mqsc', // must match the language id in package.json
		'MQSC Language Server',
		serverOptions(cfg),
		{
			documentSelector: [
				{ language: 'mqsc', scheme: 'file' },
				{ scheme: 'file', pattern: '**/*.mqsc' },
			],
			// Push the whole `mqsc` configuration section to the server at
			// startup and whenever it changes (workspace/didChangeConfiguration
			// via vscode-languageclient). The server reads `{diagnostics:…}`
			// from that payload; `"default"` values are not severities, so
			// they fall through to the natural ones.
			// No `synchronize.fileEvents` on purpose: the server registers its
			// own **/*.mqsc watcher via client/registerCapability (PR 8), and
			// vscode-languageclient answers that with a real FileSystemWatcher.
			// A static fileEvents list here would only duplicate those events.
			synchronize: { configurationSection: 'mqsc' },
			initializationOptions: initOptions,
		}
	);

	// Live-apply severity changes: push the whole mqsc settings object to
	// the server, which reads `{"diagnostics": {...}}` (or nested under
	// "mqsc") from workspace/didChangeConfiguration.
	context.subscriptions.push(
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (!e.affectsConfiguration('mqsc')) {
				return;
			}
			const logFileChanged = e.affectsConfiguration('mqsc.server.logFile');
			const pathChanged = e.affectsConfiguration('mqsc.server.path');
			if (pathChanged || logFileChanged) {
				// binary or log target changed: needs a restart of the server
				client.stop().then(() => client.start()).catch(() => {});
				return;
			}
			// Severity changes need no manual push: the `synchronize` block
			// already forwards the `mqsc` section on change, and a second
			// hand-rolled notification would race with it (REVIEW.md M4).
		})
	);

	return client.start();
}

function deactivate() {
	return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
