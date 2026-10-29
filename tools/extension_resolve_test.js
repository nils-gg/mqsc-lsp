// Behavior test for the VS Code extension's server resolution (PR 10).
//
// Loads extension.js with stubbed `vscode` / `vscode-languageclient`
// modules — no VS Code install and no npm dependencies — and drives
// activate() through the five resolution scenarios: nothing found shows
// guidance and starts no client; an explicit missing path shows guidance
// quoting it; an explicit existing path is used verbatim; PATH hits and
// ~/.local/bin candidates are found in auto mode.
//
// Run: node tools/extension_resolve_test.js
const Module = require('module');
const fs = require('fs');
const path = require('path');
const assert = require('assert');
const os = require('os');

const EXT = path.join(__dirname, '..', 'editors', 'vscode', 'extension',
    'client', 'extension.js');

const state = {
    serverPath: undefined, // value of mqsc.server.path, if the scenario sets one
    warnings: [],
    startedWith: [],
};

class FakeLanguageClient {
    constructor(id, name, serverOptions) {
        this.options = serverOptions;
    }
    start() {
        state.startedWith.push(this.options.run.command);
        return Promise.resolve();
    }
    stop() {
        return Promise.resolve();
    }
}

const vscodeStub = {
    workspace: {
        getConfiguration: () => ({
            get: (k) => (k === 'server.path' ? state.serverPath : undefined),
        }),
        onDidChangeConfiguration: () => ({ dispose() {} }),
    },
    window: {
        showWarningMessage: (msg) => {
            state.warnings.push(msg);
            return Promise.resolve(undefined);
        },
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

function freshActivate(nextState) {
    Object.assign(state, { warnings: [], startedWith: [] }, nextState);
    delete require.cache[require.resolve(EXT)];
    const ext = require(EXT);
    return ext.activate({ subscriptions: { push() {} } });
}

async function main() {
    const emptyHome = fs.mkdtempSync(path.join(os.tmpdir(), 'mqsc-ext-home-'));
    const realPath = process.env.PATH;
    const realHome = process.env.HOME;
    // the fake PATH keeps system utilities (which/ls) reachable but holds
    // no mqsc-lsp; no-bin is where scenario 4 "installs" one
    const noBin = path.join(emptyHome, 'no-bin');
    fs.mkdirSync(noBin);
    process.env.PATH = noBin + ':/usr/bin:/bin';
    process.env.HOME = emptyHome; // candidate dirs live under HOME

    // 1. nothing anywhere -> guidance, no client
    await freshActivate({ serverPath: undefined });
    assert.strictEqual(state.startedWith.length, 0, 'no client without a server');
    assert.strictEqual(state.warnings.length, 1, 'exactly one guidance message');
    assert.ok(/not found/.test(state.warnings[0]), state.warnings[0]);

    // 2. explicit path that does not exist -> guidance quoting the path
    await freshActivate({ serverPath: '/nowhere/mqsc-lsp' });
    assert.strictEqual(state.startedWith.length, 0);
    assert.ok(/\/nowhere\/mqsc-lsp/.test(state.warnings[0]), state.warnings[0]);

    // 3. explicit path that exists -> started with exactly it
    const real = path.join(emptyHome, 'mqsc-lsp');
    fs.writeFileSync(real, '#!/bin/sh\n', { mode: 0o755 });
    await freshActivate({ serverPath: real });
    assert.deepStrictEqual(state.startedWith, [real]);
    assert.strictEqual(state.warnings.length, 0);

    // 4. auto mode finds a PATH hit and resolves it absolutely
    const onPath = path.join(noBin, 'mqsc-lsp');
    fs.writeFileSync(onPath, '#!/bin/sh\n', { mode: 0o755 });
    await freshActivate({ serverPath: 'mqsc-lsp' }); // the default value
    assert.deepStrictEqual(state.startedWith, [onPath]);

    // 5. auto mode finds a ~/.local/bin candidate when PATH has nothing
    fs.rmSync(onPath);
    fs.mkdirSync(path.join(emptyHome, '.local', 'bin'), { recursive: true });
    const local = path.join(emptyHome, '.local', 'bin', 'mqsc-lsp');
    fs.copyFileSync(real, local);
    await freshActivate({ serverPath: undefined });
    assert.deepStrictEqual(state.startedWith, [local]);

    process.env.PATH = realPath;
    process.env.HOME = realHome;
    console.log('PASS: all 5 resolution scenarios behave as documented');
}

main().catch((e) => {
    console.error('FAIL:', e.message);
    process.exit(1);
});
