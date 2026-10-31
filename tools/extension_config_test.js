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
        getConfiguration: () => ({
            get: (k) => {
                // Faithful to VS Code's ConfigurationKey, which does
                // key.split('.'): any non-string section is a TypeError.
                if (k !== undefined && typeof k !== 'string') {
                    throw new TypeError(`${String(k)}.split is not a function`);
                }
                return state.values[k];
            },
        }),
        onDidChangeConfiguration: () => ({ dispose() {} }),
    },
    window: { showWarningMessage: () => Promise.resolve(undefined) },
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
    const server = path.join(tmp, 'mqsc-lsp');
    fs.writeFileSync(server, '#!/bin/sh\n', { mode: 0o755 });

    state.values = {
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
    };

    // 1. activation completes: get() with array keys would throw TypeError
    //    right here, which is exactly the shipped v0.1.0/v0.1.1 bug.
    await freshActivate();
    assert.strictEqual(state.started.length, 1, 'exactly one client started');
    const { command, env, initOptions } = state.started[0];
    assert.strictEqual(command, server);
    assert.strictEqual(env.MQSC_LSP_LOG, state.values['server.logFile'],
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
    state.values = { 'server.path': server };
    await freshActivate();
    assert.deepStrictEqual(state.started[0].initOptions, { diagnostics: {} },
        'clean defaults send an empty diagnostics map and no workspaceIndex key');

    console.log('PASS: extension settings plumbing survives activation and ' +
        'builds the server contract');
}

main().catch((e) => {
    console.error('FAIL:', e.message);
    process.exit(1);
});
