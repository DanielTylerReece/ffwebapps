//! System tray helper for ffwebapps web apps (Linux / StatusNotifierItem).
//!
//! Shows a tray icon with an unread-count badge for a running web app. Closing
//! the web app window is intercepted by the runtime and turned into a hide
//! request (a `.hide` sentinel) which we honour by hiding the window from the
//! taskbar via the window manager (KWin). A single click toggles the window
//! between hidden and shown. Using the window manager (rather than unmapping the
//! window) preserves the surface, so there is no resize flicker.
//!
//! Lifecycle: one tray per web app (de-duplicated by a pidfile). The tray exits
//! when its runtime exits, and it re-registers itself if the StatusNotifier host
//! (e.g. plasmashell) restarts, so it never silently disappears while the app is
//! running. "Quit" terminates the runtime itself, not just this helper.

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ffwebapps-tray is only supported on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use ksni::menu::StandardItem;
    use ksni::{Handle, MenuItem, Tray, TrayService};

    fn rt_dir() -> String {
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
    }

    fn tray_pidfile(id: &str) -> PathBuf {
        PathBuf::from(format!("{}/ffwebapps-tray-{id}.pid", rt_dir()))
    }

    fn log(msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/ffwebapps-tray.log", rt_dir()))
        {
            let _ = writeln!(f, "{msg}");
        }
    }

    struct Options {
        id: String,
        name: String,
        icon: String,
        wmclass: String,
        exec: String,
        unread_file: PathBuf,
        hide_file: PathBuf,
        show_file: PathBuf,
        hidden_file: PathBuf,
        running_file: PathBuf,
    }

    fn parse_args() -> Options {
        let mut id = String::new();
        let mut name = String::from("Web App");
        let mut icon = String::from("applications-internet");
        let mut wmclass = String::new();
        let mut exec = String::new();

        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let mut value = || args.next().unwrap_or_default();
            match flag.as_str() {
                "--id" => id = value(),
                "--name" => name = value(),
                "--icon" => icon = value(),
                "--wmclass" => wmclass = value(),
                "--exec" => exec = value(),
                _ => {}
            }
        }

        let rt = rt_dir();
        Options {
            unread_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.unread")),
            hide_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.hide")),
            show_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.show")),
            hidden_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.hidden")),
            running_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.running")),
            id,
            name,
            icon,
            wmclass,
            exec,
        }
    }

    /// Singleton guard: if a tray for this app id is already alive, return true.
    fn already_running(id: &str) -> bool {
        let pidfile = tray_pidfile(id);
        if let Ok(contents) = fs::read_to_string(&pidfile)
            && let Ok(pid) = contents.trim().parse::<i32>()
            && PathBuf::from(format!("/proc/{pid}")).exists()
        {
            return true;
        }
        let _ = fs::write(&pidfile, std::process::id().to_string());
        false
    }

    fn read_pid(file: &PathBuf) -> Option<i32> {
        fs::read_to_string(file).ok().and_then(|s| s.trim().parse::<i32>().ok())
    }

    fn pid_alive(running_file: &PathBuf) -> bool {
        read_pid(running_file)
            .map(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
            .unwrap_or(false)
    }

    fn read_unread(unread_file: &PathBuf) -> u32 {
        fs::read_to_string(unread_file).ok().and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
    }

    /// Run a KWin script (loads it, runs it, unloads it). Tries qdbus6 then qdbus.
    fn kwin_run(js: &str) {
        let path = format!("{}/ffwebapps-kwin.js", rt_dir());
        if fs::write(&path, js).is_err() {
            return;
        }
        for qb in ["qdbus6", "qdbus"] {
            let out = Command::new(qb)
                .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript", &path])
                .output();
            let o = match out {
                Ok(o) if o.status.success() => o,
                _ => continue,
            };
            let id = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if id.is_empty() {
                continue;
            }
            let _ = Command::new(qb)
                .args(["org.kde.KWin", &format!("/Scripting/Script{id}"), "org.kde.kwin.Script.run"])
                .output();
            let _ = Command::new(qb)
                .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", &path])
                .output();
            return;
        }
    }

    /// Hide from the taskbar (hidden = true), or restore + activate (hidden =
    /// false), the web app window identified by its Wayland app_id.
    fn set_window_hidden(wmclass: &str, hidden: bool) {
        if wmclass.is_empty() {
            return;
        }
        // Hide by moving the window far off-screen (a fixed reversible offset)
        // rather than minimising — this avoids KDE's minimize animation (which
        // slides toward the dock, not the tray) and preserves the exact geometry.
        // The moves are gated on the window's actual position so they are
        // idempotent and self-correcting: only move off-screen if it's currently
        // on-screen and vice versa. This means a stale hidden-state marker can
        // never push a visible window off-screen or pull a hidden one twice.
        let body = if hidden {
            "if(w.frameGeometry.x>-10000){ w.skipTaskbar=true; w.skipSwitcher=true; w.skipPager=true; \
             const g=w.frameGeometry; w.frameGeometry={x:g.x-50000, y:g.y, width:g.width, height:g.height}; }"
        } else {
            "if(w.frameGeometry.x<-10000){ const g=w.frameGeometry; \
             w.frameGeometry={x:g.x+50000, y:g.y, width:g.width, height:g.height}; } \
             w.skipTaskbar=false; w.skipSwitcher=false; w.skipPager=false; w.minimized=false; workspace.activeWindow=w;"
        };
        let js = format!(
            "const l=(workspace.windowList?workspace.windowList():workspace.clientList());\
             for(const w of l){{ if(w.resourceClass=='{wmclass}'){{ {body} }} }}"
        );
        kwin_run(&js);
    }

    fn is_hidden(hidden_file: &PathBuf) -> bool {
        hidden_file.exists()
    }

    /// Hide the window off-screen and record the hidden state. The marker is
    /// file-backed so the state survives a tray restart (e.g. after the
    /// StatusNotifier host restarts).
    fn hide_window(opts: &Options) {
        set_window_hidden(&opts.wmclass, true);
        let _ = fs::write(&opts.hidden_file, "1");
    }

    /// Restore + focus the window (a no-op move if it's already on-screen, so
    /// this doubles as "raise/focus") and clear the hidden state.
    fn show_window(opts: &Options) {
        set_window_hidden(&opts.wmclass, false);
        let _ = fs::remove_file(&opts.hidden_file);
    }

    struct State {
        unread: u32,
    }

    #[derive(Clone)]
    struct AppTray {
        opts: Arc<Options>,
        state: Arc<Mutex<State>>,
    }

    impl AppTray {
        /// Toggle the window (show <-> hide), or relaunch the app if it isn't
        /// running. Singleton-safe: relaunch remotes into the existing instance.
        fn activate_app(&self) {
            if !pid_alive(&self.opts.running_file) {
                log("activate: not running -> launch");
                let _ = Command::new("sh").arg("-c").arg(&self.opts.exec).spawn();
                let _ = fs::remove_file(&self.opts.hidden_file);
                return;
            }
            if is_hidden(&self.opts.hidden_file) {
                log("activate: show");
                show_window(&self.opts);
            } else {
                log("activate: hide");
                hide_window(&self.opts);
            }
        }

        /// Quit the whole app: terminate the runtime, clean up, then exit.
        fn quit_app(&self) {
            log("quit: terminating runtime");
            if let Some(pid) = read_pid(&self.opts.running_file) {
                let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
            }
            cleanup(&self.opts);
            std::process::exit(0);
        }
    }

    impl Tray for AppTray {
        fn icon_name(&self) -> String {
            self.opts.icon.clone()
        }

        fn id(&self) -> String {
            format!("ffwebapps-{}", self.opts.id)
        }

        fn title(&self) -> String {
            self.opts.name.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            let unread = self.state.lock().unwrap().unread;
            let description = if unread > 0 { format!("{unread} unread") } else { String::new() };
            ksni::ToolTip {
                icon_name: self.opts.icon.clone(),
                title: self.opts.name.clone(),
                description,
                icon_pixmap: Vec::new(),
            }
        }

        // Quiet unread badge (no pulsing).
        fn overlay_icon_name(&self) -> String {
            if self.state.lock().unwrap().unread > 0 { "mail-unread".into() } else { String::new() }
        }

        fn status(&self) -> ksni::Status {
            ksni::Status::Active
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            log("ksni activate() (single click)");
            self.activate_app();
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open".into(),
                    icon_name: self.opts.icon.clone(),
                    activate: Box::new(|this: &mut Self| this.activate_app()),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    icon_name: "application-exit".into(),
                    activate: Box::new(|this: &mut Self| this.quit_app()),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Remove this app's runtime sentinels and the tray pidfile.
    fn cleanup(opts: &Options) {
        let _ = fs::remove_file(&opts.unread_file);
        let _ = fs::remove_file(&opts.hide_file);
        let _ = fs::remove_file(&opts.show_file);
        let _ = fs::remove_file(&opts.hidden_file);
        let _ = fs::remove_file(tray_pidfile(&opts.id));
    }

    pub fn run() {
        let opts = Arc::new(parse_args());
        if opts.id.is_empty() {
            eprintln!("ffwebapps-tray: --id is required");
            std::process::exit(1);
        }
        if already_running(&opts.id) {
            return;
        }

        let state = Arc::new(Mutex::new(State { unread: read_unread(&opts.unread_file) }));

        // The current ksni handle is replaced whenever the service is re-created
        // (after a StatusNotifier host restart); the poll thread uses it to push
        // unread-badge refreshes to whichever service generation is live.
        let cur_handle: Arc<Mutex<Option<Handle<AppTray>>>> = Arc::new(Mutex::new(None));

        // Single background thread for the whole process lifetime: honours hide
        // requests, refreshes the unread badge, and ties the tray's lifetime to
        // the runtime (exit when the runtime exits; bail if it never comes up).
        {
            let opts = opts.clone();
            let state = state.clone();
            let cur_handle = cur_handle.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut was_alive = false;
                loop {
                    std::thread::sleep(Duration::from_millis(250));

                    // Lifecycle: exit when the runtime dies (after it came up),
                    // or give up if it never appears within the grace window.
                    if pid_alive(&opts.running_file) {
                        was_alive = true;
                    } else if was_alive {
                        log("runtime exited -> tray quitting");
                        cleanup(&opts);
                        std::process::exit(0);
                    } else if start.elapsed() > Duration::from_secs(20) {
                        log("runtime never started -> tray quitting");
                        cleanup(&opts);
                        std::process::exit(0);
                    }

                    // Honour a hide request from the runtime (window close).
                    if opts.hide_file.exists() {
                        let _ = fs::remove_file(&opts.hide_file);
                        if !is_hidden(&opts.hidden_file) {
                            log("hide request -> hide window");
                            hide_window(&opts);
                        }
                    }
                    // Honour a show/focus request (e.g. a duplicate launch that
                    // was redirected here instead of opening a second window).
                    if opts.show_file.exists() {
                        let _ = fs::remove_file(&opts.show_file);
                        log("show request -> restore/raise window");
                        show_window(&opts);
                    }

                    // Refresh the unread badge if it changed.
                    let current = read_unread(&opts.unread_file);
                    let changed = {
                        let mut st = state.lock().unwrap();
                        let changed = st.unread != current;
                        st.unread = current;
                        changed
                    };
                    if changed && let Some(handle) = cur_handle.lock().unwrap().as_ref() {
                        let _ = handle.update(|_: &mut AppTray| {});
                    }
                }
            });
        }

        // Service (re)generation loop: ksni's run() returns if the StatusNotifier
        // host goes away (e.g. plasmashell restart). Re-register so the icon comes
        // back instead of vanishing. The poll thread exits the process when the
        // runtime dies, which breaks out of this loop.
        loop {
            let tray = AppTray { opts: opts.clone(), state: state.clone() };
            let service = TrayService::new(tray);
            *cur_handle.lock().unwrap() = Some(service.handle());
            let _ = service.run();
            *cur_handle.lock().unwrap() = None;
            log("tray service ended -> re-registering");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
