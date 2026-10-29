// VS Code client for mqsc-lsp (T-19, server lookup PR 10). Plain CommonJS
// so the extension runs without a build step; the only dependency is
// vscode-languageclient (declared in package.json). The server binary is
// NOT bundled: it is FOUND — mqsc.server.path when set to a real command,
// otherwise PATH, then the common per-user install directories — and when
// nothing resolves, the user gets a link to the GitHub Release instead of
// a dead extension (installing the extension should lead to a working
// server, not PATH troubleshooting).
const vscode = require('vscode');
const { LanguageClient } = require('vscode-languageclient/node');
const { spawnSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

let client;

const RELEASES_URL = 'https://github.com/nils-gg/mqsc-lsp/releases';
const EXE = process.platform === 'win32' ? 'mqsc-lsp.exe' : 'mqsc-lsp';

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

function isFile(p) {
	try {
		return fs.statSync(p).isFile();
	} catch {
		return false;
	}
}

/** PATH lookup without shipping a `which` dependency: `where` on Windows,
 *  `which` everywhere else. Returns the first hit as an absolute path. */
function findOnPath(command) {
	const probe = spawnSync(
		process.platform === 'win32' ? 'where' : 'which',
		[command],
		{ windowsHide: true, encoding: 'utf8' }
	);
	if (probe.status !== 0) {
		return undefined;
	}
	const first = probe.stdout.split(/\r?\n/)[0].trim();
	return first || undefined;
}

/** Where a release download typically ends up when the user follows the
 *  README (extract it there) — the same order is documented there. */
function candidatePaths() {
	const home = os.homedir();
	const dirs =
		process.platform === 'win32'
			? [
					path.join(
						process.env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'),
						'Programs',
						'mqsc-lsp'
					),
					path.join(home, '.local', 'bin'),
				]
			: [
					path.join(home, '.local', 'bin'),
					'/usr/local/bin',
					'/opt/homebrew/bin',
					'/usr/bin',
				];
	return dirs.map((d) => path.join(d, EXE));
}

/** { command } to launch, or { error } — in which case the server must NOT
 *  be started and the user gets guidance instead. */
function resolveServer(cfg) {
	const configured = (cfg.get('server.path') || 'mqsc-lsp').trim();
	if (configured && configured !== 'mqsc-lsp') {
		// an explicit choice is honored exactly — a wrong one must fail
		// loudly with a clear reason, not silently fall back to PATH
		if (/[\\/]/.test(configured)) {
			return isFile(configured)
				? { command: configured }
				: {
						error: `mqsc.server.path points to "${configured}", which does not exist.`,
					};
		}
		return findOnPath(configured)
			? { command: configured }
			: { error: `The command "${configured}" from mqsc.server.path was not found on PATH.` };
	}
	// default "mqsc-lsp" = auto: PATH first (a manual install wins), then
	// the directories the release README tells users to extract into
	const onPath = findOnPath('mqsc-lsp') || findOnPath(EXE);
	if (onPath) {
		return { command: onPath };
	}
	const candidate = candidatePaths().find(isFile);
	if (candidate) {
		return { command: candidate };
	}
	return {
		error:
			'The mqsc-lsp server binary was not found on PATH or in its usual ' +
			'install directories (~/.local/bin, /usr/local/bin, /opt/homebrew/bin, ' +
			'%LOCALAPPDATA%\\Programs\\mqsc-lsp).',
	};
}

/** Tell the user how to fix a missing server, with the two actions that end
 *  the hunt: get the binary, or point the setting at an existing one. */
function showServerHelp(reason) {
	vscode.window
		.showWarningMessage(
			reason + ' Download a prebuilt binary from the mqsc-lsp releases.',
			'Open Release Page',
			'Open Settings'
		)
		.then((choice) => {
			if (choice === 'Open Release Page') {
				vscode.env.openExternal(vscode.Uri.parse(RELEASES_URL));
			} else if (choice === 'Open Settings') {
				vscode.commands.executeCommand(
					'workbench.action.openSettings',
					'mqsc.server.path'
				);
			}
		});
}

function serverOptions(command, cfg) {
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

function initOptionsFor(cfg) {
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
	return initOptions;
}

function buildClient(command, cfg) {
	return new LanguageClient(
		'mqsc', // must match the language id in package.json
		'MQSC Language Server',
		serverOptions(command, cfg),
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
			initializationOptions: initOptionsFor(cfg),
		}
	);
}

function activate(context) {
	const cfg = vscode.workspace.getConfiguration('mqsc');
	const resolved = resolveServer(cfg);
	if (!resolved.command) {
		// no server to launch: guide the user to a download or the setting
		// instead of letting LanguageClient fail on a phantom command
		showServerHelp(resolved.error);
		return undefined;
	}
	client = buildClient(resolved.command, cfg);

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
				// binary or log target changed: the server needs a restart,
				// and a new command needs a NEW client — LanguageClient has
				// the old one baked into its serverOptions
				const nextCfg = vscode.workspace.getConfiguration('mqsc');
				const next = resolveServer(nextCfg);
				client
					.stop()
					.then(() => {
						if (!next.command) {
							showServerHelp(next.error);
							return undefined;
						}
						client = buildClient(next.command, nextCfg);
						return client.start();
					})
					.catch(() => {});
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
