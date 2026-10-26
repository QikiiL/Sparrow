# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Sparrow / 剪雀** — a system clipboard manager built with Tauri 2 + React 19 + TypeScript + Vite. UI is in Chinese. Migrated from a C# WPF version. **Windows-only** (uses clipboard-win/winreg; file capture/paste via CF_HDROP). Current version **1.1.1**; main binary `sparrow.exe`, installer `sparrow-setup.exe`. The product was called **Clipboard 0.x** before the 1.0.0 rename (repo dir still `clipboard-manager-tauri`).

**Portable layout (XDG-style), everything next to the exe** (`lib.rs::exe_dir`): `config/` holds settings.json + storage pointer + window-state.json (via `app_config_dir()`), `data/` holds clipboard.db + images/ (via `app_data_dir_default()`). Installer default install dir is `%LOCALAPPDATA%\Sparrow`, but **0.x installs are upgraded in place**: `installer.nsi::DetectLegacyInstallDir` reads the 0.x uninstall entry (`…\Uninstall\clipboard`) `InstallLocation` (SHCTX → HKCU → HKLM fallback, outer quotes stripped, dir must contain `clipboard-manager-tauri.exe` or `uninstall.exe`) and reuses that path — 0.x let the user pick any dir, e.g. `D:\clipboard`. Detection also forces passive mode (directory/reinstall pages skipped, app auto-launched). Only when the user picks a *different* dir does the installer copy the old `config/` + `data/` over (never overwriting), and `CLEANUP_LEGACY_INSTALL` afterwards deletes the 0.x uninstall key, `Software\lyz\clipboard`, the `clipboard*.lnk` shortcuts and `$INSTDIR\clipboard-manager-tauri.exe`. First launch still migrates from the legacy `%APPDATA%/com.lyz.clipboard-manager-tauri` dir (`migrate_legacy_layout`). Uninstalling does **not** delete user data: `NSIS_HOOK_PREUNINSTALL` moves `config/` and `data/` to the sibling `sparrow-userdata` dir (outside `$INSTDIR`, so the uninstaller's `RMDir /r` cannot take it along) and leaves them for the user to keep or delete.

### Naming & compatibility (1.0.0 rename)

Kept **unchanged on purpose** — changing any of these breaks existing installs or the SMS-capture chain:

| Item | Value | Why it stays |
| --- | --- | --- |
| `identifier` | `com.lyz.clipboard-manager-tauri` | Asset-protocol scope, `%APPDATA%` legacy dir name, `migrate_legacy_layout` source |
| MSIX package `Name` / `Publisher` | `ClipboardManagerIdentity` / `CN=ClipboardManager` | PFN hash `z0fnhbkv2vcxr` is derived from them |
| `APP_AUMID` / `HELPER_AUMID` | `ClipboardManagerIdentity_z0fnhbkv2vcxr!ClipboardManagerApp` / `…!SmsHelper` | Shell activation of the sparse package |
| Legacy migration paths | `%APPDATA%\com.lyz.clipboard-manager-tauri`, `%LOCALAPPDATA%\clipboard`, registry `…\Uninstall\clipboard\InstallLocation` | Old users' settings / data (0.x install dir is whatever the registry says, e.g. `D:\clipboard`) |
| GitHub repo | `QikiiL/Sparrow`（原 `QikiiL/Clipboard`，1.0.0 改名） | `update.rs` raw + jsdelivr URLs、`version.json`、README 均已指向新名；旧地址由 GitHub 重定向，仅作 0.x 更新兼容入口 |

Renamed in 1.0.0: `productName` → `Sparrow` (NSIS output `Sparrow_<ver>_x64-setup.exe`, install dir, shortcut names, uninstall `DisplayName`), `mainBinaryName` / Cargo package / npm name → `sparrow`, MSIX `DisplayName` → `剪雀` / `剪雀短信助手` (`Description`: `Sparrow 剪贴板管理器` / `Sparrow SMS helper`; a manifest edit means rebuilding the `.msix` with `build-identity.ps1` **and re-signing**; PFN itself is unaffected), autostart scheduled task → `SparrowAutostart`, uninstall backup dir → `sparrow-userdata`, NSIS hooks `taskkill` target → `sparrow.exe`.

> All of the above is already implemented. `settings_service.rs` uses `SparrowAutostart` and deletes the 0.x task `ClipboardManagerAutostart` (`LEGACY_AUTOSTART_TASK`) at startup; `nsis-hooks.nsh` sets `USERDATA_BACKUP = sparrow-userdata` and `KILL_RUNNING_APP` best-effort kills the 0.x image `clipboard-manager-tauri.exe` first, then `sparrow.exe`, then `sms-helper.exe`. The 0.x → 1.0.0 takeover prefers the registry-detected `$LegacyInstallDir` over the `LEGACY_INSTDIR = %LOCALAPPDATA%\clipboard` fallback, skips copying entirely on an in-place upgrade (`$INSTDIR == $LegacyInstallDir`), only copies `config`/`data` when the new dir is still empty, never runs the old uninstaller, and `CLEANUP_LEGACY_INSTALL` removes the 0.x uninstall key / `Software\lyz\clipboard` / old shortcuts / old exe (exact names only, best-effort).

## Build Commands

```bash
# Full app (from project root)
npm run tauri dev          # Vite + Rust with hot reload
npm run tauri build        # Production build

# Frontend only
npm run dev                # Vite dev server (port 1420)
npm run build              # tsc + vite build
npm test                   # vitest run (single pass)

# Rust only (from src-tauri/)
cargo check                # Type check
cargo clippy               # Lint
cargo test                 # Unit tests
```

Note: the default toolchain is MSVC (`stable-x86_64-pc-windows-msvc`), and `cl.exe`/`link.exe` are installed. `src-tauri/.cargo/config.toml` already pins the MSVC linker plus `CC`/`CXX`/`AR`/`INCLUDE`/`LIB`, so plain `cargo` works from any shell — no need to run `vcvars64.bat` manually (the `vcvars` calls in `dev.bat`/`build-release.bat` are legacy leftovers, harmless to keep). Do NOT use `cargo +stable-x86_64-pc-windows-gnu`.

## Architecture

**Two IPC mechanisms connect frontend to backend:**

1. **Tauri Commands** (`invoke()` from frontend): `activate_item`, `delete_item`, `toggle_favorite`, `pause_monitoring`, `get_image_base64`, `save_settings`, `load_settings`, `show_window`, `hide_window`, `close_app`, `register_hotkey`, `get_close_behavior`, `set_close_behavior`, `enable_win_v_integration`, `disable_win_v_integration`, `set_always_on_top`, `set_keep_open`, `get_paste_mode`, `set_paste_mode`, `get_db_path`, `create_group`, `update_group`, `delete_group`, `set_item_group`

2. **Tauri Events** (`listen()` in frontend): `clipboard-changed`, `item-deleted`, `monitoring-paused`, `groups-changed`, `ask-close-behavior`, `paste-mode-changed`

**Dual SQLite connections** to the same file (`data/clipboard.db` next to the exe — `app_data_dir_default()`; relocatable, see the storage pointer below):
- Frontend: `@tauri-apps/plugin-sql` for SELECT queries (`src/lib/db.ts`, read-only helper `queryItems`)
- Backend: `sqlx::SqlitePool` for INSERT/UPDATE/DELETE (`src-tauri/src/lib.rs`)
- WAL mode + busy_timeout=5000 set at startup to prevent lock contention

## Key Files

**Rust (`src-tauri/src/`):**
- `lib.rs` — App setup: plugins, tray, monitor, hotkey, cleanup, command registration; `--minimized` startup flag handling; programmatic main-window creation + app keep-alive (see "hide = destroy" pattern below)
- `utils/window_manager.rs` — Window lifecycle: `create_main_window` (config parity + geometry/pinned restore + close-request wiring), `destroy_main_window` (persists geometry, kills WebView2), `show_or_create` (all summon paths; background-thread rebuild with `CREATING` reentry guard)
- `services/clipboard_monitor.rs` — Polls clipboard every 500ms via arboard + clipboard-win (CF_HDROP file lists, CF_DIB fallback), SHA-256 dedup, emits events
- `services/paste_service.rs` — Clipboard write (arboard / clipboard-win CF_HDROP) + Ctrl+V simulation (enigo)
- `services/settings_service.rs` — Settings via tauri-plugin-store
- `services/tray_service.rs` — System tray menu (pause state is persisted to settings)
- `services/storage_service.rs` — Data location: `resolve_data_dir()` at startup (bootstrap `storage.json` in the default config dir may point to a custom dir), two-phase migration (write pointer → restart → cold-start copy + rewrite absolute image paths in DB), `change_storage_location`/`reset_storage_location` commands
- `services/image_cleanup.rs` — Safe deletion of item image files (only inside app images dir)
- `commands/` — All Tauri command handlers (clipboard / groups / settings / window / db_path)
- `migrations/` — SQLite schema (items + groups tables)

**React (`src/`):**
- `App.tsx` — Root: ThemeProvider + SearchBar + GroupTabs + ClipboardList + StatusBar + SettingsPanel
- `stores/clipboardStore.ts` — Zustand store (items, groups, filters, UI state, maxItems)
- `hooks/useClipboardListener.ts` — Listens for events, reloads items from DB
- `hooks/useDatabase.ts` — Initial data/settings load on mount; reloads groups on `groups-changed`
- `lib/db.ts` — Database access layer with `queryItems()` (read-only)
- `lib/queryBuilder.ts` — Dynamic SQL builder (search with LIKE ESCAPE, group/favorites filters, LIMIT from settings)

## Key Patterns

- **Hide = destroy, show = recreate**: hiding the main window destroys it entirely — the whole `msedgewebview2.exe` tree exits and memory returns to the OS. Summon paths (hotkey toggle, tray, `show_window`) all go through `window_manager::show_or_create`, which recreates the window (~1s) and restores geometry from `window-state.json` (a standalone file written live by Moved/Resized events — NOT stored in AppSettings, because the settings panel round-trips the whole settings object on every instant-save and would null unknown fields) + pinned state from settings. The window is created hidden and shown on `PageLoadEvent::Finished` to avoid a white flash. The app runs headless after the last window dies: `lib.rs` handles `RunEvent::ExitRequested { code: None }` with `api.prevent_exit()`; real exits must call `app.exit(0)` explicitly (tray quit, `close_app`, `CloseBehavior::Close`). The window is NOT declared in `tauri.conf.json` — `create_main_window` is the single creation path. Direct WebView2 API calls behind Tauri's back (manual `controller.Close()`, `TrySuspend`, `SetIsVisible`) are forbidden — wry owns the controller; that class of hack caused white screens/dead input before
- **Smart click (activate_item)**: single-clicking an item defaults to PASTE. The paste target is frozen **at click time** — `utils/foreground_tracker.rs` tracks the last *external* foreground window (`SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`, 40ms polling fallback) in **three states** (`Unknown` / `NoTarget` / `Window(hwnd)`), and `foreground_tracker::freeze_paste_target` resolves them as: tracked valid window → hwnd captured when the hotkey showed the panel (`utils/input_focus.rs`, only for `Unknown`, i.e. cold start) → 0; a `NoTarget` record (user clicked the desktop/taskbar/task view, or the recorded window is gone/cloaked) or a stale tracked window yields **`None` = copy only** (no window is activated at all, so the keep-open Z-order fallback can never turn it into "paste into a random window") and **never** falls back to the captured hwnd — otherwise "click desktop, then click an item" would yank the previous app back to the foreground; `Unknown` with no capture yields `Some(0)`, which is what keeps the old tray/cold-start paths (Z-order fallback / implicit activation) alive. That ordering matters because the panel is topmost by default and does not hide on focus loss, so "summon panel → click another app's input field → come back and click an item" is normal usage. The frozen value is passed by parameter only (never written back into `PasteMode`). The window is then destroyed and foreground restored to the frozen target (`SetForegroundWindow` + poll; minimized targets get `SW_RESTORE` first), then Ctrl+V is simulated — but only after `input_focus::wait_for_paste_target` confirms the foreground really is the target; otherwise the clipboard is still written and **no** keys are sent ("copy only"). Text items skip the clipboard write when the clipboard already holds identical content. "Copy only" mode is a manual toggle in the StatusBar (session-sticky); useful where a stray Ctrl+V is unwanted (e.g. Explorer) — it writes the clipboard and never touches focus (the panel still closes unless keep-open is on). Tray/`show_window`-opened panels have no captured target (`clear_paste_target`) and rely on the tracker or the implicit activation a destroy causes. With `keep_open_on_paste` set (toggle button next to 📌 in the header), the window is NOT destroyed — focus goes to the same frozen target, else the first activable non-own-process window below the panel in Z-order (mirrors the implicit activation a destroy would cause; covers startup/tray-opened panels whose target was never captured), and only if both fail does the click degrade to the destroy path / copy-only so Ctrl+V never lands in our own panel; see `docs/KEEP-OPEN-DESIGN.md`
- **Suppress mechanism**: When pasting, `ClipboardMonitor.suppress = true` for 500ms to prevent re-capturing the pasted content
- **Always-on-top**: 📌 button in the header (`set_always_on_top`), persisted as `pinned` in settings
- **Content dedup**: SHA-256 hash checked against DB; existing items only get `last_used_at` refreshed instead of a duplicate insert (copy counts are not tracked)
- **Image handling**: Saved as PNG in app data dir; `file_path` stores absolute path; `content` stores `[图片]`; deleting/cleaning items removes the image file
- **File handling**: Explorer copies captured via CF_HDROP (paths joined by `\n` in `content`); pasting writes CF_HDROP back
- **Event-driven UI**: Frontend never polls; listens for backend events and re-queries DB
- **ClipboardType enum**: 0=Text, 1=Link, 2=Image, 3=File
- **Paused state**: persisted in settings, applied to monitor at startup; tray toggle and settings panel both persist

## Configuration

- **Global hotkey**: Ctrl+Shift+V (configurable); optional Win+V integration (requires admin, HKLM registry)
- **Clipboard polling**: 500ms interval
- **Cleanup task**: Hourly, deletes expired items (retention_days) and excess (max_item_count), skips favorites, removes orphaned image files
- **Autostart**: Windows Task Scheduler task `SparrowAutostart` (`schtasks`, `/RL HIGHEST`) with `--minimized` (app starts hidden in tray). Not the HKCU Run key — an elevated app (`requireAdministrator`) is silently skipped at logon; `lib.rs` self-heals the task if it disappears. 0.x used the task name `ClipboardManagerAutostart`.
- **Settings**: JSON file via tauri-plugin-store (not SQLite); newly added fields use `#[serde(default)]` for backward compat
- **Dark mode**: Tailwind `class` strategy, persisted in localStorage

## Common Pitfalls

- WAL mode is required for dual-connection pattern; without it, SQLITE_BUSY errors occur
- The 500ms suppress window in `paste_item` must be long enough for the paste to complete
- `enigo` injects Ctrl+V via SendInput, which cannot cross Windows integrity levels (UIPI): pasting into an elevated window requires the app itself to run elevated, which is why the manifest is `requireAdministrator`
- Image `file_path` uses platform-native separators; DB URL converts to forward slashes
- SQLite `datetime('now')` stores UTC; the frontend must parse it with a `Z` suffix or timestamps skew by the local offset
- LIKE queries must pair `escapeLike()` with an `ESCAPE '\'` clause (see `queryBuilder.ts`)
- clipboard-win `set_clipboard()` takes data by value and is incompatible with `FileList`'s `Setter<[T]>` impl — hold a `Clipboard` guard and call `raw::set_file_list_with(.., DoClear)` instead; arboard calls must happen with the guard dropped
- Frontend `db.ts` calls `invoke('get_db_path')` to get the absolute path — don't hardcode `sqlite:clipboard.db`
- ALL db/images paths must resolve through `storage_service::current_data_dir`/`images_dir` — never hardcode `app_config_dir`/`app_data_dir`; the user may have moved data to a custom dir. Migration only happens at cold start (no open SQLite connections); never copy `clipboard.db` while the app is running. The resolved images dir must also be granted at startup via `asset_protocol_scope().allow_directory()` — frontend thumbnails load through the asset protocol (`convertFileSrc`), whose config scope only covers the default `$APPDATA/images/*`
- `std::fs::canonicalize` on Windows returns `\\?\`-prefixed extended paths — NEVER persist them (storage.json, DB columns) or build `sqlite:` URLs from them (`sqlite://?/D:/...` crashes the backend pool at startup). Use `storage_service::strip_extended_prefix`; the backend pool must connect via `SqliteConnectOptions::filename(path)`, not a URL string
- `build.rs` links `app.manifest` (`requestedExecutionLevel=requireAdministrator`) only into the main bin (`rustc-link-arg-bin`, with `new_without_app_manifest()` suppressing the default manifest), so the test bins inherit the elevation requirement and fail to launch without an elevated shell (`cargo test` -> bin target: os error 740, "the requested operation requires elevation"). `cargo test --lib` is unaffected and runs directly (90 passed / 4 ignored)

## Development Workflow

When using `/dw` command, follow this workflow:
1. Use Agent tool for ALL implementation work
2. Use Skill tool with `skill="requesting-code-review"` for code review after implementation
3. Use Skill tool with `skill="verification-before-completion"` for final verification
4. Run tasks in parallel when possible
5. NEVER implement code directly — always dispatch subagents

## Karpathy Guidelines

Behavioral guidelines to reduce common LLM coding mistakes, derived from [Andrej Karpathy's observations](https://x.com/karpathy/status/2015883857489522876) on LLM coding pitfalls.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.
