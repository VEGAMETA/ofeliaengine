//! VM behavior: modules, hot reload with state, errors with lines, time budget, memory
//! limit, sandbox, native and interpreted agreement.

#![allow(clippy::unwrap_used)]

use engine_script::{ScriptError, ScriptSettings, ScriptVm};
use std::time::Duration;

fn vm() -> ScriptVm {
    ScriptVm::new(ScriptSettings::default()).unwrap()
}

#[test]
fn reload_keeps_state_and_calls_on_reload() {
    let mut vm = vm();
    vm.load_module(
        "m",
        "local s = ... s.v = 1 return { get = function() return s.v end }",
    )
    .unwrap();
    vm.load_module(
        "m",
        "local s = ... local M = {} function M.get() return s.v end \
         function M.on_reload(old) s.v = old.get() + 41 end return M",
    )
    .unwrap();
    assert_eq!(vm.call::<_, f64>("m", "get", ()).unwrap(), 42.0);
}

#[test]
fn a_broken_edit_keeps_the_old_code_and_names_the_line() {
    let mut vm = vm();
    vm.load_module("ai", "return { think = function() return 1 end }")
        .unwrap();
    let e = vm
        .load_module(
            "ai",
            "local M = {}\nfunction M.think()\n  return 1 +\nend\nreturn M",
        )
        .unwrap_err();
    match &e {
        ScriptError::Compile { module, line, .. } => {
            assert_eq!(module, "ai");
            assert_eq!(*line, Some(4), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        vm.call::<_, f64>("ai", "think", ()).unwrap(),
        1.0,
        "old code still runs"
    );
}

#[test]
fn runtime_errors_name_the_line() {
    let mut vm = vm();
    vm.load_module(
        "door",
        "local M = {}\nfunction M.open(t)\n  return t.hinge.angle\nend\nreturn M",
    )
    .unwrap();
    let e = vm.call::<_, ()>("door", "open", ()).unwrap_err();
    match e {
        ScriptError::Runtime { line, .. } => assert_eq!(line, Some(3)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        vm.call::<_, ()>("door", "close", ()),
        Err(ScriptError::UnknownFunction { .. })
    ));
    assert!(matches!(
        vm.call::<_, ()>("nope", "x", ()),
        Err(ScriptError::UnknownModule(_))
    ));
}

#[test]
fn runaway_loops_stop_at_the_budget() {
    let mut vm = ScriptVm::new(ScriptSettings {
        call_budget: Duration::from_millis(20),
        ..ScriptSettings::default()
    })
    .unwrap();
    vm.load_module(
        "bad",
        "return { spin = function() while true do end end, ok = function() return 7 end }",
    )
    .unwrap();
    let t = std::time::Instant::now();
    assert!(matches!(
        vm.call::<_, ()>("bad", "spin", ()),
        Err(ScriptError::Timeout { .. })
    ));
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(
        vm.call::<_, f64>("bad", "ok", ()).unwrap(),
        7.0,
        "the VM stays usable"
    );
}

#[test]
fn memory_limit_is_enforced() {
    let mut vm = ScriptVm::new(ScriptSettings {
        memory_limit: 8 << 20,
        ..ScriptSettings::default()
    })
    .unwrap();
    vm.load_module(
        "hog",
        "return { eat = function() local t = {} for i = 1, 1e8 do t[i] = i end end }",
    )
    .unwrap();
    assert_eq!(
        vm.call::<_, ()>("hog", "eat", ()),
        Err(ScriptError::OutOfMemory)
    );
}

#[test]
fn sandbox_blocks_globals_and_os_access() {
    let mut vm = vm();
    let e = vm
        .load_module("evil", "os.execute('calc') return {}")
        .unwrap_err();
    assert!(matches!(e, ScriptError::Runtime { .. }), "{e:?}");
    vm.load_module("a", "x_global = 1 return {}").unwrap();
    vm.load_module("b", "return { get = function() return x_global end }")
        .unwrap();
    assert_eq!(
        vm.call::<_, Option<f64>>("b", "get", ()).unwrap(),
        None,
        "no shared globals"
    );
}

#[test]
fn native_and_interpreted_agree() {
    let src = "return { f = function(n) local s = 0 for i = 1, n do s += math.sqrt(i) * (i % 7) end return s end }";
    let mut native = vm();
    let mut interp = ScriptVm::new(ScriptSettings {
        native: false,
        ..ScriptSettings::default()
    })
    .unwrap();
    native.load_module("m", src).unwrap();
    interp.load_module("m", src).unwrap();
    let a: f64 = native.call("m", "f", 10_000).unwrap();
    let b: f64 = interp.call("m", "f", 10_000).unwrap();
    assert_eq!(a.to_bits(), b.to_bits());
}
