//! The script debugger's stage (ADR 0119): while debugging is on, the scripts' stages run on
//! a fiber of their own, and a stop breakpoint gives the thread back halfway through one.
//!
//! - **A stop:** the stop handler (on the fiber, inside the breakpoint's callback) keeps
//!   where it stopped and suspends the fiber; the player's frame goes on without the rest of
//!   the stage, and its next frames run no stage until the editor says how to go on.
//! - **Going on:** at the start of a later frame the player goes back into the stage, the
//!   world and the physics world opened anew for that frame ([`crate::scripts::reopen`]).
//!   When the stage ends, the player runs the rest of its tick.
//! - **Ending it** (Play stops): the stopped call ends with an error, every call after it in
//!   the stage fails at once, and the stage runs out.
//! - **The host is the stage's while it is held:** the player hands out no `Scripts` then;
//!   the editor sets breakpoints through the [`ScriptDebugger`], which holds only the VM.
//! - **Tracy's zones** opened in the stage are closed at a stop and opened again after it
//!   (`engine_profiler::leave_fiber`, `enter_fiber`).

use crate::scripts::Scripts;
use core::ffi::c_void;
use engine_ecs::World;
use engine_platform::fiber::{self, Fiber, Ran};
use engine_script::{Resume, ScriptDebugger, Stopped};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The stage's stack: address space reserved, its pages committed as it grows.
const STACK: usize = 8 << 20;

/// Which stage, with what it is given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StageKind {
    /// A tick's, of `dt` seconds.
    Tick {
        /// The tick's length.
        dt: f32,
    },
    /// A frame's, `alpha` of the way to the next tick.
    Frame {
        /// The frame's length.
        dt: f32,
        /// The fraction of a tick since the last.
        alpha: f32,
    },
}

/// What the fiber's job runs, read once as it starts.
struct Job {
    scripts: *mut Scripts,
    world: *mut World,
    physics: *const crate::ScenePhysics,
    kind: StageKind,
}

/// What the stop handler and the player share.
#[derive(Default)]
struct Shared {
    /// Where the script stopped: set by the handler, taken by the player.
    at: RefCell<Option<Stopped>>,
    /// How to go on: set by the player before it goes back into the stage.
    answer: Cell<Resume>,
}

/// A stage stopped halfway.
pub(crate) struct Held {
    /// Which stage.
    pub(crate) kind: StageKind,
    /// Where it stopped.
    pub(crate) at: Stopped,
    /// How to go on, once the editor said.
    pub(crate) answer: Option<Resume>,
}

/// A held stage that ended ([`ScriptDebug::go_on`]).
pub(crate) struct Ended {
    /// Which stage.
    pub(crate) kind: StageKind,
    /// It was ended at its stop: the rest of its tick does not run.
    pub(crate) aborted: bool,
}

/// The debugger's stage: the fiber, made once, and the stage held on it, if one is.
pub(crate) struct ScriptDebug {
    fiber: Fiber,
    shared: Rc<Shared>,
    /// The job's context, made once.
    job: Box<Job>,
    debugger: ScriptDebugger,
    pub(crate) held: Option<Held>,
}

impl ScriptDebug {
    /// Debugging `scripts`: the fiber made on the calling thread, the stop handler set.
    pub(crate) fn new(scripts: &Scripts) -> std::io::Result<Self> {
        let fiber = Fiber::new(STACK)?;
        let shared = Rc::new(Shared::default());
        let debugger = scripts.debugger();
        let handler = Rc::clone(&shared);
        debugger.set_stop_handler(Some(Box::new(move |stopped: &Stopped| {
            // Only a stage on the fiber can wait.
            if !fiber::in_job() {
                return None;
            }
            *handler.at.borrow_mut() = Some(stopped.clone());
            engine_profiler::leave_fiber();
            fiber::suspend();
            engine_profiler::enter_fiber();
            Some(handler.answer.get())
        })));
        Ok(Self {
            fiber,
            shared,
            job: Box::new(Job {
                scripts: core::ptr::null_mut(),
                world: core::ptr::null_mut(),
                physics: core::ptr::null(),
                kind: StageKind::Tick { dt: 0.0 },
            }),
            debugger,
            held: None,
        })
    }

    /// The VM's debugger, apart from the host.
    pub(crate) fn debugger(&self) -> &ScriptDebugger {
        &self.debugger
    }

    /// Runs `kind`'s stage of `scripts` on the fiber: true when it stopped halfway, and is
    /// held.
    ///
    /// # Safety
    ///
    /// No stage is held. `scripts`, `world` and `physics` are as [`Scripts::tick_open`]
    /// asks, until the stage ends: while it is held, nothing else uses the host, and the
    /// world and the physics world live on (they are opened anew before it goes on).
    pub(crate) unsafe fn run(
        &mut self,
        scripts: *mut Scripts,
        world: *mut World,
        physics: *const crate::ScenePhysics,
        kind: StageKind,
    ) -> bool {
        debug_assert!(self.held.is_none());
        *self.job = Job {
            scripts,
            world,
            physics,
            kind,
        };
        // SAFETY: the job's context lives in `self` and is read once as the job starts; the
        // rest is the caller's word.
        let ran = unsafe { self.fiber.run(stage, (&raw mut *self.job).cast::<c_void>()) };
        self.after(ran, kind)
    }

    /// Goes back into the held stage once the editor said how, with `world` and `physics`
    /// opened anew: what ended, or None while a stage is held still (or again, at the next
    /// stop).
    ///
    /// # Safety
    ///
    /// `world` and `physics` are this frame's, as [`Self::run`] asks for the rest of the
    /// stage.
    pub(crate) unsafe fn go_on(
        &mut self,
        world: *mut World,
        physics: *const crate::ScenePhysics,
    ) -> Option<Ended> {
        let held = self.held.as_mut()?;
        let answer = held.answer.take()?;
        let kind = held.kind;
        // SAFETY: as the caller says.
        unsafe { crate::scripts::reopen(world, physics) };
        self.shared.answer.set(answer);
        let ran = self.fiber.resume();
        if self.after(ran, kind) {
            return None;
        }
        let aborted = answer == Resume::Abort;
        if aborted {
            self.debugger.end_abort();
        }
        Some(Ended { kind, aborted })
    }

    /// Ends the held stage now (Play stops): its stopped call fails, every call after it in
    /// the stage fails at once, and the stage runs out, without the physics world.
    ///
    /// # Safety
    ///
    /// `world` is the player's, as [`Self::run`] asks for the rest of the stage.
    pub(crate) unsafe fn abort(&mut self, world: *mut World) {
        if self.held.take().is_none() {
            return;
        }
        // SAFETY: as the caller says; no physics world: the stage hears no contacts.
        unsafe { crate::scripts::reopen(world, core::ptr::null()) };
        self.shared.answer.set(Resume::Abort);
        // Later stops answer the abort themselves (`engine_script`'s abort mode).
        while self.fiber.resume() == Ran::Suspended {}
        self.shared.at.borrow_mut().take();
        self.debugger.end_abort();
        if self.fiber.take_panic().is_some() {
            log::error!(target: "script", "a script stage panicked as Play stopped at a stop");
        }
    }

    /// After the fiber gave the thread back: whether the stage is held.
    fn after(&mut self, ran: Ran, kind: StageKind) -> bool {
        if ran == Ran::Suspended {
            // Only the stop handler suspends, where it stopped kept.
            let at = self
                .shared
                .at
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Stopped {
                    module: String::new(),
                    line: 0,
                    locals: Vec::new(),
                    upvalues: Vec::new(),
                    stack: Vec::new(),
                });
            self.held = Some(Held {
                kind,
                at,
                answer: None,
            });
            return true;
        }
        self.held = None;
        if let Some(panic) = self.fiber.take_panic() {
            std::panic::resume_unwind(panic);
        }
        false
    }
}

impl Drop for ScriptDebug {
    fn drop(&mut self) {
        // The player ends a held stage first (`Player::end_script_stop`).
        debug_assert!(self.held.is_none());
        self.debugger.set_stop_handler(None);
    }
}

/// The fiber's job: the stage, its Tracy zones following it across stops.
///
/// # Safety
///
/// `context` is a [`ScriptDebug`]'s job, filled by [`ScriptDebug::run`].
unsafe fn stage(context: *mut c_void) {
    // SAFETY: as the caller says; read once.
    let job = unsafe { &*context.cast::<Job>() };
    let (scripts, world, physics, kind) = (job.scripts, job.world, job.physics, job.kind);
    engine_profiler::enter_fiber();
    // SAFETY: as `ScriptDebug::run`'s caller said.
    unsafe {
        let scripts = &mut *scripts;
        match kind {
            StageKind::Tick { dt } => scripts.tick_open(world, dt, physics),
            StageKind::Frame { dt, alpha } => scripts.frame_open(world, dt, alpha, physics),
        }
    }
    engine_profiler::leave_fiber();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use engine_ecs::Transform;
    use engine_script::{BreakAction, Breakpoint, Hits};

    const COUNT: &str = "local Count = component(\"Count\", { n = 0 })
function Count.on_tick(self, dt)
    self.n += 1
    self.Transform.translation += vector.create(1, 0, 0)
end
return Count
";

    fn stop(hits: Hits) -> Breakpoint {
        Breakpoint {
            line: 3,
            action: BreakAction::Stop,
            condition: None,
            hits,
            enabled: true,
        }
    }

    fn x(world: &World, e: engine_ecs::Entity) -> f32 {
        world.get::<Transform>(e).unwrap().translation.x
    }

    #[test]
    fn a_stop_holds_the_stage_while_the_world_is_edited_then_it_goes_on_or_ends() {
        let dir = std::env::temp_dir().join(format!("ofelia-stops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/count.luau"), COUNT).unwrap();
        let mut world = World::new();
        let mut scripts = Box::new(Scripts::load(&mut world, &dir, &dir.join("scripts")).unwrap());
        let a = world.spawn((Transform::default(),));
        let b = world.spawn((Transform::default(),));
        assert!(scripts.add(&mut world, a, "Count") && scripts.add(&mut world, b, "Count"));
        let mut debug = ScriptDebug::new(&scripts).unwrap();
        scripts.set_breakpoints("count", vec![stop(Hits::Nth(1))]);
        let host: *mut Scripts = &mut *scripts;
        let tick = StageKind::Tick { dt: 0.25 };
        // SAFETY: the host and the world outlive the stage; the test leaves the host alone
        // while it is held.
        assert!(unsafe { debug.run(host, &mut world, core::ptr::null(), tick) });
        let at = &debug.held.as_ref().unwrap().at;
        assert_eq!((at.module.as_str(), at.line), ("count", 3));
        assert_eq!(at.stack[0].function.as_deref(), Some("on_tick"));
        // The editor's frames meanwhile: they edit the world and take the breakpoint away.
        world.get_mut::<Transform>(b).unwrap().translation.y = 5.0;
        let c = world.spawn((Transform::default(),));
        debug.debugger().set_breakpoints("count", Vec::new());
        // SAFETY: as above.
        assert!(
            unsafe { debug.go_on(&mut world, core::ptr::null()) }.is_none(),
            "not told"
        );
        debug.held.as_mut().unwrap().answer = Some(Resume::Continue);
        // SAFETY: as above.
        let ended = unsafe { debug.go_on(&mut world, core::ptr::null()) }.unwrap();
        assert!(ended.kind == tick && !ended.aborted);
        // Both ticked: the first from its stop, the second on the edited world.
        assert_eq!(x(&world, a), 1.0);
        let moved = world.get::<Transform>(b).unwrap().translation;
        assert_eq!((moved.x, moved.y), (1.0, 5.0));
        assert!(world.contains(c));
        // Play stops at a stop: the stopped hook ends there, the stage's other calls fail at
        // once, and no instance is stopped by it.
        scripts.set_breakpoints("count", vec![stop(Hits::Every)]);
        let host: *mut Scripts = &mut *scripts;
        // SAFETY: as above.
        assert!(unsafe { debug.run(host, &mut world, core::ptr::null(), tick) });
        // SAFETY: as above.
        unsafe { debug.abort(&mut world) };
        assert!(debug.held.is_none());
        assert_eq!((x(&world, a), x(&world, b)), (1.0, 1.0));
        scripts.set_breakpoints("count", Vec::new());
        scripts.tick(&mut world, 0.25);
        assert_eq!((x(&world, a), x(&world, b)), (2.0, 2.0), "both run on");
        drop(debug);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
