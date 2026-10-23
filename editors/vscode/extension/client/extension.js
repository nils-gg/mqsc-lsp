// Minimal VS Code client for mqsc-lsp (T-19). Plain CommonJS so the
// extension runs without a build step; the only dependency is
// vscode-languageclient. The server binary is NOT bundled — point
// mqsc.server.path at a build (`cargo build --release` in server/) or an
// installed release.
const vscode = require('vscode');
const {
	LanguageClient,
	TransportKind,
} = require('vscode-languageclient/node');

let client;

const DIAG_CODES = [
	'PARSE_ERROR', 'UNKNOWN_VERB', 'UNKNOWN_OBJECT', 'UNKNOWN_PARAM',
	'BAD_VALUE', 'DUPLICATE_PARAM', 'MISSING_CONTINUATION', 'LEXICAL',
	'MISSING_REQUIRED', 'MISSING_NAME', 'WILDCARD', 'UNDEFINED_OBJECT',
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
	return {
		run: { command, transport: TransportKind.stdio, options },
		debug: { command, transport: TransportKind.stdio, options },
	};
}

function activate(context) {
	const cfg = vscode.workspace.getConfiguration('mqsc');
	const initOptions = { diagnostics: diagnosticsSettings(cfg) };

	client = new LanguageClient(
		'mqsc', // must match the language id in package.json
		'MQSC Language Server',
		serverOptions(cfg),
		{
			documentSelector: [
				{ language: 'mqsc', scheme: 'file' },
				{ scheme: 'file', pattern: '**/*.mqsc' },
			],
			synchronize: { fileEvents: vscode.workspace.createFileSystemWatcher('**/*.mqsc') },
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
			const next = vscode.workspace.getConfiguration('mqsc');
			const logFileChanged = e.affectsConfiguration('mqsc.server.logFile');
			const pathChanged = e.affectsConfiguration('mqsc.server.path');
			if (pathChanged || logFileChanged) {
				// binary or log target changed: needs a restart of the server
				client.stop().then(() => client.start()).catch(() => {});
				return;
			}
			if (e.affectsConfiguration('mqsc.diagnostics')) {
				client.sendNotification('workspace/didChangeConfiguration', {
					settings: { diagnostics: diagnosticsSettings(next) },
				});
			}
		})
	);

	return client.start();
}

function deactivate() {
	return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
