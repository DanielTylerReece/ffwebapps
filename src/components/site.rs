use std::collections::BTreeMap;
use std::process::Child;

use anyhow::{Context, Result};
use data_url::DataUrl;
use log::info;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use url::Url;
pub use web_app_manifest::WebAppManifest as SiteManifest;
use web_app_manifest::resources::{IconResource, ProtocolHandlerResource};
use web_app_manifest::types::{ImagePurpose, ImageSize, Url as ManifestUrl};

use crate::components::runtime::Runtime;
use crate::directories::ProjectDirs;
use crate::storage::Config;
use crate::utils::sanitize_string;

const DOWNLOAD_ERROR: &str = "Failed to download web app manifest";
const DATA_URL_ERROR: &str = "Failed to process web app manifest data URL";
const PARSE_ERROR: &str = "Failed to parse web app manifest";
const INVALID_URL: &str = "Web app without valid absolute URL is not possible";

/// Contains configuration for the web app.
///
/// Most optional data here are just overwrites for information
/// provided by the web app in its manifest. If they are not not
/// set, they will default to the value in the manifest.
///
/// This struct also contains few required configuration for
/// the web app, such as a document and manifest URL.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[allow(clippy::derive_partial_eq_without_eq)]
pub struct SiteConfig {
    /// A custom web app name.
    pub name: Option<String>,

    /// A custom web app description.
    pub description: Option<String>,

    /// A custom web app start URL.
    pub start_url: Option<Url>,

    /// A custom web app icon URL.
    pub icon_url: Option<Url>,

    /// Direct URL of the site's main document.
    pub document_url: Url,

    /// Direct URL of the site's web app manifest.
    pub manifest_url: Url,

    /// Custom web app categories.
    pub categories: Option<Vec<String>>,

    /// Custom web app keywords.
    pub keywords: Option<Vec<String>>,

    /// Enabled URL handlers.
    ///
    /// Contains web app URL scopes that the browser extension
    /// will intercept and open in the web app window.
    #[serde(default)]
    pub enabled_url_handlers: Vec<String>,

    /// Enabled protocol handlers.
    ///
    /// Contains web app's protocol schemes (without the trailing `:`)
    /// that are registered to the operating system.
    ///
    /// Both the handlers specified in the manifest and ones added
    /// using the `registerProtocolHandler` API must be included here
    /// in order to be registered.
    #[serde(default)]
    pub enabled_protocol_handlers: Vec<String>,

    /// Custom protocol handlers.
    ///
    /// Contains protocol handlers dynamically registered using
    /// the [`registerProtocolHandler`] JavaScript API.
    ///
    /// [`registerProtocolHandler`]: https://developer.mozilla.org/docs/Web/API/Navigator/registerProtocolHandler
    #[serde(default)]
    pub custom_protocol_handlers: Vec<ProtocolHandlerResource>,

    /// Whether the web app should be launched on the system login.
    #[serde(default)]
    pub launch_on_login: bool,

    /// Whether the web app should be launched on the browser launch.
    #[serde(default)]
    pub launch_on_browser: bool,

    /// Stable per-app identifier for Firefox's first-party Web Apps
    /// (Taskbar Tabs) registry. Generated when the web app is installed.
    ///
    /// Also used as the Wayland `app_id` (`org.mozilla.firefox.webapp-<id>`)
    /// and the `.desktop` `StartupWMClass`.
    #[serde(default)]
    pub webapp_id: Option<uuid::Uuid>,

    /// Whether out-of-scope links should open in the system default browser.
    ///
    /// Defaults to enabled when unset.
    #[serde(default)]
    pub external_links: Option<bool>,

    /// Domains (wildcards allowed) that stay inside the app window even when
    /// out of the manifest scope — e.g. auth/SSO providers and related
    /// services. When empty, a sensible default is derived from the scope.
    #[serde(default)]
    pub allowed_domains: Vec<String>,

    /// Opt-in: force/maximise hardware video decoding for WebRTC calls. Off by
    /// default. Regular video decode is already GPU-accelerated by Firefox on
    /// Linux; this forces decoding past Firefox's GPU blocklist and enables the
    /// hardware VP8 path used by WhatsApp/Meet. Can expose driver bugs.
    #[serde(default)]
    pub hardware_webrtc: bool,

    /// Opt-in: process scheduling applied to the runtime at launch, e.g.
    /// `nice:-5`, `rr:5`, `fifo:5`, `batch`, `idle`. Keeps audio/video
    /// glitch-free under load (useful for video chat on RT kernels). RT
    /// policies (`rr`/`fifo`) need `rtprio` privileges; ffwebapps falls back to
    /// normal scheduling if they can't be applied.
    #[serde(default)]
    pub scheduling: Option<String>,
}

/// Build the launch wrapper for an optional scheduling spec. Returns an empty
/// vec when there's no (or an unrecognised) spec; otherwise an argv prefix that
/// runs the runtime under the requested policy. RT policies gracefully fall back
/// to normal scheduling if they can't be applied (e.g. no `rtprio` privilege).
fn scheduling_launcher(spec: Option<&str>) -> Vec<String> {
    let spec = match spec {
        Some(s) => s.trim(),
        None => return vec![],
    };
    let sched = if let Some(n) = spec.strip_prefix("nice:") {
        format!("nice -n {}", n.trim())
    } else if let Some(p) = spec.strip_prefix("rr:") {
        format!("chrt -r {}", p.trim())
    } else if let Some(p) = spec.strip_prefix("fifo:") {
        format!("chrt -f {}", p.trim())
    } else if spec == "batch" {
        "chrt -b 0".to_string()
    } else if spec == "idle" {
        "chrt -i 0".to_string()
    } else {
        return vec![];
    };

    // `<sched> "$@"` runs the runtime under the policy; on failure (e.g. no
    // privilege for an RT policy) we still `exec "$@"` so the app launches.
    let script = format!("{sched} \"$@\" || exec \"$@\"");
    vec!["sh".to_string(), "-c".to_string(), script, "ffwebapps-sched".to_string()]
}

#[non_exhaustive]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct Site {
    /// A web app ID.
    ///
    /// Stored as the ULID format. Unique for each web app
    /// instance and auto-generated when a web app is installed.
    pub ulid: Ulid,

    /// A profile ID.
    ///
    /// Represents the profile where this web app is installed.
    pub profile: Ulid,

    /// A web app config.
    pub config: SiteConfig,

    /// A web app manifest.
    pub manifest: SiteManifest,
}

impl Site {
    fn download(url: &Url, client: &Client) -> Result<String> {
        // If the URL is not a data URL, just download it using reqwest
        let json = if url.scheme() != "data" {
            client
                .get(url.to_owned())
                .header(reqwest::header::REFERER, url.to_string())
                .send()?
                .text()?

        // If the URL is a data URL (used for installing non-PWA sites), decode it using data-url
        } else {
            let url = DataUrl::process(url.as_str()).context(DATA_URL_ERROR)?;
            let (body, _) = url.decode_to_vec().context(DATA_URL_ERROR)?;
            String::from_utf8(body).context(DATA_URL_ERROR)?
        };

        // Trim BOM from the URL to prevent JSON parse errors
        Ok(json.trim_start_matches('\u{feff}').into())
    }

    #[inline]
    pub fn new(profile: Ulid, config: SiteConfig, client: &Client) -> Result<Self> {
        info!("Downloading the web app manifest");
        let json = Self::download(&config.manifest_url, client).context(DOWNLOAD_ERROR)?;

        // If the manifest URL is a data URL, replace it with the document URL
        let manifest_url = if config.manifest_url.scheme() != "data" {
            &config.manifest_url
        } else {
            &config.document_url
        };

        info!("Parsing the web app manifest");
        let mut manifest: SiteManifest = serde_json::from_str(&json).context(PARSE_ERROR)?;
        manifest.process(&config.document_url, manifest_url).context(PARSE_ERROR)?;

        Ok(Self { ulid: Ulid::new(), profile, config, manifest })
    }

    #[inline]
    pub fn update(&mut self, client: &Client) -> Result<()> {
        // There is nothing to update if the manifest is a data URL because it is always static
        if self.config.manifest_url.scheme() == "data" {
            return Ok(());
        }

        info!("Downloading the web app manifest");
        let json = Self::download(&self.config.manifest_url, client).context(DOWNLOAD_ERROR)?;

        info!("Parsing the web app manifest");
        let mut manifest: SiteManifest = serde_json::from_str(&json).context(PARSE_ERROR)?;
        manifest
            .process(&self.config.document_url, &self.config.manifest_url)
            .context(PARSE_ERROR)?;

        self.manifest = manifest;
        Ok(())
    }

    #[inline]
    pub fn launch<I: IntoIterator<Item = (String, String)>>(
        &self,
        dirs: &ProjectDirs,
        runtime: &Runtime,
        config: &Config,
        urls: &[Url],
        arguments: &[String],
        variables: I,
    ) -> Result<Child> {
        let profile = dirs.userdata.join("profiles").join(self.profile.to_string());

        // Resolve the stable per-app ID for Firefox's Web Apps (Taskbar Tabs) registry
        let webapp_id = self
            .config
            .webapp_id
            .context("Web app ID is not set; relaunch to generate it")?;

        // Start URL: an explicit override URL if one is given, otherwise the site start URL
        let start_url = match urls.first() {
            Some(url) => url.to_string(),
            None => self.url(),
        };

        // Launch as a first-party Firefox Web App (Taskbar Tab) window.
        // The matching registry entry is written by `taskbartabs::sync_registry`.
        #[rustfmt::skip]
        let mut args = vec![
            "-profile".into(), profile.display().to_string(),
            "-taskbar-tab".into(), webapp_id.to_string(),
            "-new-window".into(), start_url,
            "-container".into(), "0".into(),
        ];

        // Pass variables needed for specific runtime features
        let mut vars = BTreeMap::new();

        // Give each web app a unique Firefox remoting name. Without this, the app
        // shares the default "firefox" remoting name and can intercept external-link
        // launches meant for the user's default browser (opening them in the app's
        // profile instead). A unique name also makes relaunching focus the existing
        // app window rather than spawning a duplicate (single instance per app).
        vars.insert("MOZ_APP_REMOTINGNAME".into(), format!("ffwebapps-{}", self.ulid));

        if config.runtime_enable_wayland {
            vars.insert("MOZ_ENABLE_WAYLAND".into(), "1".into());
        }
        if config.runtime_use_xinput2 {
            vars.insert("MOZ_USE_XINPUT2".into(), "1".into());
        }
        if config.runtime_use_portals {
            vars.insert("GTK_USE_PORTAL".into(), "1".into());
        }

        // Include all user arguments and variables and launch the runtime,
        // optionally under a scheduling policy (nice / RT) for smooth media.
        args.extend_from_slice(arguments);
        vars.extend(variables);
        let launcher = scheduling_launcher(self.config.scheduling.as_deref());
        runtime.run(&launcher, &args, vars)
    }
}

impl Site {
    /// Start URL is used as an info URL on supported systems.
    #[rustfmt::skip]
    pub fn url(&self) -> String {
        // Try to get user-specified start URL
        if let Some(url) = &self.config.start_url { url.to_string() }

        // If not set, use manifest-provided start URL
        else if let ManifestUrl::Absolute(url) = &self.manifest.start_url { url.to_string() }

        // This should not happen on valid web apps
        else { unreachable!("{}", INVALID_URL) }
    }

    /// Domain of a web app's scope is used as a publisher name
    /// on supported systems or when the app name is undefined.
    pub fn domain(&self) -> String {
        if let ManifestUrl::Absolute(url) = &self.manifest.scope {
            match url.host() {
                Some(domain) => domain.to_string(),
                None => unreachable!("{}", INVALID_URL),
            }
        } else {
            unreachable!("{}", INVALID_URL)
        }
    }

    /// First tries the user-specified name, then tries manifest name
    /// and then short name. If no name is specified, uses the domain.
    pub fn name(&self) -> String {
        [
            self.config.name.as_deref(),
            self.manifest.name.as_deref(),
            self.manifest.short_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(sanitize_string)
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| sanitize_string(&self.domain()))
    }

    /// First tries the user-specified description, then tries manifest description.
    /// If no description is specified, returns an empty string.
    pub fn description(&self) -> String {
        [self.config.description.as_deref(), self.manifest.description.as_deref()]
            .into_iter()
            .flatten()
            .map(sanitize_string)
            .find(|description| !description.is_empty())
            .unwrap_or_else(|| "".into())
    }

    /// First tries the user-specified icon, then tries manifest icons.
    pub fn icons(&self) -> Vec<IconResource> {
        match &self.config.icon_url {
            Some(icon) => vec![IconResource {
                src: ManifestUrl::Absolute(icon.clone()),
                sizes: [ImageSize::default()].iter().cloned().collect(),
                purpose: [ImagePurpose::default()].iter().cloned().collect(),
                r#type: None,
                label: None,
            }],
            None => self.manifest.icons.clone(),
        }
    }

    /// Categories can be used for user organization.
    ///
    /// There is no fixed list of categories, but some known categories are converted
    /// to XDG menu categories on Linux and Apple App Store categories on macOS.
    ///
    /// First tries the user-specified categories, then try manifest categories.
    pub fn categories(&self) -> Vec<String> {
        match &self.config.categories {
            Some(categories) => categories,
            None => &self.manifest.categories,
        }
        .iter()
        .map(|item| sanitize_string(item))
        .collect()
    }

    /// Keywords can also be used for user organization and contain
    /// additional information that can be used to describe the web app.
    ///
    /// Keywords are used as additional search queries on Linux.
    ///
    /// First tries the user-specified keywords, then try manifest keywords.
    pub fn keywords(&self) -> Vec<String> {
        match &self.config.keywords {
            Some(keywords) => keywords,
            None => &self.manifest.keywords,
        }
        .iter()
        .map(|item| sanitize_string(item))
        .collect()
    }
}
