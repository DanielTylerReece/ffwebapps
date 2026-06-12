# ffwebapps

Run any website as a **native, chromeless desktop app** on Linux — built on
Firefox's first-party "Web Apps" (Taskbar Tabs) infrastructure, with
system-tray integration.

ffwebapps installs websites (PWAs or any site) as standalone apps: their own
window with no tabs or address bar, their own taskbar/dock identity, a
system-tray icon with an unread badge, close-to-tray / run-in-background, and
out-of-scope links that open in your real browser.

It's a CLI-driven fork of [PWAsForFirefox](https://github.com/filips123/PWAsForFirefox)'s
native component, re-architected to drive Firefox's built-in Web Apps support
instead of patching the browser chrome at runtime.

## Features

- **Chromeless window** — no tabs, no address bar; a dark, app-styled titlebar
- **Distinct app identity** — its own Wayland `app_id`, so the dock / taskbar /
  alt-tab treat it as a separate application
- **System tray** — icon with a live **unread badge**, **run-in-background**,
  and **close-to-tray**: the window hides and restores at the exact same
  position and size (no minimize animation, no flicker)
- **Smart link routing** — out-of-scope links open in your **default browser**,
  while the app's own domains and auth/SSO providers stay in-window
- **Lightweight runtime** — symlinks your system Firefox (a few hundred KB), so
  it tracks Firefox updates instead of bundling a second copy
- **Desktop integration** — generates `.desktop` launchers and icons for you

## Requirements

- **Firefox** ≥ 151 (provides the Web Apps / Taskbar Tabs modules; used as the
  linked runtime)
- **Rust / Cargo** to build
- **KDE Plasma** — the tray hide/show and window control currently target KWin
  (`qdbus6`)

## Install

### Arch Linux (PKGBUILD)

```bash
cd packages/arch
makepkg -si
```

### From source

```bash
cargo build --release --bin ffwebapps --bin ffwebapps-tray
# Put target/release/ffwebapps and ffwebapps-tray on your PATH, and copy
# ./userchrome to /usr/share/ffwebapps/userchrome (or point FFPWA_SYSDATA at it).
```

## Usage

One-time setup (link your system Firefox as the runtime):

```bash
ffwebapps runtime install --link
```

Install an app:

```bash
ffwebapps site install <MANIFEST_URL> --document-url <PAGE_URL> --name "App Name"
```

It then appears in your application menu as a chromeless window with a tray icon.

> `<MANIFEST_URL>` is the site's web-app manifest (the `<link rel="manifest" href="…">`
> on the page); `--document-url` is the page itself.

Daily use:

- **Launch** — from your app menu, or `ffwebapps site launch <ULID>`
- **Close → tray** — the window's X hides it to the tray (the app keeps running)
- **Restore / minimize** — single-click the tray icon (toggles)
- **Unread** — shown as a badge on the tray icon
- **External links** — open in your default browser automatically

### Commands

```bash
ffwebapps runtime install [--link] | uninstall | patch

ffwebapps site install <MANIFEST_URL> [--document-url --name --start-url --profile --launch-now …]
ffwebapps site launch <ULID> [--url <URL> | --protocol [<URL>]]
ffwebapps site update <ULID> [--update-manifest --update-icons]
ffwebapps site uninstall <ULID>

ffwebapps profile list            # lists profiles + their apps and ULIDs
ffwebapps profile create | update <ULID> | remove <ULID>
```

Run `ffwebapps <command> --help` for the full flag list.

## Configuration

Per-app preferences live in the app's profile
(`~/.local/share/ffwebapps/profiles/<profile>/`):

- `user.js`
  - `ffwebapps.externalLinks.enabled` — toggle external-link routing
  - `ffwebapps.allowedDomains` — comma-separated wildcard domains kept in-window
- `chrome/userChrome.css` — titlebar colour and chrome tweaks (default `#000`)

## How it works

ffwebapps drives Firefox's first-party **Web Apps (Taskbar Tabs)** feature:

1. The CLI writes a per-app entry into the profile's `taskbartabs.json` registry
   plus a small runtime autoconfig (enables the feature and handles external
   links).
2. It launches the runtime with `firefox -taskbar-tab <id>`, which opens a
   standalone minimal-UI window with a per-app `app_id`.
3. `userChrome.css` strips the remaining toolbar for a chromeless look.
4. A small `ffwebapps-tray` helper (a StatusNotifierItem) shows the icon/badge
   and hides/restores the window via KWin.

No browser-chrome monkeypatching — it builds on maintained Mozilla code.

## Limitations

- Tray hide/show and window control target **KDE Plasma (KWin)**; on other
  desktops the icon appears but hide/restore won't work yet.
- No drop shadow on the chromeless window (a Wayland CSD limitation).
- CLI-only: you supply the manifest URL (there is no browser extension for
  auto-discovery).

## Credits & License

A fork of [PWAsForFirefox](https://github.com/filips123/PWAsForFirefox) by
Filip Š. Licensed under **MPL-2.0** — see [`LICENSE`](LICENSE).
