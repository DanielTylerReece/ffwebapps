# ffwebapps — Handoff

Status as of 2026-06-12. Repo: `github.com/nine7nine/ffwebapps` (working tree:
`/home/ninez/Claude/ffwebapps`, GitHub clone: `/home/ninez/Github/ffwebapps`).

ffwebapps runs websites as native, chromeless Firefox "Web App" (Taskbar Tab)
windows on Linux, with a system-tray icon, close-to-tray, and out-of-scope links
opening in the default browser.

## Architecture (current, working)

**The runtime owns its window and lifecycle; the tray is a thin remote.**
One IPC channel: Unix socket `$XDG_RUNTIME_DIR/ffwebapps-<ULID>.sock`, served by
the Firefox runtime (privileged JS in `userchrome/runtime/_autoconfig.cfg` via
`nsIServerSocket.initWithFilename`). Newline-delimited protocol:

- client → runtime: `hello v1 tray` / `hello v1 launcher`, `show`, `hide`,
  `toggle`, `quit`
- runtime → client: `hello v1 <pid>` once, then `unread <n>` on change

There are **no sentinel files and no pidfiles**. Runtime alive ⟺ socket accepts.

- **Tray** (`src/bin/ffwebapps-tray.rs`): StatusNotifierItem via `ksni`, flock
  singleton, persistent socket connection. EOF ⇒ runtime gone ⇒ tray exits.
  Click/Open → `toggle`; Quit → `quit` + 5 s `libc::kill(SIGKILL)` fallback.
  **Zero `Command::new` calls — the tray cannot launch anything.**
- **Launcher** (`src/console/site.rs`): singleton = try-connect to the socket;
  if alive, sends `show` directly to the runtime and returns (no second window).
- **Hide/show** (runtime JS): on KWin, the window is moved off-screen (gated,
  idempotent ±50000 move) with skipTaskbar/skipSwitcher/skipPager — preserves
  exact geometry/position (this is the long-debugged, proven mechanism).
  Non-KWin fallback: unmap via `nsIBaseWindow.visibility` (hides everywhere;
  re-show placement is then up to the compositor).
- **Close-to-tray**: window close is intercepted (close event, WindowIsClosing,
  win.close) and turned into a hide **only while a tray client is connected**;
  otherwise it closes for real. Quit sets an in-memory flag — no `.quit` file.
- **KWin rules** (`src/integrations/implementation/linux.rs`): on KDE, install
  writes a per-app `positionrule=4` (Remember) rule to `~/.config/kwinrulesrc`.
  NOTE: inert in practice — see gotchas. Candidate for removal.

## Hard-won gotchas (do not re-learn these)

1. **`nsIBaseWindow.visibility` cannot be read back** — `AppWindow::GetVisibility`
   hardcodes `true` (Mozilla bug 306245). Track hidden state yourself or toggle
   will always pick "hide".
2. **Unmap/remap loses window position on KWin Wayland.** A remapped toplevel is
   a NEW window to the compositor; KWin re-places it (verified empirically:
   500,300 → remap → 1165,811). KWin's "Remember" position rules apply a stored
   value on map but were never observed to *capture* one on unmap — rules stayed
   value-less across many hide/show cycles. Hence the off-screen mechanism.
3. **Mixed-binary versions are the root of "app relaunches after quit" class
   bugs.** Always: install to /usr via PKGBUILD, regenerate launchers, kill
   strays. The `.desktop` Exec lines must NOT point at `target/debug`.
4. `ffwebapps site update <ULID> --no-manifest-updates --no-icon-updates`
   regenerates launchers without network (Teams' manifest fetch can fail and
   abort scripts).
5. `pkill -f` with literals like `ffwebapps-tray` matches your own shell — kill
   by `/proc/<pid>/exe` path instead.

## Install / test

PKGBUILD: `packages/arch/` (installs `/usr/bin/ffwebapps{,-tray}` +
`/usr/share/ffwebapps/userchrome`). After install: `site update` each app
(rewrites `.desktop` to /usr/bin and installs KWin rules), kill strays, clear
`$XDG_RUNTIME_DIR/ffwebapps-*`. Apps (ULID): Teams
`01KTVNB6PT0YSDPX9X083P5Z6N`, WhatsApp `01KTVNKW9TNPEH9Z3C3X63EPQ3`, Bitwarden
`01KTWY4T6CTRV0SEAVJJ60ASB7`.

Verified working: launch, tray icon, close-to-tray hide, tray toggle show/hide
at exact position, singleton focus-instead-of-duplicate, quit with full
teardown and no relaunch, unread badge.

## Open / deferred

- **Teams SSO re-auth** (deferred by user): MSAL `InteractionRequired`; the
  Sign-in click produces no navigation at all — not an ffwebapps link-handling
  issue. Avenues: popup-blocking check, `display-mode: standalone` detection
  (Teams saw `isPwa:false`), compare in stock Firefox.
- Cleanup candidates: remove the inert KWin Remember rules from linux.rs (and
  uninstall path), or replace with something that actually captures position.
- `core.262809` in the repo root is an old core dump — untracked, delete at will.
