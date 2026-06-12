//! Firefox first-party "Web Apps" (Taskbar Tabs) registry.
//!
//! Instead of patching the browser chrome at runtime, we drive Firefox's
//! built-in Web Apps infrastructure: a per-app entry in
//! `<profile>/taskbartabs/taskbartabs.json` plus launching the runtime with
//! `-taskbar-tab <id>` produces a self-contained app window with its own
//! Wayland `app_id` (`org.mozilla.firefox.webapp-<id>`).
//!
//! The JSON shape must match Firefox's `TaskbarTabs.1.schema.json`, which is
//! validated on every load/save by the browser.

use std::fs::{File, create_dir_all, write};
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use web_app_manifest::types::Url as ManifestUrl;

use crate::components::site::Site;

/// A navigation scope for a Web App: a required hostname and an optional path
/// prefix (matching the Web App Manifest "within scope" algorithm).
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Scope {
    hostname: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    prefix: Option<String>,
}

/// A single registered Web App (Taskbar Tab).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct TaskbarTab {
    id: String,
    scopes: Vec<Scope>,
    user_context_id: u32,
    start_url: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    name: Option<String>,
}

/// The on-disk registry file (`taskbartabs.json`).
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Registry {
    version: u32,
    #[serde(rename = "taskbarTabs")]
    taskbar_tabs: Vec<TaskbarTab>,
}

impl Default for Registry {
    fn default() -> Self {
        Self { version: 1, taskbar_tabs: vec![] }
    }
}

/// Derive the navigation scope (hostname + optional path prefix) from the
/// site's manifest scope.
fn scope_from_site(site: &Site) -> Scope {
    let hostname = site.domain();

    let prefix = if let ManifestUrl::Absolute(url) = &site.manifest.scope {
        let path = url.path();
        if path.is_empty() || path == "/" { None } else { Some(path.to_string()) }
    } else {
        None
    };

    Scope { hostname, prefix }
}

/// Write or update this site's entry in the profile's Web Apps registry so that
/// `firefox -taskbar-tab <webapp_id>` opens it as a standalone app window.
pub fn sync_registry(profile_dir: &Path, site: &Site) -> Result<()> {
    let id = site.config.webapp_id.context("Web app ID is not set")?.to_string();

    let entry = TaskbarTab {
        id: id.clone(),
        scopes: vec![scope_from_site(site)],
        user_context_id: 0,
        start_url: site.url(),
        name: Some(site.name()),
    };

    let directory = profile_dir.join("taskbartabs");
    create_dir_all(&directory).context("Failed to create taskbartabs directory")?;
    let filename = directory.join("taskbartabs.json");

    // Load the existing registry (tolerating a missing or corrupt file).
    let mut registry: Registry = if filename.exists() {
        match File::open(&filename) {
            Ok(file) => serde_json::from_reader(BufReader::new(file)).unwrap_or_default(),
            Err(_) => Registry::default(),
        }
    } else {
        Registry::default()
    };

    // Upsert by ID.
    if let Some(existing) = registry.taskbar_tabs.iter_mut().find(|tab| tab.id == id) {
        *existing = entry;
    } else {
        registry.taskbar_tabs.push(entry);
    }

    let file = File::create(&filename).context("Failed to write taskbartabs registry")?;
    serde_json::to_writer(file, &registry).context("Failed to serialize taskbartabs registry")?;

    Ok(())
}

/// Auth/SSO providers kept in-app for every web app, so logins don't get
/// bounced to the external browser mid-flow.
const AUTH_DOMAINS: &[&str] = &[
    "login.microsoftonline.com",
    "login.microsoft.com",
    "login.live.com",
    "login.windows.net",
    "*.msftauth.net",
    "*.msauth.net",
    "*.b2clogin.com",
    "accounts.google.com",
    "*.okta.com",
    "*.auth0.com",
    "*.duosecurity.com",
    "*.onelogin.com",
];

/// Microsoft 365 service domains, added for Microsoft web apps (e.g. Teams)
/// so the full app experience stays in-window.
const MICROSOFT_DOMAINS: &[&str] = &[
    "*.microsoft.com",
    "*.office.com",
    "*.office.net",
    "*.sharepoint.com",
    "*.microsoftonline.com",
    "*.cloud.microsoft",
    "*.skype.com",
    "*.teams.microsoft.com",
    "*.microsoftonline-p.com",
    "*.azureedge.net",
    "*.sharepointonline.com",
];

/// Derive a sensible default in-app allow-list from a site's scope: the scope
/// host and its parent domain, plus common auth providers, plus the Microsoft
/// 365 bundle for Microsoft apps.
fn default_allowed_domains(site: &Site) -> Vec<String> {
    let host = site.domain();
    let mut domains: Vec<String> = vec![host.clone()];

    // Parent domain wildcard, e.g. `teams.cloud.microsoft` -> `*.cloud.microsoft`
    if let Some((_, parent)) = host.split_once('.')
        && parent.contains('.')
    {
        domains.push(format!("*.{parent}"));
        domains.push(parent.to_string());
    }

    domains.extend(AUTH_DOMAINS.iter().map(|s| s.to_string()));

    let is_microsoft = host.ends_with("microsoft")
        || host.contains(".microsoft.")
        || host.contains("office")
        || host.contains("teams");
    if is_microsoft {
        domains.extend(MICROSOFT_DOMAINS.iter().map(|s| s.to_string()));
    }

    domains.sort_unstable();
    domains.dedup();
    domains
}

/// Write the per-app preferences (`user.js`) that drive the runtime's
/// out-of-scope link handling: whether it is enabled, and which domains stay
/// in-app. The app profile is owned by ffwebapps, so `user.js` is managed here.
pub fn write_profile_prefs(profile_dir: &Path, site: &Site) -> Result<()> {
    let enabled = site.config.external_links.unwrap_or(true);
    let domains = if site.config.allowed_domains.is_empty() {
        default_allowed_domains(site)
    } else {
        site.config.allowed_domains.clone()
    };
    let list = domains.join(",");

    let mut contents = format!(
        "// Managed by ffwebapps — do not edit.\n\
         user_pref(\"ffwebapps.externalLinks.enabled\", {enabled});\n\
         user_pref(\"ffwebapps.allowedDomains\", \"{list}\");\n"
    );

    // Opt-in: force/maximise hardware video decoding for WebRTC calls. On Linux
    // Firefox already GPU-decodes regular video and the WebRTC H.264/VP9 paths
    // by default; the two knobs left off are (a) forcing decode past Firefox's
    // GPU blocklist and (b) the hardware VP8 path (WhatsApp/Meet). Both can
    // expose driver bugs, hence opt-in. Verify with about:support / about:webrtc.
    if site.config.hardware_webrtc {
        contents.push_str(
            "user_pref(\"media.hardware-video-decoding.force-enabled\", true);\n\
             user_pref(\"media.navigator.mediadatadecoder_vp8_hardware_enabled\", true);\n",
        );
    }

    create_dir_all(profile_dir).context("Failed to create profile directory")?;
    write(profile_dir.join("user.js"), contents).context("Failed to write profile prefs")?;

    Ok(())
}
