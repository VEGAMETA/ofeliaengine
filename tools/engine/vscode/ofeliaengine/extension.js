// The ofeliaengine debugger for VS Code (ADR 0119 E9-2): it names the debug type and
// attaches to the editor's script debugger over DAP. The editor listens on 127.0.0.1 and
// writes its port and a token to `.ofelia/debug/editor.json` in the project; this reads
// them, gives the token in the attach request, and connects. Nothing else runs here: the
// editor is the debug adapter.

const vscode = require('vscode');
const fs = require('fs');
const path = require('path');

/** The editor's endpoint for the project in `folder`: its port and its token. */
function endpoint(config, folder) {
    const file = config.endpoint || path.join(folder, '.ofelia', 'debug', 'editor.json');
    return JSON.parse(fs.readFileSync(file, 'utf8'));
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

    resolveDebugConfigurationWithSubstitutedVariables(folder, config) {
        const root = folder
            ? folder.uri.fsPath
            : vscode.workspace.workspaceFolders && vscode.workspace.workspaceFolders[0]
                ? vscode.workspace.workspaceFolders[0].uri.fsPath
                : undefined;
        if (!root && !config.endpoint) {
            vscode.window.showErrorMessage('ofeliaengine: open the game\'s folder first.');
            return undefined;
        }
        try {
            const found = endpoint(config, root);
            config.token = found.token;
            config.__port = found.port;
        } catch (e) {
            vscode.window.showErrorMessage(
                'ofeliaengine: no editor has this game open (start it with `cargo run --features editor`, then attach).'
            );
            return undefined;
        }
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
