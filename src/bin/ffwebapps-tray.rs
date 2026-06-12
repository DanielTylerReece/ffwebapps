//! System tray helper for ffwebapps web apps (Linux / StatusNotifierItem).
//!
//! A thin remote control for a running web app. The app's Firefox runtime owns
//! its window and its lifecycle: it serves a per-app Unix domain socket
//! ($XDG_RUNTIME_DIR/ffwebapps-<ULID>.sock) and hides or shows its window by
//! (un)mapping it — the standard, compositor-agnostic mechanism on Wayland and
//! X11 alike. This helper only registers a StatusNotifierItem icon (the
//! freedesktop tray standard: Plasma, waybar, XFCE, LXQt, GNOME via extension)
//! with an unread badge, and forwards toggle/quit commands over the socket.
//!
//! Lifecycle: one tray per web app (de-duplicated by a flock singleton). It
//! holds one persistent socket connection; when the runtime exits, the socket
//! reaches EOF and the tray exits with it. It re-registers itself if the
//! StatusNotifier host (e.g. plasmashell) restarts, so it never silently
//! disappears while the app is running. "Quit" asks the runtime to exit.
//!
//! There is deliberately no code here that can spawn a process, so the tray
//! can never (re)launch anything.

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
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use ksni::menu::StandardItem;
    use ksni::{Handle, MenuItem, Tray, TrayService};

    fn rt_dir() -> String {
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
    }

    fn log(msg: &str) {
        if let Ok(mut f) = OpenOptions::new()
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
    }

    fn parse_args() -> Options {
        let mut id = String::new();
        let mut name = String::from("Web App");
        let mut icon = String::from("applications-internet");

        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let mut value = || args.next().unwrap_or_default();
            match flag.as_str() {
                "--id" => id = value(),
                "--name" => name = value(),
                "--icon" => icon = value(),
                // Accepted for compatibility with older launchers; unused.
                "--wmclass" | "--exec" => {
                    let _ = value();
                }
                _ => {}
            }
        }

        Options { id, name, icon }
    }

    /// Race-free singleton: hold an exclusive advisory lock (`flock`) on a
    /// per-app lock file. Only one tray can hold it at a time; the OS releases it
    /// automatically when the process exits or dies, so there are never stale or
    /// orphaned locks, no reused-PID confusion, and a relaunched app always ends
    /// up with exactly one tray. Returns the held lock file — keep it alive for
    /// the whole process lifetime — or `None` if another tray already owns it.
    fn acquire_singleton(id: &str) -> Option<File> {
        let path = format!("{}/ffwebapps-tray-{id}.lock", rt_dir());
        let file =
            OpenOptions::new().create(true).write(true).truncate(false).open(&path).ok()?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 { Some(file) } else { None }
    }

    /// Connect to the runtime's IPC socket, waiting for the runtime to come up
    /// (we are usually spawned right after Firefox, before it binds the socket).
    fn connect(id: &str) -> Option<UnixStream> {
        let path = format!("{}/ffwebapps-{id}.sock", rt_dir());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match UnixStream::connect(&path) {
                Ok(stream) => return Some(stream),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(_) => return None,
            }
        }
    }

    struct State {
        unread: u32,
        runtime_pid: Option<i32>,
    }

    #[derive(Clone)]
    struct AppTray {
        opts: Arc<Options>,
        state: Arc<Mutex<State>>,
        conn: Arc<Mutex<UnixStream>>,
    }

    impl AppTray {
        /// Send a command to the runtime. If the connection is gone, the reader
        /// thread is already exiting the process; nothing to recover here.
        fn send(&self, cmd: &str) {
            let mut conn = self.conn.lock().unwrap();
            let sent =
                conn.write_all(cmd.as_bytes()).and_then(|()| conn.write_all(b"\n")).is_ok();
            if !sent {
                log(&format!("send {cmd}: connection to runtime lost"));
            }
        }

        /// Toggle the window (show <-> hide). The runtime owns the window and
        /// the hidden state; this only forwards the request — it can never
        /// relaunch anything.
        fn activate_app(&self) {
            log("activate: toggle");
            self.send("toggle");
        }

        /// Quit the whole app: ask the runtime to exit. The reader thread exits
        /// this process as soon as the runtime closes the socket. If the runtime
        /// hangs, force-kill it (a direct syscall, no subprocess) so Quit always
        /// tears the app down — it never survives as a trayless window.
        fn quit_app(&self) {
            log("quit: asking the runtime to quit");
            let pid = self.state.lock().unwrap().runtime_pid;
            self.send("quit");
            std::thread::sleep(Duration::from_secs(5));
            if let Some(pid) = pid {
                log("quit: runtime did not exit in time, sending SIGKILL");
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
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

    pub fn run() {
        let opts = Arc::new(parse_args());
        if opts.id.is_empty() {
            eprintln!("ffwebapps-tray: --id is required");
            std::process::exit(1);
        }
        // Race-free singleton: hold the per-app lock for the whole process
        // lifetime (binding kept in scope until the process exits). If another
        // tray already owns this app, exit immediately — no duplicate, no orphan.
        let _singleton = match acquire_singleton(&opts.id) {
            Some(lock) => lock,
            None => return,
        };

        let Some(stream) = connect(&opts.id) else {
            log("runtime never came up -> tray exiting");
            return;
        };
        let Ok(reader) = stream.try_clone() else { return };
        let conn = Arc::new(Mutex::new(stream));
        {
            let mut conn = conn.lock().unwrap();
            let _ = conn.write_all(b"hello v1 tray\n");
        }

        let state = Arc::new(Mutex::new(State { unread: 0, runtime_pid: None }));

        // The current ksni handle is replaced whenever the service is re-created
        // (after a StatusNotifier host restart); the reader thread uses it to push
        // unread-badge refreshes to whichever service generation is live.
        let cur_handle: Arc<Mutex<Option<Handle<AppTray>>>> = Arc::new(Mutex::new(None));

        // Reader thread: receives the runtime's pid and unread-badge pushes, and
        // ties the tray's lifetime to the runtime — EOF or a read error means the
        // runtime is gone, so the tray exits (the flock releases automatically).
        {
            let state = state.clone();
            let cur_handle = cur_handle.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if let Some(pid) = line.strip_prefix("hello v1 ") {
                        state.lock().unwrap().runtime_pid = pid.trim().parse().ok();
                    } else if let Some(count) = line.strip_prefix("unread ") {
                        let count = count.trim().parse().unwrap_or(0);
                        let changed = {
                            let mut st = state.lock().unwrap();
                            let changed = st.unread != count;
                            st.unread = count;
                            changed
                        };
                        if changed && let Some(handle) = cur_handle.lock().unwrap().as_ref() {
                            handle.update(|_: &mut AppTray| {});
                        }
                    }
                }
                log("runtime closed the connection -> tray exiting");
                std::process::exit(0);
            });
        }

        // Service (re)generation loop: ksni's run() returns if the StatusNotifier
        // host goes away (e.g. plasmashell restart). Re-register so the icon comes
        // back instead of vanishing. The reader thread exits the process when the
        // runtime dies, which breaks out of this loop.
        loop {
            let tray =
                AppTray { opts: opts.clone(), state: state.clone(), conn: conn.clone() };
            let service = TrayService::new(tray);
            *cur_handle.lock().unwrap() = Some(service.handle());
            let _ = service.run();
            *cur_handle.lock().unwrap() = None;
            log("tray service ended -> re-registering");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
