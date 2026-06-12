use std::io;
use std::io::Write;

use anyhow::{Context, Result, bail};
use cfg_if::cfg_if;
use log::{info, warn};
use ulid::Ulid;
use url::Url;

use crate::components::runtime::Runtime;
use crate::components::site::{Site, SiteConfig};
use crate::console::app::{
    SiteInstallCommand,
    SiteLaunchCommand,
    SiteUninstallCommand,
    SiteUpdateCommand,
};
use crate::console::{Run, store_value, store_value_vec};
use crate::directories::ProjectDirs;
use crate::integrations;
use crate::integrations::{IntegrationInstallArgs, IntegrationUninstallArgs};
use crate::storage::Storage;
use crate::utils::construct_certificates_and_client;

/// If this web app's runtime is already alive — its IPC socket accepts a
/// connection — ask it to show/focus its window and return true. A stale socket
/// file (e.g. after a crash) refuses connections, so this doubles as the
/// is-it-running check: no pidfiles, no PID-reuse hazard.
#[cfg(platform_linux)]
fn runtime_show(id: &Ulid, show: bool) -> bool {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let Ok(mut stream) = UnixStream::connect(format!("{rt}/ffwebapps-{id}.sock")) else {
        return false;
    };
    let msg: &[u8] =
        if show { b"hello v1 launcher\nshow\n" } else { b"hello v1 launcher\n" };
    stream.write_all(msg).is_ok()
}

/// Spawn the tray helper for a web app. It de-duplicates itself per app id, so
/// calling this when a tray is already running is a no-op.
#[cfg(platform_linux)]
fn spawn_tray(dirs: &ProjectDirs, site: &Site) {
    // Locate the tray binary next to the running `ffwebapps` binary — they are
    // always installed together. This is robust regardless of how we were
    // launched: the desktop (.desktop) launcher does NOT set FFPWA_EXECUTABLES,
    // so `dirs.executables` would otherwise fall back to a default dir and the
    // tray would silently fail to start (no tray icon on a menu/taskbar launch).
    let tray_bin = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ffwebapps-tray")))
        .filter(|path| path.exists())
        .unwrap_or_else(|| dirs.executables.join("ffwebapps-tray"));
    let icon = format!("FFPWA-{}", site.ulid);
    let _ = std::process::Command::new(tray_bin)
        .args([
            "--id".to_string(),
            site.ulid.to_string(),
            "--name".to_string(),
            site.name(),
            "--icon".to_string(),
            icon,
        ])
        .spawn();
}

impl Run for SiteLaunchCommand {
    fn run(&self) -> Result<()> {
        let dirs = ProjectDirs::new()?;
        let mut storage = Storage::load(&dirs)?;

        // Ensure the web app has a stable Web Apps (Taskbar Tabs) ID, generating
        // and persisting one for web apps installed before this was introduced.
        if storage.sites.get(&self.id).context("Web app does not exist")?.config.webapp_id.is_none()
        {
            storage.sites.get_mut(&self.id).unwrap().config.webapp_id = Some(uuid::Uuid::new_v4());
            storage.write(&dirs)?;
        }

        let site = storage.sites.get(&self.id).context("Web app does not exist")?;
        let args = if !&self.arguments.is_empty() { &self.arguments } else { &storage.arguments };

        // Singleton: if this web app is already running, never open a second
        // window. A duplicate launch otherwise spawns another taskbar-tab window
        // that single-page apps reject ("open in another window / Use here").
        // Instead, ask the runtime (over its IPC socket) to show/focus the
        // existing window, and make sure a tray is present. Skipped when a
        // specific URL/protocol is given, which legitimately opens or navigates
        // a window.
        #[cfg(platform_linux)]
        {
            let has_target = matches!(&self.protocol, Some(Some(_))) || !self.url.is_empty();
            if !has_target && runtime_show(&self.id, !self.hidden) {
                info!("Web app already running — focusing the existing window");
                spawn_tray(&dirs, site);
                return Ok(());
            }
        }

        #[cfg(platform_macos)]
        {
            if !self.direct_launch {
                integrations::launch(site, &self.url, args)?;
                return Ok(());
            }
        }

        let runtime = Runtime::new(&dirs)?;
        let profile = storage.profiles.get(&site.profile).context("Web app without a profile")?;

        if runtime.version.is_none() {
            bail!("Runtime not installed");
        }

        #[cfg(all(platform_linux, not(feature = "immutable-runtime")))]
        {
            use std::fs::File;
            use std::io::Read;
            use std::path::Path;

            use blake3::{Hash, hash};

            fn hasher<P: AsRef<Path>>(path: P) -> Hash {
                let mut file = File::open(path.as_ref().join("firefox")).unwrap();
                let mut buf = Vec::new();
                let _ = file.read_to_end(&mut buf);

                hash(&buf)
            }

            if storage.config.use_linked_runtime
                && hasher(crate::components::runtime::FFOX) != hasher(&runtime.directory)
            {
                runtime.link()?;
            }
        }

        // Our runtime/profile assets are tiny (a small autoconfig + userChrome.css),
        // so always (re)apply them on launch. This keeps the external-link handling
        // and chromeless styling up to date without a separate patch step.
        let should_patch = true;

        if should_patch {
            #[cfg(not(feature = "immutable-runtime"))]
            runtime.patch(&dirs, Some(site))?;
            profile.patch(&dirs)?;
        }

        // Handle protocol handler URLs
        // See: https://html.spec.whatwg.org/multipage/system-state.html#protocol-handler-invocation
        let handler = if let Some(Some(protocol)) = &self.protocol {
            let scheme = protocol.scheme().to_string();
            let input = urlencoding::encode(protocol.as_str());

            if !site.config.enabled_protocol_handlers.contains(&scheme) {
                bail!("Scheme {} not enabled", scheme);
            }

            let handler: String = site
                .config
                .custom_protocol_handlers
                .iter()
                .find(|handler| handler.protocol == scheme)
                .or_else(|| {
                    site.manifest
                        .protocol_handlers
                        .iter()
                        .find(|handler| handler.protocol == scheme)
                })
                .context(format!("Scheme {scheme} not found"))?
                .to_owned()
                .url
                .try_into()
                .context("Failed to convert protocol handler")?;
            let handler = handler.replacen("%s", &input, 1);
            let handler = Url::parse(&handler).context("Failed to convert protocol handler")?;
            Some(handler)
        } else {
            None
        };

        let url = match handler {
            Some(url) => vec![url],
            None => self.url.to_owned(),
        };

        // Write/update the Web Apps (Taskbar Tabs) registry entry for this app
        let profile_dir = dirs.userdata.join("profiles").join(site.profile.to_string());
        crate::components::taskbartabs::sync_registry(&profile_dir, site)
            .context("Failed to update the Web Apps registry")?;
        crate::components::taskbartabs::write_profile_prefs(&profile_dir, site)
            .context("Failed to write web app preferences")?;

        // Environment passed to the runtime: the stored user variables, plus
        // the start-hidden request honoured by the runtime's autoconfig.
        let mut variables = storage.variables.clone();
        if self.hidden {
            variables.insert("FFWEBAPPS_START_HIDDEN".into(), "1".into());
        }

        info!("Launching the web app");
        cfg_if! {
            if #[cfg(platform_macos)] {
                site.launch(&dirs, &runtime, &storage.config, &url, args, variables)?.wait()?;
            } else {
                site.launch(&dirs, &runtime, &storage.config, &url, args, variables)?;
            }
        }

        // Spawn the tray helper so the app gets a tray icon, an unread badge,
        // and close-to-tray. It de-duplicates itself per web app, so a freshly
        // launched app always ends up with exactly one tray.
        #[cfg(platform_linux)]
        spawn_tray(&dirs, site);

        Ok(())
    }
}

impl Run for SiteInstallCommand {
    fn run(&self) -> Result<()> {
        self._run()?;
        Ok(())
    }
}

impl SiteInstallCommand {
    pub fn _run(&self) -> Result<Ulid> {
        if self.manifest_url.scheme() == "data" && self.document_url.is_none() {
            bail!("The document URL is required when the manifest URL is a data URL");
        }

        let dirs = ProjectDirs::new()?;
        let mut storage = Storage::load(&dirs)?;

        let profile = storage
            .profiles
            .get_mut(&self.profile.unwrap_or_else(Ulid::nil))
            .context("Profile does not exist")?;

        info!("Installing the web app");

        let config = SiteConfig {
            name: self.name.clone(),
            description: self.description.clone(),
            categories: self.categories.clone(),
            keywords: self.keywords.clone(),
            document_url: match &self.document_url {
                Some(url) => url.clone(),
                None => self.manifest_url.join(".")?,
            },
            manifest_url: self.manifest_url.clone(),
            start_url: self.start_url.clone(),
            icon_url: self.icon_url.clone(),
            enabled_url_handlers: vec![],
            enabled_protocol_handlers: vec![],
            custom_protocol_handlers: vec![],
            launch_on_login: self.launch_on_login.unwrap_or(false),
            launch_on_browser: self.launch_on_browser.unwrap_or(false),
            webapp_id: Some(uuid::Uuid::new_v4()),
            external_links: None,
            allowed_domains: vec![],
            hardware_webrtc: self.hardware_webrtc,
            software_rendering: self.software_rendering,
            scheduling: self.scheduling.clone(),
            user_agent: None,
            start_hidden: false,
        };

        let client = construct_certificates_and_client(
            self.client.user_agent.as_deref(),
            &self.client.tls_root_certificates_der,
            &self.client.tls_root_certificates_pem,
            self.client.tls_danger_accept_invalid_certs,
            self.client.tls_danger_accept_invalid_hostnames,
        )?;

        let site = Site::new(profile.ulid, config, &client)?;
        let ulid = site.ulid;

        if self.system_integration {
            info!("Installing system integration");
            integrations::install(&IntegrationInstallArgs {
                site: &site,
                dirs: &dirs,
                client: Some(&client),
                update_manifest: true,
                update_icons: true,
                old_name: None,
            })
            .context("Failed to install system integration")?;
        }

        profile.sites.push(ulid);
        storage.sites.insert(ulid, site);
        storage.write(&dirs)?;

        info!("Web app installed: {ulid}");

        if self.launch_now {
            let command = SiteLaunchCommand {
                id: ulid,
                url: vec![],
                protocol: None,
                arguments: vec![],
                hidden: false,
                #[cfg(platform_macos)]
                direct_launch: false,
            };
            command.run()?;
        }

        Ok(ulid)
    }
}

impl Run for SiteUninstallCommand {
    fn run(&self) -> Result<()> {
        let dirs = ProjectDirs::new()?;
        let mut storage = Storage::load(&dirs)?;

        let site = storage.sites.get(&self.id).context("Web app does not exist")?;

        if !self.quiet {
            warn!("This will remove the web app");
            warn!("Data will NOT be removed, remove them from the app browser");

            print!("Do you want to continue (y/n)? ");
            io::stdout().flush()?;

            let mut confirm = String::new();
            io::stdin().read_line(&mut confirm)?;
            confirm = confirm.trim().into();

            if confirm != "Y" && confirm != "y" {
                info!("Aborting!");
                return Ok(());
            }
        }

        info!("Uninstalling the web app");
        storage
            .profiles
            .get_mut(&site.profile)
            .context("Web app with invalid profile")?
            .sites
            .retain(|id| *id != self.id);
        let site = storage.sites.remove(&self.id);

        if self.system_integration
            && let Some(site) = site
        {
            info!("Uninstalling system integration");
            integrations::uninstall(&IntegrationUninstallArgs { site: &site, dirs: &dirs })
                .context("Failed to uninstall system integration")?;
        }

        storage.write(&dirs)?;

        info!("Web app uninstalled!");
        Ok(())
    }
}

impl Run for SiteUpdateCommand {
    fn run(&self) -> Result<()> {
        let dirs = ProjectDirs::new()?;
        let mut storage = Storage::load(&dirs)?;

        let site = storage.sites.get_mut(&self.id).context("Web app does not exist")?;
        let old_name = site.name();

        info!("Updating the web app");
        store_value!(site.config.name, self.name);
        store_value!(site.config.description, self.description);
        store_value!(site.config.start_url, self.start_url);
        store_value!(site.config.icon_url, self.icon_url);
        store_value_vec!(site.config.categories, self.categories);
        store_value_vec!(site.config.keywords, self.keywords);
        store_value!(site.config.enabled_url_handlers, self.enabled_url_handlers);
        store_value!(site.config.enabled_protocol_handlers, self.enabled_protocol_handlers);
        store_value!(site.config.launch_on_login, self.launch_on_login);
        store_value!(site.config.launch_on_browser, self.launch_on_browser);
        store_value!(site.config.hardware_webrtc, self.hardware_webrtc);
        store_value!(site.config.software_rendering, self.software_rendering);
        store_value!(site.config.scheduling, self.scheduling);
        store_value!(site.config.user_agent, self.user_agent);
        store_value!(site.config.start_hidden, self.start_hidden);

        let client = construct_certificates_and_client(
            self.client.user_agent.as_deref(),
            &self.client.tls_root_certificates_der,
            &self.client.tls_root_certificates_pem,
            self.client.tls_danger_accept_invalid_certs,
            self.client.tls_danger_accept_invalid_hostnames,
        )?;

        if self.update_manifest {
            site.update(&client).context("Failed to update web app manifest")?;
        }

        if self.system_integration {
            info!("Updating system integration");
            integrations::install(&IntegrationInstallArgs {
                site,
                dirs: &dirs,
                client: Some(&client),
                update_manifest: self.update_manifest,
                update_icons: self.update_icons,
                old_name: Some(&old_name),
            })
            .context("Failed to update system integration")?;
        }

        storage.write(&dirs)?;

        info!("Web app updated!");
        Ok(())
    }
}
