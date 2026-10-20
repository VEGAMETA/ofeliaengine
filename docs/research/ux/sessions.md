# Autosave and the session

Checked: 2026-10-20. Products were studied as products (behaviour, documentation).
VS Code (MIT) and Godot (MIT) were read for ideas; Blender (GPL-2.0-or-later) for ideas
only. No code was carried over from any of them. Input for
[ADR 0107](../../adr/0107-editor.md) §4, phase E1-5b. CLAUDE.md §12: autosave and crash
recovery; nothing blocks the UI.

Fixed by ADR 0107 §4, not reopened here: autosave after 30 s with unsaved changes, before
Play and before a restart, into `.ofelia/`, never over the user's file (it changes only
on Ctrl+S); the session (documents with unsaved changes, cameras, selection, layout,
panels) comes back without a dialog, as VS Code's hot exit; no undo across a restart.

What exists: `save` writes a scene's text beside its file (`.scene.partial`), then
renames it over the file. `History::head` against `saved_head` gives the unsaved state
(the `*`). `say` puts a message in the status bar for 4 s.

## Sources

| Product | Version or date checked | Link | License where code was read |
|---|---|---|---|
| VS Code docs: Hot Exit, Auto Save | page of 2026-10-14, checked 2026-10-20 | [Basic editing](https://code.visualstudio.com/docs/editing/codebasics) | — |
| VS Code source: `workingCopyBackupTracker.ts`, `workingCopyBackupService.ts`, `textFileEditorModel.ts`, `textFileSaveErrorHandler.ts`, `files.contribution.ts`, `commandsQuickAccess.ts` | `main`, 2026-10-20 | [microsoft/vscode](https://github.com/microsoft/vscode/tree/main/src/vs/workbench/services/workingCopy) | MIT, ideas |
| Blender manual: Recovering Data; Preferences: Save & Load, File Paths | 5.2 LTS, 2026-10-20 | [Recovering Data](https://docs.blender.org/manual/en/latest/troubleshooting/recover.html) | — |
| Blender source: `wm_files.cc` (autosave), `wm_init_exit.cc` (`quit.blend`) | `main`, 2026-10-20 | [blender/blender](https://github.com/blender/blender/blob/main/source/blender/windowmanager/intern/wm_files.cc) | GPL-2.0-or-later: ideas only, no code carried over |
| Unreal Engine: Editor Preferences (Auto Save); API: `UEditorLoadingSavingSettings`, `EAutoSaveMethod`, `EPackageAutoSaveType`, `IPackageAutoSaver`, `FPackageAutoSaverScopeGuard`, `ERestoreOpenAssetTabsMethod` | 5.8, 2026-10-20 | [UEditorLoadingSavingSettings](https://dev.epicgames.com/documentation/en-us/unreal-engine/API/Editor/UnrealEd/UEditorLoadingSavingSettings) | not read (product) |
| Epic forums, "Improved Autosave" | posts of 2015, read 2026-10-20 | [thread](https://forums.unrealengine.com/t/improved-autosave/27418) | — |
| Godot docs: EditorSettings, Project Manager (recovery mode), version control | 4.7, 2026-10-20 | [EditorSettings](https://docs.godotengine.org/en/stable/classes/class_editorsettings.html), [recovery mode](https://docs.godotengine.org/en/stable/tutorials/editor/project_manager.html) | — |
| Godot source: `editor/editor_node.cpp`, `editor/scene/3d/node_3d_editor_plugin.cpp` and `node_3d_editor_viewport.cpp`, `editor/file_system/editor_paths.cpp` | `master`, 2026-10-20 | [godotengine/godot](https://github.com/godotengine/godot/blob/master/editor/editor_node.cpp) | MIT, ideas |
| Unity: issue tracker, release notes 6000.5.0f1 (UUM-142761), Unity Discussions (2019–2026); no manual page covers the backup | 6000.5, 2026-10-20 | [release notes](https://unity.com/releases/editor/whats-new/6000.5.0f1), [backup issue](https://issuetracker.unity3d.com/issues/editorscenemanager-dot-backupscenes-saves-non-dirty-scenes-when-entering-play-mode), [recovery thread](https://discussions.unity.com/t/how-to-disable-scene-backup-recovery/1713414) | — |
| JetBrains IntelliJ IDEA help: Save and revert changes, Local History, File cache conflict | 2026.2, 2026-10-20 | [Local History](https://www.jetbrains.com/help/idea/local-history.html), [conflict](https://www.jetbrains.com/help/idea/file-cache-conflict.html) | — |
| Ours: ADR 0104 (save games), ADR 0108's scene bench, `docs/perf/history.md`, `engine_editor` | 2026-10-20 | [ADR 0104](../../adr/0104-save-games.md), [ADR 0108](../../adr/0108-scene-format-2.md) | — |

## VS Code: hot exit

- **What saves:** a backup of each editor with unsaved changes, 1 s after the last
  change (2 s with a short auto save delay); each change pushes it back. Backups are
  made whatever `files.hotExit` says. `files.autoSave` (off by default) writes the file.
- **Where:** `%APPDATA%\Code\Backups\<workspace>\<scheme>\<hash of the path>`. The first
  line holds the URI and what the file on disk was (`mtime`, `ctime`, `size`, `etag`),
  then the text. A backup is deleted once its editor has no unsaved changes.
- **What comes back:** after a crash or a normal exit, every editor with its unsaved
  changes, untitled ones too, and nothing is asked (`files.hotExit` is `onExit` by
  default; `off` asks Save, Don't Save or Cancel).
- **The file changed on disk:** a restored editor keeps the backup's `etag`. Saving over
  a changed file fails with a notification, not a dialog ("The content of the file is
  newer. Please compare your version with the file contents…"): Compare or Overwrite.
- **Cost:** few writes while typing, from a text snapshot, asynchronous but in place: a
  backup "can turn empty because it always first gets truncated and then written to".

## Blender: auto save and `quit.blend`

- **What saves:** a timer, every 2 minutes by default. In the source it skips a file
  without unsaved changes, tries again in 10 ms while a modal operator runs (a drag, a
  transform), and restarts after the write, "in case file write takes a long time".
- **Where:** the temporary directory, as `<name>_<pid>_autosave.blend`, a regular
  compressed `.blend`. A clean exit writes the session to `quit.blend` and deletes the
  autosave. A save by hand keeps the previous file as `.blend1`.
- **What comes back:** nothing by itself. File > Recover > Last Session opens
  `quit.blend`; File > Recover > Auto Save browses the temporary directory, whose files
  "may be deleted when you reboot". Save Prompt asks before closing with unsaved changes.
  Nothing compares a recovered file with the one on disk: the person picks by hand.
- **Cost:** a full write from the window manager's timer on the main thread, hence the
  wait for modal operators.

## Unreal: auto save and package restore

- **What saves:** every `AutoSaveTimeMinutes`, maps and content packages each on a
  switch, not before `AutoSaveInteractionDelayInSeconds` after "the last user
  interactions", after a warning shown `AutoSaveWarningInSeconds` ahead. Code can
  suspend it (`SuspendAutoSave`, a scope guard: "the auto-saver won't trigger").
- **Where:** `Saved/Autosaves` in the project, with a restore file of what was dirty;
  `AutoSaveMaxBackups` files are kept. `BackupAndOverwrite` (beta) saves in place.
- **What comes back:** after a crash, a prompt offers "to restore any packages that were
  dirty and have auto-saves". After a clean shutdown, asset editors reopen by
  `RestoreOpenAssetTabsOnRestart`: always prompt, always restore, or never. What the
  restore does with a package changed on disk since is not documented.
- **Cost:** an autosave is a full save of each dirty map and asset, hence the warning,
  the delay and the suspension. Users asked for more than "that 10 second notice" and
  for no autosave during shader compilation (forum, 2015).

## Godot 4: editor state, no scene autosave

- **What saves:** no timed autosave of scenes, so no cost. The user's file is saved
  before running (`run/auto_save/save_before_running`) and, if turned on, on focus loss.
- **Where:** `.godot/editor/`, not committed. Per scene,
  `<file>-editstate-<md5 of its path>.cfg`, section `editor_states`: `$selected_nodes`
  and each plugin's state, the 3D editor's with the snapping (`snap_enabled`…) and each
  viewport's camera (`position`, `x_rotation`, `y_rotation`, `distance`…), written when
  the scene is saved or its tab closes; folded rows in a `-folding-` file. The layout,
  `editor_layout.cfg` (docks, `open_scenes`, `current_scene`), 0.5 s after a change.
- **What comes back:** the scenes open last time (`restore_scenes_on_load`), without
  unsaved changes: quitting asks, a crash loses them. After a crash the project usually
  reopens in recovery mode, without plugins, tool scripts, GDExtension or scene restore.
- **The file changed on disk:** a dialog, "The following files are newer on disk: …
  What action should be taken?": "Reload from disk" or "Ignore external changes".

## Unity: a backup when entering Play

- **What saves:** entering Play backs up the open scenes, clean ones too ("BackupScenes
  saves non-dirty scenes when entering Play Mode", Unity's tracker). No timed autosave.
- **Where:** `Temp/__Backupscenes/0.backup`, beside the lock file "indicating that the
  process didn't shutdown properly" (forum, 2026).
- **What comes back:** in older versions nothing by itself: `0.backup` had to be copied
  out before relaunching (forum, 2019). Unity 6 asks at start and puts the scene in
  `Assets/_Recovery` (users' reports). The dialog holds up loading, users say, and it
  came after crashes with nothing unsaved until 6000.5.0f1 cleaned up "SceneBackup files
  when they do not represent unsaved state".
- **The file changed on disk:** not documented. **Cost:** part of entering Play.

## JetBrains IDEs: autosave and Local History

- **What saves:** the user's files, on compiling, running, debugging, version control,
  closing and quitting, and optionally when idle. Nothing needs restoring.
- **Local History** keeps earlier states in the IDE's system directory for "the last 5
  working days"; it "is not a replacement for proper version control".
- **The file changed on disk and in the IDE:** "File cache conflict": Load FS Changes,
  Keep Memory Changes, or Show Difference to merge.

## What we take

| The developer… | What happens |
|---|---|
| works ten minutes without Ctrl+S, and the editor crashes | everything comes back; at most about the last 30 s are lost; the status bar says it was a crash |
| changes Rust code; the editor rebuilds and restarts (E1-5c) | back within a second, as it was |
| closes the editor with unsaved changes | nothing is asked; the next start brings them back |
| presses F5, and native code crashes during Play | the scene as it was at F5 |
| pulls a teammate's change to a scene with unsaved changes | the unsaved version comes back marked; Ctrl+S writes nothing and says why; a command picks a side, and both are kept |

1. **Files in `.ofelia/`:**

   ```text
   session.lock                     held open while an editor runs; left by a crash
   session                          the session (text, versioned)
   autosave/main-41bd9902….scene    scenes/main.scene's unsaved changes (full scene id)
   autosave/main-41bd9902….1.scene  the one before it; .2 before that
   autosave/untitled-<id>.scene     a scene never saved
   ```

   - Named by the file's stem and the scene id: two `main.scene` in different folders
     stay apart, a renamed file keeps its autosaves, and runs do not pile up files
     (Blender names them by process id).
   - An autosave is the text Ctrl+S would write: it opens as a scene, and copying it
     over the file is a recovery by hand. What it was made from is kept in the session
     (4), as VS Code keeps it in a backup's first line: the scene format does not change.
2. **When an autosave is written:**
   - **A deadline, not a debounce:** the first change in neither the file nor an
     autosave (`History::head` differs from both) starts a 30 s clock that later changes
     do not push back, or steady work would never autosave. After the write, the next
     change starts a new clock. An idle editor builds no frames: the deadline wakes it.
   - **At a quiet moment** (Blender's modal check, Unreal's interaction delay): no
     gesture in progress and no input for 1 s. A gesture is a drag (its step still
     merges, and Escape can take it back), typing in a text field, or a held mouse
     button. At the latest 30 s later, it writes when a gesture ends.
   - **Before Play and before a restart:** at once, for every scene with unsaved changes.
   - **Clean again** (Ctrl+S, or undo back to the saved state): the scene's autosaves
     are deleted. Unity's spurious dialogs came from backups that held nothing unsaved.
   - **A guard** (Unreal's scope guard) will hold it off in work over several frames.
3. **Writing:**
   - On the UI thread, only the text: `engine_scene::save_with` into a buffer kept
     between autosaves, handed to the writer and back.
   - One writer thread, as save games have (ADR 0104): `<name>.partial`, flushed
     (`FlushFileBuffers`), renamed over the target. The older autosaves shift (`.1` to
     `.2`, the latest to `.1`) only after the new one is flushed. A rename refused
     because an indexer or an antivirus holds the file is retried.
   - One write in flight per scene; a newer text replaces one still waiting. A failed
     write keeps the older autosaves, and the status bar says why.
4. **When the user's file changed on disk:**
   - The session keeps each document's base: the file's size, last write time and a
     64-bit hash of its bytes (`engine_core::hash::state_hash64`) as last loaded or
     saved, as VS Code keeps `mtime`, `size` and `etag`.
   - At restore, the same size and time bring the autosave back as unsaved changes.
     Otherwise the file is hashed, and a different hash is a conflict (open question 1):
     - nothing is overwritten silently: the file and the autosave are both kept;
     - the tab carries a mark, and a note stays in the status bar until it is resolved:
       "main.scene changed on disk since your unsaved changes of 14:02";
     - "Scene: Take Disk Version" loads the file as one undo step (Ctrl+Z brings the
       unsaved version back) and keeps the autosave as `….conflict.scene`;
     - "Scene: Save Over Disk Version" writes the unsaved version (VS Code's Overwrite);
     - Ctrl+S writes nothing meanwhile and names both commands (VS Code's `askUser`).
   - A file deleted on disk: the autosave comes back, the tab says so, Ctrl+S writes it.
   - While the editor runs (`engine_platform::fs_watch`, E4), the same rules; a scene
     without unsaved changes reloads from disk as one undo step.
5. **Three autosaves per scene,** at least 30 s apart: undo does not survive a restart,
   so the older two are the way back past a mistake made just before a crash or a
   rebuild (Unreal keeps `AutoSaveMaxBackups`; VS Code and Blender, one). "Scene: Restore
   Unsaved Changes…" lists them with their times, as Blender's File > Recover does.
6. **The session file:**
   - per open scene: path, scene id, base and autosave; the editor camera (target, yaw,
     pitch, distance, field of view; Godot keeps the same four); the selection by scene
     ids in pick order, the active one last, and the Shift anchor; folded rows by scene
     id; the tree's and the inspector's scroll and filters; the active tool;
   - for the editor: the tabs and the active one; the panels' sizes and the open bottom
     tab; the palette's last 50 commands by stable name (VS Code's
     `workbench.commandPalette.history`); the snapping default (Y) and its steps;
   - not kept: undo history (fixed), the play world (a restart stops Play), open popups;
   - written with every autosave, when a document opens or closes, at exit and before a
     restart; when only views changed, at most every 10 s at a quiet moment;
   - in the scene grammar (ADR 0108) under `[session version=1]`. Another version or a
     damaged file goes aside as `session.broken`; the restore command lists autosaves.
7. **A clean exit and a crash:**
   - At start the editor creates `.ofelia/session.lock` and holds it open without
     sharing; Windows closes the handle when the process dies, even in a crash. A file
     already there means the last run did not exit cleanly; one held by another process,
     a second editor (open question 5). A restart's new process waits for the old lock.
   - A clean exit (the window closed, a rebuild restart) asks nothing, as hot exit does.
     It writes the last autosaves and the session, flushes them, then deletes the lock
     (not delete-on-close, which a crash would also trigger). Nothing is lost.
   - After a crash the same restore runs, and the status bar says "Restored after a
     crash: unsaved changes of main.scene from 14:02:31". At most the last 30 s and the
     quiet wait are lost; nothing for a crash during Play.
   - A crash during the restore (Godot's recovery mode): the lock says `restoring` until
     10 s after it, then `running`. A start that finds `restoring` opens the user's files
     only, keeps the autosaves, and says so; the restore command tries again.
8. **What to measure** (ref-1, release):
   - the text on the UI thread for the editor's own entities (`Name`, `Transform`,
     `Model`, lights) at 200 and 10,000. The scene bench took 6.60 ms for 10,000 with a
     `Transform`, 7.19 ms with a `Name` too: expect 0.15 and 7 ms or more;
   - the target, no visible hitch: at 200, well inside the 1 ms UI budget (ADR 0107 §8);
     at 10,000, under one 60 Hz frame and only at a quiet moment, at F5 or at a restart.
     Over about 8 ms: open question 7;
   - `SceneRegistry::from_program()`, built at every save today; the writer's write,
     flush and rename, which a restart waits for; the restore (session, base hash,
     autosave loaded: 9.65–11.97 ms for 10,000 today) inside the 1 s back-to-work budget;
   - tests: a crash (the editor dropped without exit); restores over an unchanged, a
     changed and a deleted file; a drag holding the autosave back; a held lock.

## What we do not take

- **A restore prompt at start** (Unreal, Unity): it holds up loading, and in Unity it
  came after crashes with nothing to restore.
- **Recovery by hand** (Blender's File > Recover, Unity's old `0.backup`): people look
  for it after the work is gone.
- **Writing the user's file automatically** (VS Code's `files.autoSave`, Godot's save on
  focus loss and before running, JetBrains, Unreal's `BackupAndOverwrite`): ADR 0107
  rules it out, and half-done work would land in the team's diffs.
- **A countdown** (Unreal): our autosave costs no visible time.
- **Backups rewritten in place** (VS Code), and **the system's temporary directory**
  (Blender), which reboots and cleaners empty.
- **Editor state spread over files per scene and kind** (Godot): one session file.

## Open questions

1. **A conflict at restore** (4):
   - A) the unsaved version comes back marked, and Ctrl+S writes nothing until a command
     picks a side (VS Code). Work goes on where it was; the teammate's change shows only
     after "Take Disk Version".
   - B) the file on disk opens, and the unsaved version in a second tab. The newest file
     is in front, but keeping one's work needs a second tab whose save goes elsewhere.
   - Recommendation: A.
2. **Older autosaves:** three per scene, deleted once it is clean (proposed), or a local
   history across saves (JetBrains' 5 working days), since every rebuild restart loses
   undo. Recommendation: three now; look again after E1-5c.
3. **The quiet moment:** 1 s without input, 30 s more at most (proposed); a tunable.
4. **Per project or per user:** the recent commands, the snapping default and the
   panels' sizes in `.ofelia/session` (like Godot's `.godot/editor/`), or in the user's
   folder (VS Code keeps recent commands per user profile). Recommendation: the project
   now; per-user preferences come with the settings (`:` in the palette).
5. **A second editor on the same project:** refuse it and bring the first one's window
   forward, or let it run without writing the session. Recommendation: refuse.
6. **Ctrl+S through the writer too:** no hitch on big scenes; the `*` goes when the
   write lands, and stays if it fails. Recommendation: yes.
7. **If the text of 10,000 entities shows:** copy the world in the frame and write the
   text behind it, as save games do (ADR 0104: 10,000 entities captured in 0.51 ms). It
   needs a scene writer that works from a copy.
8. **Closing a tab with unsaved changes** (tabs come later): keep its autosave and offer
   "Reopen Closed Scene" instead of asking; `editor-layout.md` cuts confirmation
   dialogs. Recommendation: yes.
