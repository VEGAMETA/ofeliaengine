# ofeliaengine for VS Code

Debug an ofeliaengine game's Luau scripts from VS Code, beside the editor's own Debugger
panel: one set of breakpoints for both.

1. Start the editor on the game (`cargo run --features editor`, or `ofeliaengine edit`).
2. In VS Code, open the game's folder and press F5 with **Attach to the editor** (or
   **Attach and Play**, which plays the open scene and stops it when the session ends).
3. Set breakpoints in `scripts/`, logpoints and conditions included; Play stops there.
   The stack, the variables (`self` with its props and its entity's components), hovers,
   watches and the debug console work at a stop; F10, F11 and Shift+F11 step.

The editor listens on 127.0.0.1 only and writes its port and a token to
`.ofelia/debug/editor.json`; this extension reads them. Nothing else connects.
