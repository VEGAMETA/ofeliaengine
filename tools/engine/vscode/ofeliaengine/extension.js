// The ofeliaengine debugger for VS Code (ADR 0119 E9-2, E9-3): it names the debug type and
// attaches to a script debugger over DAP, the editor's or a standalone player's (a
// development build started with `--debug`). Each listens on 127.0.0.1 and writes its port
// and a token to the project's `.ofelia/debug/` (`editor.json`, `player-<pid>.json`); this
// reads them, gives the token in the attach request, and connects. Nothing else runs here:
// the editor or the player is the debug adapter.

const vscode = require('vscode');
const fs = require('fs');
const path = require('path');

/** Whether process `pid` runs: a player's file outlives a player that crashed. */
function running(pid) {
    try {
        process.kill(pid, 0);
        return true;
    } catch (e) {
        return e.code === 'EPERM';
    }
}

/** The players of the project in `folder` that listen, newest first. */
function players(folder) {
    const dir = path.join(folder, '.ofelia', 'debug');
    let names = [];
    try {
        names = fs.readdirSync(dir).filter((n) => /^player-\d+\.json$/.test(n));
    } catch (e) {
        return [];
    }
    const found = [];
    for (const name of names) {
        const file = path.join(dir, name);
        try {
            const endpoint = JSON.parse(fs.readFileSync(file, 'utf8'));
            if (running(endpoint.pid)) {
                found.push({ endpoint, time: fs.statSync(file).mtimeMs });
            }
        } catch (e) {
            // Being written, or not ours.
        }
    }
    return found.sort((a, b) => b.time - a.time).map((f) => f.endpoint);
}

/** The endpoint `config` attaches to in the project in `folder`, or why there is none. */
async function endpoint(config, folder) {
    if (config.endpoint) {
        return JSON.parse(fs.readFileSync(config.endpoint, 'utf8'));
    }
    if (config.target !== 'player') {
        const file = path.join(folder, '.ofelia', 'debug', 'editor.json');
        try {
            return JSON.parse(fs.readFileSync(file, 'utf8'));
        } catch (e) {
            throw new Error(
                'no editor has this game open (start it with `cargo run --features editor`, then attach)'
            );
        }
    }
    const found = players(folder);
    if (found.length === 0) {
        throw new Error(
            'no player of this game listens (start it with `cargo run -- --debug`, or `-- --wait-for-debugger` to stop in its first hooks)'
        );
    }
    if (found.length === 1) {
        return found[0];
    }
    // Several: the newest first in the list.
    const picked = await vscode.window.showQuickPick(
        found.map((e) => ({ label: e.title || 'player', description: `process ${e.pid}`, endpoint: e })),
        { placeHolder: 'Which player to attach to' }
    );
    if (!picked) {
        return undefined;
    }
    return picked.endpoint;
}

class Configurations {
    resolveDebugConfiguration(folder, config) {
        // F5 with no launch.json: attach to the editor.
        if (!config.type && !config.request) {
            config.type = 'ofeliaengine';
            config.request = 'attach';
            config.name = 'Attach to the editor';
        }
        return config;
    }

    async resolveDebugConfigurationWithSubstitutedVariables(folder, config) {
        const root = folder
            ? folder.uri.fsPath
            : vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0]
                ? vscode.workspace.workspaceFolders[0].uri.fsPath
                : undefined;
        if (!root && !config.endpoint) {
            vscode.window.showErrorMessage('ofeliaengine: open the game\'s folder first.');
            return undefined;
        }
        let found;
        try {
            found = await endpoint(config, root);
        } catch (e) {
            vscode.window.showErrorMessage(`ofeliaengine: ${e.message}.`);
            return undefined;
        }
        if (!found) {
            return undefined;
        }
        config.token = found.token;
        config.__port = found.port;
        return config;
    }
}

class Adapter {
    createDebugAdapterDescriptor(session) {
        return new vscode.DebugAdapterServer(session.configuration.__port, '127.0.0.1');
    }
}

function activate(context) {
    context.subscriptions.push(
        vscode.debug.registerDebugConfigurationProvider('ofeliaengine', new Configurations()),
        vscode.debug.registerDebugAdapterDescriptorFactory('ofeliaengine', new Adapter())
    );
}

function deactivate() {}

module.exports = { activate, deactivate };
