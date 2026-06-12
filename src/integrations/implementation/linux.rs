use std::convert::TryInto;
use std::fmt::Write as FmtWrite;
use std::fs::{File, create_dir_all, remove_file, write};
use std::io::Write as IoWrite;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use glob::glob;
use image::GenericImageView;
use log::{debug, error, warn};
use reqwest::blocking::Client;
use url::Url;
use web_app_manifest::resources::IconResource;
use web_app_manifest::types::{ImagePurpose, ImageSize};

use crate::components::site::Site;
use crate::integrations::categories::XDG_CATEGORIES;
use crate::integrations::utils::{download_icon, normalize_category_name, store_icon};
use crate::integrations::{IntegrationInstallArgs, IntegrationUninstallArgs};
use crate::utils::sanitize_string;

const BASE_DIRECTORIES_ERROR: &str = "Failed to determine base system directories";
const CONVERT_ICON_URL_ERROR: &str = "Failed to convert icon URL";
const CONVERT_SHORTCUT_URL_ERROR: &str = "Failed to convert shortcut URL";
const DOWNLOAD_ICON_ERROR: &str = "Failed to download icon";
const PROCESS_ICON_ERROR: &str = "Failed to process icon";
const LOAD_ICON_ERROR: &str = "Failed to load icon";
const SAVE_ICON_ERROR: &str = "Failed to save icon";
const CREATE_ICON_DIRECTORY_ERROR: &str = "Failed to create icon directory";
const CREATE_ICON_FILE_ERROR: &str = "Failed to create icon file";
const CREATE_APPLICATION_DIRECTORY_ERROR: &str = "Failed to create application directory";
const WRITE_APPLICATION_FILE_ERROR: &str = "Failed to write application file";
const COPY_STARTUP_ENTRY_ERROR: &str = "Failed to copy startup entry";

//////////////////////////////
// Utils
//////////////////////////////

/// Update system's application cache.
#[rustfmt::skip]
fn update_application_cache(data: &Path) {
    let _ = Command::new("touch").arg(data.join("icons")).arg(data.join("icons/hicolor")).spawn();
    let _ = Command::new("update-desktop-database").arg(data.join("applications")).spawn();
    let _ = Command::new("update-mime-database").arg(data.join("mime")).spawn();
    let _ = Command::new("gtk-update-icon-cache").spawn();
    let _ = Command::new("xdg-desktop-menu").arg("forceupdate").spawn();
}

//////////////////////////////
// Implementation
//////////////////////////////

#[derive(Debug, Clone)]
struct SiteIds {
    pub name: String,
    pub description: String,
    pub ulid: String,
    pub classid: String,
}

impl SiteIds {
    pub fn create_for(site: &Site) -> Self {
        let name = site.name();
        let description = site.description();
        let ulid = site.ulid.to_string();
        let classid = format!("FFPWA-{ulid}");
        Self { name, description, ulid, classid }
    }
}

/// Obtain and process icons from the icon list.
///
/// All supported icons from the icon list are downloaded and stored to
/// the correct locations to comply with the Icon Theme Specification.
///
/// All SVG icons are directly stored as `scalable` or `symbolic` icons,
/// and other supported icons are converted to PNG and then stored.
///
/// The 48x48 icon has to exist as required by the Icon Theme Specification.
/// In case it is not provided by the icon list, it is obtained using
/// the [`store_icon`] function.
///
/// # Parameters
///
/// - `id`:    An icon ID, consisting from the web app ID and shortcut ID.
/// - `name`:  A web app or shortcut name. Used to generate a fallback icon.
/// - `icons`: A list of available icons for the web app or shortcut.
/// - `data`:  A path to the XDG data directory.
/// - `client`: An instance of a blocking HTTP client.
///
fn store_icons(
    id: &str,
    name: &str,
    icons: &[IconResource],
    data: &Path,
    client: &Client,
) -> Result<()> {
    // The 48x48 icon has to exist as required by the Icon Theme Specification
    // We need to generate it manually if the manifest does not provide it
    let mut required_icon_found = false;

    // Download and store all icons
    for icon in icons {
        // Wrapped into a closure to emulate currently unstable `try` blocks
        let mut process = || -> Result<()> {
            // Only icons with absolute URLs can be used
            let url: Url = icon.src.clone().try_into().context(CONVERT_ICON_URL_ERROR)?;
            debug!("Processing icon {url}");

            // Download icon and get its content type
            let (content, content_type) =
                download_icon(url, client).context(DOWNLOAD_ICON_ERROR)?;

            if content_type == "image/svg+xml" {
                // Scalable (normal SVG) icons can be directly saved into the correct directory
                if icon.purpose.contains(&ImagePurpose::Any) {
                    let directory = data.join("icons/hicolor/scalable/apps");
                    let filename = directory.join(format!("{id}.svg"));

                    debug!("Saving as scalable icon");
                    create_dir_all(directory).context(CREATE_ICON_DIRECTORY_ERROR)?;
                    let mut file = File::create(filename).context(CREATE_ICON_FILE_ERROR)?;
                    file.write_all(&content).context(SAVE_ICON_ERROR)?;
                }

                // Symbolic (monochrome SVG) icons can be directly saved into the correct directory
                if icon.purpose.contains(&ImagePurpose::Monochrome) {
                    let directory = data.join("icons/hicolor/symbolic/apps");
                    let filename = directory.join(format!("{id}-symbolic.svg"));

                    debug!("Saving as symbolic icon");
                    create_dir_all(directory).context(CREATE_ICON_DIRECTORY_ERROR)?;
                    let mut file = File::create(filename).context(CREATE_ICON_FILE_ERROR)?;
                    file.write_all(&content).context(SAVE_ICON_ERROR)?;
                }

                return Ok(());
            }

            // Raster icons must contain "any" type
            // Symbolic raster icons are not supported by DEs
            if !icon.purpose.contains(&ImagePurpose::Any) {
                return Ok(());
            }

            // Raster icons need to be processed (converted to PNG) using the `image` crate
            debug!("Processing as raster icon");
            let img = image::load_from_memory(&content).context(LOAD_ICON_ERROR)?;
            let size = img.dimensions();

            let directory = data.join(format!("icons/hicolor/{}x{}/apps", size.0, size.1));
            let filename = directory.join(format!("{id}.png"));
            create_dir_all(directory).context(CREATE_ICON_DIRECTORY_ERROR)?;
            img.save(filename).context(SAVE_ICON_ERROR)?;

            if size == (48, 48) {
                required_icon_found = true;
            }

            Ok(())
        };

        // Process the icon and catch errors
        if let Err(error) = process().context(PROCESS_ICON_ERROR) {
            error!("{error:?}");
            warn!("Falling back to the next available icon");
        }
    }

    // We need to create 48x48 icon to comply with the specification
    // Use the first working icon from the normalized list
    if !required_icon_found {
        // Create directory for 48x48 icons in case it does not exist
        let directory = data.join("icons/hicolor/48x48/apps");
        let filename = directory.join(format!("{id}.png"));
        create_dir_all(directory).context(CREATE_ICON_DIRECTORY_ERROR)?;

        warn!("No required 48x48 icon is provided");
        warn!("Generating it from other available icons");
        let size = &ImageSize::Fixed(48, 48);
        return store_icon(icons, name, size, &filename, client);
    }

    Ok(())
}

fn remove_icons(classid: &str, data: &Path) {
    let directory = data.display().to_string();
    let pattern = format!("{directory}/icons/hicolor/*/apps/{classid}*");

    if let Ok(paths) = glob(&pattern) {
        for path in paths.filter_map(Result::ok) {
            let _ = remove_file(path);
        }
    }
}

fn create_desktop_entry(
    args: &IntegrationInstallArgs,
    ids: &SiteIds,
    exe: &str,
    data: &Path,
) -> Result<()> {
    // Process some known manifest categories and reformat them into XDG names
    let mut categories = vec![];
    for category in args.site.categories() {
        // Normalize category name for easier matching
        let category = normalize_category_name(&category);

        // Get the mapped XDG category based on the site categories
        if let Some(category) = XDG_CATEGORIES.get(&category) {
            categories.extend_from_slice(category);
        }
    }
    categories.sort_unstable();
    categories.dedup();

    // Get the .desktop filename in the applications directory
    let directory = data.join("applications");
    let filename = directory.join(format!("{}.desktop", ids.classid));

    // The taskbar-tab window's Wayland app_id / WM class is derived from the web
    // app's UUID; the launcher's StartupWMClass must match it so the window groups
    // under this entry and shows its icon.
    let wmclass = match args.site.config.webapp_id {
        Some(id) => format!("org.mozilla.firefox.webapp-{id}"),
        None => ids.classid.clone(),
    };

    // Store entry data
    let mut entry = format!(
        "[Desktop Entry]
Type=Application
Version=1.4
Name={name}
Comment={description}
Keywords={keywords};
Categories=GTK;{categories};
Icon={icon}
Exec={exe} site launch {id} --protocol %u
Actions={actions}
MimeType={protocols}
Terminal=false
StartupNotify=true
StartupWMClass={wmclass}
",
        id = &ids.ulid,
        name = &ids.name,
        description = &ids.description,
        keywords = &args.site.keywords().join(";"),
        categories = &categories.join(";"),
        actions = (0..args.site.manifest.shortcuts.len()).fold(String::new(), |mut output, i| {
            let _ = write!(output, "{i};");
            output
        }),
        protocols = args.site.config.enabled_protocol_handlers.iter().fold(
            String::new(),
            |mut output, protocol| {
                let _ = write!(output, "x-scheme-handler/{};", sanitize_string(protocol));
                output
            }
        ),
        icon = &ids.classid,
        wmclass = &wmclass,
        exe = &exe,
    );

    // Store all shortcuts
    for (i, shortcut) in args.site.manifest.shortcuts.iter().enumerate() {
        let name = sanitize_string(&shortcut.name);
        let url: Url = shortcut.url.clone().try_into().context(CONVERT_SHORTCUT_URL_ERROR)?;
        let icon = format!("{}-{}", ids.classid, i);

        if args.update_icons {
            store_icons(&icon, &name, &shortcut.icons, data, args.client.unwrap())
                .context("Failed to store shortcut icons")?;
        }

        let action = format!(
            "
[Desktop Action {actionid}]
Name={name}
Icon={icon}
Exec={exe} site launch {siteid} --url \"{url}\"
",
            actionid = i,
            siteid = &ids.ulid,
            name = &name,
            icon = &icon,
            url = &url,
            exe = &exe,
        );

        entry += &action;
    }

    // Create the directory and write the file
    create_dir_all(directory).context(CREATE_APPLICATION_DIRECTORY_ERROR)?;
    write(filename, entry).context(WRITE_APPLICATION_FILE_ERROR)?;

    Ok(())
}

fn create_startup_entry(
    args: &IntegrationInstallArgs,
    ids: &SiteIds,
    data: &Path,
    config: &Path,
) -> Result<()> {
    let applications_entry = data.join("applications").join(format!("{}.desktop", ids.classid));
    let autostart_entry = config.join("autostart").join(format!("{}.desktop", ids.classid));

    if args.site.config.launch_on_login {
        // If launch on login is enabled, copy its shortcut to the autostart
        // directory — with `--hidden` added when the app should start in the
        // tray instead of opening its window at login.
        let mut entry =
            std::fs::read_to_string(applications_entry).context(COPY_STARTUP_ENTRY_ERROR)?;
        if args.site.config.start_hidden {
            entry = entry.replace(
                &format!("site launch {}", args.site.ulid),
                &format!("site launch {} --hidden", args.site.ulid),
            );
        }
        write(autostart_entry, entry).context(COPY_STARTUP_ENTRY_ERROR)?;
    } else {
        // Otherwise, try to remove its shortcut from the autostart directory
        let _ = remove_file(autostart_entry);
    }

    Ok(())
}

fn remove_desktop_entry(classid: &str, data: &Path) {
    let directory = data.join("applications");
    let filename = directory.join(format!("{classid}.desktop"));
    let _ = remove_file(filename);
}

fn remove_startup_entry(classid: &str, config: &Path) {
    let directory = config.join("autostart");
    let filename = directory.join(format!("{classid}.desktop"));
    let _ = remove_file(filename);
}

//////////////////////////////
// KWin window rule (KDE only)
//////////////////////////////

// On Wayland the compositor places a (re-)mapped window itself: when a web app
// window that was hidden to the tray (unmapped) is shown again, KWin would
// re-place it instead of returning it to where the user left it. KWin's own
// window rules support a "Remember" position policy (positionrule=4) that makes
// KWin save and restore the position of matching windows — exactly what
// hide-to-tray needs. We install one declarative rule per web app, keyed to its
// window class, at integration-install time; KWin then handles positions itself
// with no runtime moving parts. Other desktops are unaffected (this is KWin's
// own config file and is only written in KDE sessions).

fn kde_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
}

/// Surgically upsert or remove this app's rule section in ~/.config/kwinrulesrc
/// and keep the `[General]` rules list/count in sync, preserving everything else
/// in the file (the user's own rules, and values KWin writes back itself — for
/// "Remember" rules KWin stores the remembered position in our section).
fn write_kwin_rule(config: &Path, site: &Site, classid: &str, install: bool) -> Result<()> {
    let webapp_id = match site.config.webapp_id {
        Some(id) => id,
        None => return Ok(()),
    };
    let path = config.join("kwinrulesrc");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let section = format!("ffwebapps-{classid}");

    // Split into (header, lines) blocks, dropping any existing section of ours.
    let mut blocks: Vec<(String, Vec<String>)> = vec![(String::new(), vec![])];
    for line in content.lines() {
        if line.starts_with('[') {
            blocks.push((line.to_string(), vec![]));
        } else {
            blocks.last_mut().unwrap().1.push(line.to_string());
        }
    }
    blocks.retain(|(header, _)| header != &format!("[{section}]"));

    // Only rules listed in [General] rules= are active; keep the list in sync.
    if !blocks.iter().any(|(header, _)| header == "[General]") {
        blocks.push(("[General]".to_string(), vec![]));
    }
    for (header, lines) in blocks.iter_mut() {
        if header != "[General]" {
            continue;
        }
        let mut rules: Vec<String> = lines
            .iter()
            .find_map(|line| line.strip_prefix("rules="))
            .map(|list| list.split(',').filter(|id| !id.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        rules.retain(|id| id != &section);
        if install {
            rules.push(section.clone());
        }
        lines.retain(|line| !line.starts_with("rules=") && !line.starts_with("count="));
        lines.insert(0, format!("count={}", rules.len()));
        lines.insert(1, format!("rules={}", rules.join(",")));
    }

    if install {
        // No initial position= value: the rule stays inert until KWin writes the
        // remembered position back on the first unmap, so the first launch gets
        // normal placement instead of a forced corner.
        blocks.push((
            format!("[{section}]"),
            vec![
                format!(
                    "Description=ffwebapps: remember window position for {}",
                    sanitize_string(&site.name())
                ),
                "positionrule=4".to_string(),
                format!("wmclass=org.mozilla.firefox.webapp-{webapp_id}"),
                "wmclassmatch=1".to_string(),
                "types=1".to_string(),
            ],
        ));
    }

    let mut out = String::new();
    for (header, lines) in &blocks {
        if !header.is_empty() {
            out.push_str(header);
            out.push('\n');
        }
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    write(&path, out).context("Failed to write kwinrulesrc")?;
    Ok(())
}

/// Ask KWin to reload its config so a rule change applies without a relogin.
fn reconfigure_kwin() {
    for command in [
        ["qdbus6", "org.kde.KWin", "/KWin", "org.kde.KWin.reconfigure"].as_slice(),
        ["qdbus", "org.kde.KWin", "/KWin", "org.kde.KWin.reconfigure"].as_slice(),
        [
            "dbus-send",
            "--session",
            "--type=method_call",
            "--dest=org.kde.KWin",
            "/KWin",
            "org.kde.KWin.reconfigure",
        ]
        .as_slice(),
    ] {
        let success = Command::new(command[0])
            .args(&command[1..])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if success {
            return;
        }
    }
    debug!("Could not ask KWin to reconfigure (rule applies after relogin)");
}

//////////////////////////////
// Interface
//////////////////////////////

#[inline]
pub fn install(args: &IntegrationInstallArgs) -> Result<()> {
    let ids = SiteIds::create_for(args.site);

    // Installed launchers stay clean (just the binary); custom/dev layouts bake
    // the resolved data/system directories so the launcher targets them.
    let bin = args.dirs.executables.join("ffwebapps");
    let exe = if args.dirs.sysdata == std::path::Path::new("/usr/share/ffwebapps")
        && args.dirs.executables == std::path::Path::new("/usr/bin")
    {
        bin.display().to_string()
    } else {
        format!(
            "env FFPWA_USERDATA={} FFPWA_SYSDATA={} {}",
            args.dirs.userdata.display(),
            args.dirs.sysdata.display(),
            bin.display(),
        )
    };

    let base = directories::BaseDirs::new().context(BASE_DIRECTORIES_ERROR)?;
    let data = base.data_dir().to_owned();
    let config = base.config_dir().to_owned();

    if args.update_icons {
        store_icons(&ids.classid, &ids.name, &args.site.icons(), &data, args.client.unwrap())
            .context("Failed to store web app icons")?;
    }

    create_desktop_entry(args, &ids, &exe, &data).context("Failed to create application entry")?;
    create_startup_entry(args, &ids, &data, &config).context("Failed to create startup entry")?;
    update_application_cache(&data);

    if kde_session() {
        match write_kwin_rule(&config, args.site, &ids.classid, true) {
            Ok(()) => reconfigure_kwin(),
            Err(error) => warn!("Failed to install the KWin window rule: {error}"),
        }
    }

    Ok(())
}

#[inline]
pub fn uninstall(args: &IntegrationUninstallArgs) -> Result<()> {
    let ids = SiteIds::create_for(args.site);

    let base = directories::BaseDirs::new().context(BASE_DIRECTORIES_ERROR)?;
    let data = &base.data_dir().to_owned();
    let config = &base.config_dir().to_owned();

    remove_icons(&ids.classid, data);
    remove_desktop_entry(&ids.classid, data);
    remove_startup_entry(&ids.classid, config);
    update_application_cache(data);

    if kde_session() {
        match write_kwin_rule(config, args.site, &ids.classid, false) {
            Ok(()) => reconfigure_kwin(),
            Err(error) => warn!("Failed to remove the KWin window rule: {error}"),
        }
    }

    Ok(())
}
