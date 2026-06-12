//! System tray helper for ffwebapps web apps (Linux / StatusNotifierItem).
//!
//! Shows a tray icon with an unread-count badge for a running web app. Closing
//! the web app window is intercepted by the runtime and turned into a hide
//! request (a `.hide` sentinel) which we honour by minimising + hiding the
//! window from the taskbar via the window manager (KWin). A single click toggles
//! the window between hidden and shown. Using the window manager (rather than
//! unmapping the window) preserves the surface, so there is no resize flicker.

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("firefoxpwa-tray is only supported on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use ksni::menu::StandardItem;
    use ksni::{MenuItem, Tray, TrayService};

    fn rt_dir() -> String {
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
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
            running_file: PathBuf::from(format!("{rt}/ffwebapps-{id}.running")),
            id,
            name,
            icon,
            wmclass,
            exec,
        }
    }

    /// Singleton guard: if a tray for this app id is already alive, exit.
    fn already_running(id: &str) -> bool {
        let pidfile = PathBuf::from(format!("{}/ffwebapps-tray-{id}.pid", rt_dir()));
        if let Ok(contents) = fs::read_to_string(&pidfile)
            && let Ok(pid) = contents.trim().parse::<i32>()
            && PathBuf::from(format!("/proc/{pid}")).exists()
        {
            return true;
        }
        let _ = fs::write(&pidfile, std::process::id().to_string());
        false
    }

    fn pid_alive(running_file: &PathBuf) -> bool {
        fs::read_to_string(running_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .map(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
            .unwrap_or(false)
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

    /// Minimise + hide from the taskbar (hidden = true), or restore + activate
    /// (hidden = false), the web app window identified by its Wayland app_id.
    fn set_window_hidden(wmclass: &str, hidden: bool) {
        if wmclass.is_empty() {
            return;
        }
        // Hide by moving the window far off-screen (a fixed reversible offset)
        // rather than minimising — this avoids KDE's minimize animation (which
        // slides toward the dock, not the tray) and preserves the exact geometry.
        let body = if hidden {
            "w.skipTaskbar=true; w.skipSwitcher=true; w.skipPager=true; \
             const g=w.frameGeometry; w.frameGeometry={x:g.x-50000, y:g.y, width:g.width, height:g.height};"
        } else {
            "const g=w.frameGeometry; w.frameGeometry={x:g.x+50000, y:g.y, width:g.width, height:g.height}; \
             w.skipTaskbar=false; w.skipSwitcher=false; w.skipPager=false; workspace.activeWindow=w;"
        };
        let js = format!(
            "const l=(workspace.windowList?workspace.windowList():workspace.clientList());\
             for(const w of l){{ if(w.resourceClass=='{wmclass}'){{ {body} }} }}"
        );
        kwin_run(&js);
    }

    struct AppTray {
        opts: Options,
        unread: u32,
        hidden: bool,
    }

    impl AppTray {
        fn read_unread(&self) -> u32 {
            fs::read_to_string(&self.opts.unread_file)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        }

        fn activate_app(&mut self) {
            if !pid_alive(&self.opts.running_file) {
                log("activate: not running -> launch");
                let _ = Command::new("sh").arg("-c").arg(&self.opts.exec).spawn();
                self.hidden = false;
                return;
            }
            if self.hidden {
                log("activate: show");
                set_window_hidden(&self.opts.wmclass, false);
                self.hidden = false;
            } else {
                log("activate: hide");
                set_window_hidden(&self.opts.wmclass, true);
                self.hidden = true;
            }
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
            let description = if self.unread > 0 {
                format!("{} unread", self.unread)
            } else {
                String::new()
            };
            ksni::ToolTip {
                icon_name: self.opts.icon.clone(),
                title: self.opts.name.clone(),
                description,
                icon_pixmap: Vec::new(),
            }
        }

        // Quiet unread badge (no pulsing).
        fn overlay_icon_name(&self) -> String {
            if self.unread > 0 { "mail-unread".into() } else { String::new() }
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
                    activate: Box::new(|_| std::process::exit(0)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub fn run() {
        let opts = parse_args();
        if opts.id.is_empty() {
            eprintln!("firefoxpwa-tray: --id is required");
            std::process::exit(1);
        }
        if already_running(&opts.id) {
            return;
        }

        let unread = fs::read_to_string(&opts.unread_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);

        let tray = AppTray { opts, unread, hidden: false };
        let service = TrayService::new(tray);
        let handle = service.handle();

        // Honour hide requests from the runtime (window close) and refresh unread.
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(250));
                handle.update(|tray: &mut AppTray| {
                    if tray.opts.hide_file.exists() {
                        let _ = fs::remove_file(&tray.opts.hide_file);
                        if !tray.hidden {
                            log("hide request -> minimise");
                            set_window_hidden(&tray.opts.wmclass, true);
                            tray.hidden = true;
                        }
                    }
                    let current = tray.read_unread();
                    if current != tray.unread {
                        tray.unread = current;
                    }
                });
            }
        });

        let _ = service.run();
    }
}
