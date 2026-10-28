//! The standalone player's script debugger (ADR 0119 E9-3; the UX note's §11): a
//! development build started with `--debug` listens for VS Code as the editor does
//! ([`crate::dap`]) and answers its requests in its own frame.
//!
//! - **Nothing without a client:** the player has no breakpoints of its own and no error
//!   stops. What the client set goes with it, and a stop it leaves goes on; a stop that
//!   comes while none is attached (a pause it asked for, reached after it went) goes on at
//!   once.
//! - **A stop** keeps the world still, as in the editor (ADR 0119 E9-1b-1): the frames go
//!   on drawing it, the window's title says where, and the mouse is freed until it goes on.
//! - **`--wait-for-debugger`** keeps the world still until the client's configuration is
//!   done, so the scripts' first hooks can stop.

use super::Player;
use crate::DebuggerSettings;
use crate::dap::{self, Asked, Dap, Debuggee, References, Request};
use engine_script::{ErrorStops, Resume, Stopped};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The endpoint and what its client asked.
pub(super) struct Debugger {
    dap: Dap,
    requests: Vec<Request>,
    references: References,
    /// The game's folder: the scripts' paths start from it.
    root: PathBuf,
    /// The window's title as the game has it.
    title: String,
    /// The client gave the token, as of the last turn: its going is seen once.
    attached: bool,
    /// The modules the client gave breakpoints: emptied when it goes.
    modules: Vec<String>,
    /// The world waits for the client's configuration (`--wait-for-debugger`).
    waiting: bool,
    /// The stop the client was told of (the player's count of stops), as it was then.
    told: Option<(u64, Stopped)>,
    /// Why the next stop comes: a step or a pause asked; a breakpoint otherwise.
    reason: Option<&'static str>,
    /// The cursor was locked when the stop came: locked again when it goes on.
    relock: bool,
}

impl Player {
    /// Listens for VS Code (ADR 0119 E9-3) as `asked`, where the game has a project folder
    /// whose scripts loaded: its endpoint in the project's `.ofelia/debug/`, the scripts
    /// debugged, the world kept still when it is to wait. Said in the log either way.
    pub(super) fn start_debugger(&mut self, asked: DebuggerSettings) {
        let Some(root) = self.settings.project.clone() else {
            log::warn!(target: "script", "--debug: no project folder, so no scripts to debug");
            return;
        };
        let project = std::path::absolute(&root).unwrap_or_else(|_| root.clone());
        let dir = root.join(".ofelia").join("debug");
        let title = self.settings.title.clone();
        let dap = match Dap::start(&dir, &project, Debuggee::Player, &title) {
            Ok(dap) => dap,
            Err(e) => {
                log::warn!(target: "script", "--debug: VS Code cannot attach: {e}");
                return;
            }
        };
        log::info!(
            target: "script",
            "VS Code can debug the scripts (Attach to the player): 127.0.0.1:{}, {}",
            dap.port(),
            dap.file().display()
        );
        self.set_script_debugging(true);
        if asked.wait {
            self.simulating = false;
            self.show_title(&format!("{title} \u{2014} waiting for the debugger"));
        }
        self.debugger = Some(Box::new(Debugger {
            dap,
            requests: Vec::new(),
            references: References::default(),
            root,
            title,
            attached: false,
            modules: Vec::new(),
            waiting: asked.wait,
            told: None,
            reason: None,
            relock: false,
        }));
    }

    /// VS Code's turn (ADR 0119 E9-3), once a frame before a stopped script goes on: a
    /// stop's start and end told, the requests answered, the scripts' log records sent to
    /// its console. Nothing is made while no client speaks and nothing stops.
    pub(super) fn debugger_frame(&mut self) {
        let Some(mut d) = self.debugger.take() else {
            return;
        };
        self.follow_stop(&mut d);
        let mut requests = core::mem::take(&mut d.requests);
        d.dap.take(&mut requests);
        for r in requests.drain(..) {
            let body = self.debugger_answer(&mut d, &r);
            d.dap.respond(r.seq, &r.command, body);
            match r.command.as_str() {
                "initialize" => d.dap.event("initialized", json!({})),
                "attach" if !d.dap.attached() => d.dap.drop_client(),
                "attach" => {
                    // A stop already told of (a client before this one): this one sees it.
                    if let Some((_, stop)) = &d.told {
                        let body = dap::stopped("breakpoint", stop.error.as_ref());
                        d.dap.event("stopped", body);
                    }
                }
                "disconnect" => d.dap.drop_client(),
                _ => {}
            }
        }
        d.requests = requests;
        // The client went, by its word or not: what it set goes with it.
        if d.attached && !d.dap.attached() {
            self.debugger_let_go(&mut d);
        }
        d.attached = d.dap.attached();
        d.dap.forward_log();
        self.debugger = Some(d);
    }

    /// A stop's start and its end, each told to the client once, with the window's title
    /// and the mouse.
    fn follow_stop(&mut self, d: &mut Debugger) {
        let count = self.script_stop().map(|_| self.script_stops());
        if count == d.told.as_ref().map(|(c, _)| *c) {
            return;
        }
        let Some(count) = count else {
            // It went on.
            d.told = None;
            d.references.clear();
            d.dap.event("continued", dap::continued());
            self.show_title(&d.title);
            if core::mem::take(&mut d.relock) {
                self.lock_cursor(true);
            }
            return;
        };
        if !d.dap.attached() {
            // Nobody to show it to.
            self.continue_script();
            return;
        }
        let Some(stop) = self.script_stop().cloned() else {
            return;
        };
        d.references.clear();
        let reason = d.reason.take().unwrap_or("breakpoint");
        d.dap
            .event("stopped", dap::stopped(reason, stop.error.as_ref()));
        self.show_title(&stop_title(&d.title, &stop));
        // A step's next stop finds the mouse freed by the first.
        if self.cursor_locked {
            self.lock_cursor(false);
            d.relock = true;
        }
        d.told = Some((count, stop));
    }

    /// Request `r`'s answer: its body, or why not.
    fn debugger_answer(&mut self, d: &mut Debugger, r: &Request) -> Result<Value, String> {
        match r.command.as_str() {
            "initialize" => {
                // Uncaught errors stop by default, as in the editor (the team's answer 4).
                let errors = ErrorStops {
                    uncaught: true,
                    caught: false,
                };
                return Ok(dap::capabilities(errors));
            }
            "attach" => {
                let token = r.arguments["token"].as_str().unwrap_or_default();
                return if d.dap.attach(token) {
                    Ok(Value::Null)
                } else {
                    Err(format!(
                        "this player's token is in {}: attach through the ofeliaengine \
                         extension",
                        d.dap.file().display()
                    ))
                };
            }
            "disconnect" => return Ok(Value::Null),
            _ if !d.dap.attached() => return Err(String::from("attach first")),
            _ => {}
        }
        let stop = d.told.as_ref().map(|(_, s)| s);
        if let Some(answer) = d.references.answer(r, stop, &d.root, |frame, code| {
            self.evaluate_script(frame, code)
        }) {
            return answer;
        }
        let args = &r.arguments;
        match r.command.as_str() {
            "configurationDone" => {
                if core::mem::take(&mut d.waiting) {
                    self.simulating = true;
                    self.show_title(&d.title);
                }
                Ok(Value::Null)
            }
            "setBreakpoints" => {
                let asked = Asked::all(args);
                let path = args["source"]["path"].as_str().unwrap_or_default();
                let module = dap::project_file(&d.root, path)
                    .and_then(|file| crate::scripts::module_name(&file));
                let (Some(module), Some(debugger)) = (module, self.script_debugger()) else {
                    let not: Vec<Value> = asked
                        .iter()
                        .map(|b| {
                            json!({
                                "verified": false,
                                "line": b.line,
                                "message": "not a script this game runs",
                            })
                        })
                        .collect();
                    return Ok(json!({ "breakpoints": not }));
                };
                let made: Vec<Result<engine_script::Breakpoint, String>> =
                    asked.iter().map(Asked::to_host).collect();
                let given: Vec<engine_script::Breakpoint> = made
                    .iter()
                    .filter_map(|b| b.as_ref().ok().cloned())
                    .collect();
                let any = !given.is_empty();
                debugger.set_breakpoints(&module, given);
                if any && !d.modules.contains(&module) {
                    d.modules.push(module.clone());
                }
                // Where the host placed them, once their module has loaded: the next line
                // with code.
                let mut states = debugger.breakpoint_states(&module).into_iter();
                let answers: Vec<Value> = asked
                    .iter()
                    .zip(&made)
                    .map(|(b, m)| match m {
                        Ok(_) => {
                            let placed = states.next().and_then(|s| s.placed);
                            json!({ "verified": true, "line": placed.unwrap_or(b.line) })
                        }
                        Err(why) => json!({ "verified": false, "line": b.line, "message": why }),
                    })
                    .collect();
                Ok(json!({ "breakpoints": answers }))
            }
            "setExceptionBreakpoints" => {
                if let Some(debugger) = self.script_debugger() {
                    debugger.set_error_stops(dap::error_stops(args));
                }
                Ok(Value::Null)
            }
            "setVariable" => {
                let reference = args["variablesReference"]
                    .as_u64()
                    .and_then(|r| usize::try_from(r).ok())
                    .unwrap_or(0);
                let name = args["name"].as_str().unwrap_or_default();
                let code = args["value"].as_str().unwrap_or_default();
                let stop = d.told.as_ref().map(|(_, s)| s);
                let (frame, place) = d
                    .references
                    .place(reference, name, stop)
                    .ok_or_else(|| format!("`{name}` cannot be changed here"))?;
                let v = self.set_script_value(frame, &place, code)?;
                // The stop read again: what changed shows wherever it is.
                if let (Some((_, stop)), Some(debugger)) = (d.told.as_mut(), self.script_debugger())
                {
                    debugger.refresh(stop);
                }
                Ok(d.references.set_answer(v))
            }
            "continue" => {
                self.continue_script();
                Ok(json!({ "allThreadsContinued": true }))
            }
            "next" | "stepIn" | "stepOut" => {
                self.step_script(match r.command.as_str() {
                    "next" => Resume::StepOver,
                    "stepIn" => Resume::StepInto,
                    _ => Resume::StepOut,
                });
                d.reason = Some("step");
                Ok(Value::Null)
            }
            "pause" => {
                // The next line a script runs stops.
                if self.script_stop().is_none()
                    && self
                        .script_debugger()
                        .is_some_and(|debugger| debugger.break_next())
                {
                    d.reason = Some("pause");
                }
                Ok(Value::Null)
            }
            other => Err(format!("`{other}` is not a request the player answers")),
        }
    }

    /// The client went: its breakpoints and error stops go with it, and a stop goes on.
    fn debugger_let_go(&mut self, d: &mut Debugger) {
        if let Some(debugger) = self.script_debugger() {
            for module in d.modules.drain(..) {
                debugger.set_breakpoints(&module, Vec::new());
            }
            debugger.set_error_stops(ErrorStops::default());
        }
        d.reason = None;
        self.continue_script();
    }

    /// The window's title, where the player has a window.
    fn show_title(&self, title: &str) {
        if let (Some(windows), Some(window)) = (&self.windows, &self.window) {
            let _ = windows.set_title(window.id, title);
        }
    }
}

/// The window's title at a stop: the game's, then where the script stopped.
fn stop_title(title: &str, stop: &Stopped) -> String {
    let file = crate::scripts::module_file(&stop.module);
    let at = if stop.error.is_some() {
        "an error at"
    } else {
        "stopped at"
    };
    format!("{title} \u{2014} {at} {file}:{}", stop.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stops_title_says_where() {
        let mut stop = Stopped {
            module: String::from("npc/guard"),
            line: 12,
            locals: Vec::new(),
            upvalues: Vec::new(),
            stack: Vec::new(),
            steppable: true,
            error: None,
            instance: None,
        };
        assert_eq!(
            stop_title("My game", &stop),
            "My game \u{2014} stopped at scripts/npc/guard.luau:12"
        );
        stop.error = Some(engine_script::ErrorStop {
            message: String::from("attempt to index nil"),
            caught: false,
        });
        assert_eq!(
            stop_title("My game", &stop),
            "My game \u{2014} an error at scripts/npc/guard.luau:12"
        );
    }
}
