// Behavior test for the VS Code extension's settings plumbing (v0.1.2 fix).
//
// The bug this pins: the settings helpers passed an ARRAY key to
// WorkspaceConfiguration.get(); VS Code parses dotted sections with
// String.prototype.split and throws a TypeError for a non-string section,
// so activate() died before any LanguageClient was built — users got only
// the contributed TextMate highlighting and no language features. The
// server-lookup test (extension_resolve_test.js) could not catch this: its
// permissive stub answered ANY key with undefined. This stub is faithful
// instead — a non-string section throws exactly like real VS Code does —
// and it additionally asserts the full initializationOptions payload the
// server contract expects (severity map without "default" entries, the
// workspaceIndex block with `false` surviving, target + log env pass-through).
//
// Run: node tools/extension_config_test.js
const Module = require('module');
const fs = require('fs');
const os = require('os');
const path = require('path');
const assert = require('assert');

const EXT = path.join(__dirname, '..', 'editors', 'vscode', 'extension',
    'client', 'extension.js');

const state = { values: {}, started: [] };

class FakeLanguageClient {
    constructor(id, name, serverOptions, clientOptions) {
        this.serverOptions = serverOptions;
        this.clientOptions = clientOptions;
    }
    start() {
        state.started.push({
            command: this.serverOptions.run.command,
            env: this.serverOptions.run.options.env,
            initOptions: this.clientOptions.initializationOptions,
        });
        return Promise.resolve();
    }
    stop() { return Promise.resolve(); }
}

const vscodeStub = {
    workspace: {
        // T11 (REVIEW 2026-10-31): the view honors its section argument —
        // get(key) resolves against `section`, and ONLY the 'mqsc' section
        // has values. A wrong section string in extension.js used to read
        // every setting fine through this stub; now it sees an empty world
        // and the assertions below fail.
        getConfiguration: (section) => ({
            get: (k) => {
                // Faithful to VS Code's ConfigurationKey, which does
                // key.split('.'): any non-string section is a TypeError.
                if (k !== undefined && typeof k !== 'string') {
                    throw new TypeError(`${String(k)}.split is not a function`);
                }
                if (section !== 'mqsc') return undefined;
                return state.values[`${section}.${k}`];
            },
        }),
        onDidChangeConfiguration: () => ({ dispose() {} }),
    },
    window: {
        showWarningMessage: () => Promise.resolve(undefined),
        showErrorMessage: () => Promise.resolve(undefined),
    },
    env: { openExternal: () => Promise.resolve(true) },
    commands: { executeCommand: () => Promise.resolve() },
    Uri: { parse: (s) => s },
};

const origLoad = Module._load;
Module._load = function (request, ...rest) {
    if (request === 'vscode') return vscodeStub;
    if (request === 'vscode-languageclient/node')
        return { LanguageClient: FakeLanguageClient };
    return origLoad.call(this, request, ...rest);
};

function freshActivate() {
    delete require.cache[require.resolve(EXT)];
    state.started = [];
    return require(EXT).activate({ subscriptions: { push() {} } });
}

async function main() {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'mqsc-ext-cfg-'));
    try {
    const server = path.join(tmp, 'mqsc-lsp');
    fs.writeFileSync(server, '#!/bin/sh\n', { mode: 0o755 });

    // settings written the way a user's settings.json spells them
    const V = (o) => Object.fromEntries(
        Object.entries(o).map(([k, v]) => [`mqsc.${k}`, v]));

    state.values = V({
        'server.path': server,
        'server.logFile': path.join(tmp, 'mqsc-lsp.log'),
        'diagnostics.PARSE_ERROR': 'warning',
        'diagnostics.UNDEFINED_OBJECT': 'hint',
        'diagnostics.WILDCARD': 'default',          // must NOT reach the server
        'workspaceIndex.enabled': false,            // `false` must survive
        'workspaceIndex.maxFiles': 500,
        'workspaceIndex.exclude': ['generated'],
        'targetVersion': '9.4.0',
        'platform': 'z/OS',
    });
    const logFile = state.values['mqsc.server.logFile'];

    // 1. activation completes: get() with array keys would throw TypeError
    //    right here, which is exactly the shipped v0.1.0/v0.1.1 bug.
    await freshActivate();
    assert.strictEqual(state.started.length, 1, 'exactly one client started');
    const { command, env, initOptions } = state.started[0];
    assert.strictEqual(command, server);
    assert.strictEqual(env.MQSC_LSP_LOG, logFile,
        'server.logFile reaches MQSC_LSP_LOG');

    // 2. the severity map the server reads: overrides only, no "default"
    assert.deepStrictEqual(initOptions.diagnostics, {
        PARSE_ERROR: 'warning',
        UNDEFINED_OBJECT: 'hint',
    }, 'diagnostics settings');

    // 3. the workspaceIndex block (server default is on; false must pass)
    assert.deepStrictEqual(initOptions.workspaceIndex, {
        enabled: false,
        maxFiles: 500,
        exclude: ['generated'],
    }, 'workspaceIndex settings');

    // 4. target pass-through
    assert.strictEqual(initOptions.targetVersion, '9.4.0');
    assert.strictEqual(initOptions.platform, 'z/OS');

    // 5. defaults: no settings at all -> empty maps, client still starts
    state.values = V({ 'server.path': server });
    await freshActivate();
    assert.deepStrictEqual(state.started[0].initOptions, { diagnostics: {} },
        'clean defaults send an empty diagnostics map and no workspaceIndex key');

    console.log('PASS: extension settings plumbing survives activation and ' +
        'builds the server contract');
    } finally {
        // T9 (REVIEW 2026-10-31): the temp dir used to leak on every FAIL.
        fs.rmSync(tmp, { recursive: true, force: true });
    }
}

main().catch((e) => {
    console.error('FAIL:', e.message);
    process.exit(1);
});
