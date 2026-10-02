# Access violations in test processes: the validation layer's allocator and thread lifetime

Investigation of the intermittent `STATUS_ACCESS_VIOLATION` (0xc0000005) deaths of test
processes, mostly of the golden test binary (`tests/golden/tests/golden.rs`, ADR 0062).
Measured on ref-1 (`docs/perf/reference-hw.md`) on 2026-10-02 with the Vulkan SDK
1.4.350.0 (validation layer `VkLayer_khronos_validation.dll` from it), NVIDIA RTX 4060 Ti,
Windows 11 24H2. Checked against mimalloc's sources on the same date.

## Conclusion

**A bug in the allocator the validation layer links, triggered by the engine creating
Vulkan instances on threads that exit.** The validation layer of the SDK 1.4.350 links
mimalloc v3.3.2 statically (VVL `scripts/known_good.json` at tag `vulkan-sdk-1.4.350.0`).
That mimalloc version:

1. takes the thread on which the layer DLL is loaded (its `DLL_PROCESS_ATTACH`, which runs
   on the thread calling `vkCreateInstance`) for the process's main thread, and remembers
   it by `_mi_prim_thread_id()`, which on Windows is the address of the thread's TEB;
2. when that thread exits (`DLL_THREAD_DETACH` → `_mi_thread_done`), frees the main
   thread's heap: `_mi_theap_free(&theap_main)` sets `theap_main.tld` and `.heap` to null;
3. when Windows later gives a new thread a TEB at the same address (it reuses them: in the
   regression test after 244–1384 threads), `_mi_is_main_thread()` says yes, the thread's
   first allocation in the layer installs `&theap_main` as its heap and reads
   `theap_main.tld->subproc`: a read of address 0x18 in `_mi_subproc` (`init.c:435`).

Every Rust test runs on a thread of its own, so the thread that loaded the layer always
exits early, and every later test thread and every queue thread the layer starts (one per
`VkQueue`, `vvl::Queue::ThreadFunc`) is a candidate. More threads (parallel tests, a loaded
machine stretching the run) mean more chances.

mimalloc has fixed it upstream since (issue
[#1287](https://github.com/microsoft/mimalloc/issues/1287), "mimalloc >= 3.3.0 causes
segmentation faults when used from multiple threads"; `dev3` now sets the freed
`tld_main`'s thread id to an invalid value "as sometimes the same thread-id is reused by
the OS after a thread has terminated"), and the validation layer's `main` builds mimalloc
v3.5.3 now. A later SDK will probably not crash; the engine's fix below does not depend
on that.

## Evidence

**Rate before the fix** (golden test binary, default parallel test threads, 23 tests):

| Build | Runs | Died with 0xc0000005 |
|---|---|---|
| origin/main 4931c2c | 30 + 30 | 3 + 4 |
| with the faulting thread's stack captured (vectored exception handler, local only) | 30 + 60 | 4 + 8 |
| layers and drivers pinned (loaded once per process), instances still created on the test threads | 60 | 4 |
| instances created on the loader thread (the fix), with the stack capture, 8 busy CPU threads | 100 | **0** |
| the fix, final binary (no capture), 8 busy CPU threads | 60 | **0** |
| the fix, final binary, Radeon iGPU (`ENGINE_ADAPTER=radeon`), 8 busy CPU threads | 20 | **0** (3 runs had an image mismatch: the iGPU's LDS fault under CPU load, `radeon-igpu-lds-faults.md`) |

Before the fix 19 of 180 runs crashed (10.6 %); after it 0 of 180. With p = 0.106 the chance
of 160 clean NVIDIA runs in a row is 0.894¹⁶⁰ ≈ 2·10⁻⁸.

All 21 crashes faulted at the same instruction. **The faulting thread's stack**
(symbolized with the SDK's `VkLayer_khronos_validation.pdb` and `llvm-symbolizer`; 16
captured, 15 of them on the layer's queue threads, like this one):

```
VkLayer_khronos_validation.dll+0xa40f9d  _mi_subproc               mimalloc/src/init.c:435
                              +0xa41ee9  mi_heap_main              mimalloc/src/init.c:452
                              +0xa4212b  mi_thread_init            mimalloc/src/init.c:722
                              +0xa4729d  _mi_malloc_generic        mimalloc/src/page.c:987
                              +0xa4052b  mi_theap_alloc_new        mimalloc/src/alloc.c:697
                              +0x2703b8  phmap raw_hash_set<QueryObject,…>::resize
                              +0x26e1a9  core::SetQueryStateMulti  core_checks/cc_state_tracker.cpp:691
                              +0x267006  …RecordResetQueryPool lambda
                              +0x2692c0  core::QueueSubState::Retire
                              +0x6b4d9d  vvl::Queue::Retire
                              +0x6b503c  vvl::Queue::ThreadFunc   state_tracker/queue_state.cpp:333
ucrtbase.dll, kernel32.dll, ntdll.dll  (thread start)
```

and once on a test thread, in `vkCreateInstance`: `vulkan_layer_chassis::GetInstanceProcAddr`
(`chassis/chassis_manual.cpp:152`, constructing a `std::string`) → `operator new` → the
same five mimalloc frames. At the fault (8 crashes with registers dumped, all alike): `rdx` (the thread's default heap, read from
mimalloc's TLS slot, `TlsGetValue` index 54) = `VkLayer_khronos_validation.dll+0x12a6400`,
which the PDB names `theap_main`; its first words `0, 0, 1, …` are `tld = null, heap = null,
refcount = 1`: the main heap after `_mi_theap_free`.

**Deterministic reproduction** (`crates/engine_rhi/tests/instance_lifetime.rs`): create the
process's first instance with validation on a thread that then exits, start threads until
one has the same TEB address, call into the layer from it. Instances created on the calling
thread: 5 of 5 runs died with 0xc0000005. Instances created on the loader thread: 0 of 8.

**What did not cause it.** The first suspect was the loader unloading the layer and the
driver with the process's last instance and loading them again (it does: each `Instance`
loaded and freed `vulkan-1.dll` through its own `ash::Entry`, and the loader frees an
instance's layers and drivers in `vkDestroyInstance`). Keeping them loaded did not change
the rate (4 of 60), and the crashing layer had the same base address in every crash. The
Radeon's LDS fault (`radeon-igpu-lds-faults.md`) is unrelated: these crashes are on NVIDIA,
in CPU code.

## The fix

- `engine_rhi::Instance` is created on one thread that lives as long as the process
  (`modules.rs`, `on_loader_thread`): the libraries the loader loads for an instance are
  initialized on a thread that never exits, so mimalloc's main thread stays alive and its
  TEB is never reused. One channel hand-off per instance creation.
- The loader is loaded once per process, and every module that appears while an instance
  is created (layers, drivers) is pinned (`GetModuleHandleExW(PIN)`), like Streamline's
  DLLs (ADR 0063): no unload and reload of the validation layer and the driver between
  tests. Not the cause here, but it saves 4–6 ms per instance created after the first
  (ADR 0006 addendum: 24 → 19 ms) and removes a second class of the same hazard (a
  library's threads or per-thread state outliving its unloaded code).

Related spots fixed with it (`engine_fsr/csrc/shim.cpp`), found while looking for
process-wide state that several devices share:

- `ffxGetDeviceVK` copies the `VkDeviceContext` into one static of the FidelityFX SDK and
  returns its address; two threads creating contexts at once could build one on the other
  thread's device. Each context now keeps its own `VkDeviceContext`.
- The Vulkan functions the shim forwards for the SDK (and the "hide fp16" flag) were one
  global table, rewritten by every context creation while another one may have been using
  it. Context creation (the only time the SDK calls them) is now serialized by a mutex.
- The shader module tables the shim points the SDK at belonged to one `Fsr` and were freed
  with it while the global pointer stayed; they are process-wide static data now.

None of the three was seen to crash (the windows are short, and the devices identical);
`engine_fsr`'s `contexts_are_created_on_several_devices_at_once` guards them.

## Sources

- mimalloc (MIT), tag `v3.3.2`: `src/init.c` (`_mi_subproc`, `mi_heap_main`,
  `_mi_thread_init_theap_default`, `_mi_is_main_thread`, `_mi_thread_done`),
  `src/theap.c` (`_mi_theap_free`), `include/mimalloc/prim.h` (`_mi_prim_thread_id`,
  dynamic Win32 TLS slots). Branch `dev3` (2026-10-02): `mi_tld_free` invalidates the
  thread id. Issue #1287. No code taken.
- Vulkan-ValidationLayers (Apache-2.0): `scripts/known_good.json` at `vulkan-sdk-1.4.350.0`
  (mimalloc v3.3.2) and at `main` (v3.5.3, 2026-10-02); PR #13158 (an earlier mimalloc
  update reverted for failing CI).
- Vulkan-Loader (Apache-2.0), `loader/loader.c`: layer and ICD libraries are closed in
  `vkDestroyInstance`.
