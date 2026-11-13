//! Optional ReShade, installed by its official setup tool in Crystal's data folder.
//! ReShade launches use a separate copy of the x86 game host; ordinary launches
//! never gain a proxy DLL. Presets, effects and the editable config stay in one
//! persistent folder across host and launcher updates.

use crate::i18n::t;
use crate::{App, Message};
use crate::effect::Effect;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const VERSION: &str = "6.8.0";
const SETUP_URL: &str = "https://reshade.me/downloads/ReShade_Setup_6.8.0.exe";
const SETUP_HASH: &str = "207aea16205fbf952bc8fe1879966672454cf04002e7ad34237c7990a5b3c0b4";
const DLL_HASH: &str = "538998f66c0197adcdeadfe8e5ea19dd6ca3253e40399476d291f385ce518b88";
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const MAX_INI: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    pub enabled: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub installed: bool,
    pub busy: bool,
    pub status: String,
    pub error: Option<String>,
    pub preset: String,
    generation: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Refresh,
    Checked(u64, bool, String),
    Toggle(bool),
    Setup,
    Downloaded(Result<PathBuf, String>),
    Finished(Result<(), String>),
    ChoosePreset,
    Picked(Option<PathBuf>),
    PresetSaved(Result<String, String>),
    OpenFolder,
    Website,
}

fn message(msg: Msg) -> Message {
    Message::Reshade(msg)
}

pub(crate) fn folder(root: &Path) -> PathBuf {
    root.join("crystal").join("reshade")
}

fn root() -> PathBuf {
    crate::dgvoodoo::data_root()
}

use crate::pol_less::tools::embedded::sha256_hex as digest;

/// Neither a symlink nor another reparse point (a junction, a cloud placeholder).
fn plain(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn regular_file(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| t("Could not read the ReShade file."))?;
    if !metadata.is_file() || !plain(&metadata) || metadata.len() > cap {
        return Err(t("The ReShade file is not a supported regular file.").into());
    }
    let file = File::open(path).map_err(|_| t("Could not read the ReShade file."))?;
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| t("Could not read the ReShade file."))?;
    if bytes.len() as u64 > cap {
        return Err(t("The ReShade file is too large.").into());
    }
    Ok(bytes)
}

fn directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|_| t("Could not create the ReShade folder."))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| t("Could not read the ReShade folder."))?;
    if !metadata.is_dir() || !plain(&metadata) {
        return Err(t("The ReShade folder must be a regular local folder.").into());
    }
    Ok(())
}

fn cache(root: &Path) -> Result<PathBuf, String> {
    directory(&root.join("crystal"))?;
    let dir = folder(root);
    directory(&dir)?;
    Ok(dir)
}

fn x86_image(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(b"MZ") {
        return false;
    }
    let Some(offset) = bytes
        .get(0x3c..0x40)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    else {
        return false;
    };
    bytes.get(offset..offset.saturating_add(4)) == Some(b"PE\0\0")
        && bytes.get(offset.saturating_add(4)..offset.saturating_add(6)) == Some(&[0x4c, 0x01])
}

fn runtime(root: &Path) -> Result<Vec<u8>, String> {
    let bytes = regular_file(&folder(root).join("dxgi.dll"), MAX_BINARY)?;
    if digest(&bytes) != DLL_HASH || !x86_image(&bytes) {
        return Err(t(
            "ReShade does not match the supported standard x86 release. Run ReShade setup again.",
        )
        .into());
    }
    let config = regular_file(&folder(root).join("ReShade.ini"), MAX_INI)?;
    ini_text(&config)?;
    Ok(bytes)
}

fn installed(root: &Path) -> bool {
    runtime(root).is_ok()
}

/// Whether ReShade is installed, and the preset its config names; ReShade.ini is read once.
fn status(root: &Path) -> (bool, String) {
    let dir = folder(root);
    let dll_ok = regular_file(&dir.join("dxgi.dll"), MAX_BINARY).is_ok_and(|b| digest(&b) == DLL_HASH && x86_image(&b));
    let config = regular_file(&dir.join("ReShade.ini"), MAX_INI).ok();
    match config.as_deref().map(ini_text) {
        Some(Ok(text)) => (dll_ok, parse_preset(text)),
        _ => (false, String::new()),
    }
}

#[cfg(test)]
fn current_preset(root: &Path) -> String {
    status(root).1
}

/// `PresetPath` under `[GENERAL]`.
fn parse_preset(text: &str) -> String {
    let mut general = false;
    let mut preset = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            general = line.eq_ignore_ascii_case("[GENERAL]");
        } else if general {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("PresetPath") {
                    preset = value.trim().into();
                }
            }
        }
    }
    preset
}

/// Existing files in a managed host directory are reused only when identical.
/// Concurrent launches can publish identical files, but never replace a foreign one.
fn place_once(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path
        .try_exists()
        .map_err(|_| t("Could not read the ReShade file."))?
    {
        return if regular_file(path, MAX_BINARY)? == bytes {
            Ok(())
        } else {
            Err(
                t("A different file already exists in the ReShade host folder. It was kept.")
                    .into(),
            )
        };
    }
    let temp = path.with_extension(format!("{}.tmp", crate::backend::unique_id()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| t("Could not save the ReShade file."))?;
        file.write_all(bytes)
            .map_err(|_| t("Could not save the ReShade file."))?;
        drop(file);
        match fs::hard_link(&temp, path) {
            Ok(()) => Ok(()),
            Err(_) if regular_file(path, MAX_BINARY).is_ok_and(|existing| existing == bytes) => {
                Ok(())
            }
            Err(_) => Err(t(
                "Could not publish the ReShade file without replacing an existing file.",
            )),
        }
    })();
    let _ = fs::remove_file(&temp);
    result.map_err(str::to_owned)
}

fn variant(root: &Path, host: &[u8], dll: &[u8]) -> Result<PathBuf, String> {
    let dir = cache(root)?;
    let hosts = dir.join("hosts");
    directory(&hosts)?;
    // The runtime was verified against DLL_HASH, so its name part is known.
    let launch = hosts.join(format!("{}-{}", &digest(host)[..16], &DLL_HASH[..16]));
    directory(&launch)?;
    let base = dir
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or(t("The ReShade folder path is not supported."))?;
    let pointer = format!("[INSTALL]\r\nBasePath={base}\r\n");
    place_once(&launch.join("ReShade.ini"), pointer.as_bytes())?;
    place_once(&launch.join("dxgi.dll"), dll)?;
    let loader = launch.join("pol.exe");
    place_once(&loader, host)?;
    // Marks the folder (not the host, which stays byte- and time-identical) as in
    // use, so `prune_hosts` in another launch keeps it.
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS opens a folder.
        let _ = OpenOptions::new()
            .access_mode(0x0100)
            .custom_flags(0x0200_0000)
            .open(&launch)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    }
    Ok(loader)
}

/// Launches running together prepare and prune host folders one at a time.
static HOSTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn loader_for_launch(
    loader: &Path,
    preferences: &Preferences,
    dgvoodoo: bool,
) -> Result<PathBuf, String> {
    if !preferences.enabled {
        return Ok(loader.to_path_buf());
    }
    if !dgvoodoo {
        return Err(t("Enable dgVoodoo before using ReShade.").into());
    }
    let root = root();
    let dll = runtime(&root)
        .map_err(|_| t("Set up ReShade in Settings → Graphics before enabling it."))?;
    let host = regular_file(loader, MAX_BINARY)?;
    if !x86_image(&host) {
        return Err(t("ReShade needs Crystal's 32-bit game host.").into());
    }
    let _hosts = HOSTS.lock().unwrap_or_else(|e| e.into_inner());
    let path = variant(&root, &host, &dll)?;
    let dir = path
        .parent()
        .ok_or(t("Could not create the ReShade host folder."))?;
    // ReShade hooks the translated DirectX output, so a failed wrapper install
    // must not silently launch with an ineffective ReShade option.
    crate::dgvoodoo::place(&root, &crate::dgvoodoo::PINNED, dir)?;
    prune_hosts(dir);
    Ok(path)
}

/// Whether a `hosts` entry name is one `variant` makes: 16 hex, a dash, 16 hex.
fn host_folder_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 33 && b[16] == b'-' && b.iter().enumerate().all(|(i, c)| i == 16 || c.is_ascii_hexdigit())
}

/// Best effort: deletes host folders an older Crystal or loader made. A folder whose `pol.exe`
/// cannot be deleted (Windows locks a running image) is left whole, and so is one used or
/// written in the last hour (its `pol.exe` or the folder itself), which another launch, possibly
/// in another Crystal process, may be preparing.
fn prune_hosts(current: &Path) {
    let Some(hosts) = current.parent() else { return };
    let recent = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().map_or(true, |age| age < Duration::from_secs(3600)));
    for entry in fs::read_dir(hosts).into_iter().flatten().flatten() {
        let path = entry.path();
        if path == current || !host_folder_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && plain(&m)) {
            continue;
        }
        let host = path.join("pol.exe");
        if recent(&host) || recent(&path) || (host.exists() && fs::remove_file(&host).is_err()) {
            continue;
        }
        let _ = fs::remove_dir_all(&path);
    }
}

/// Best effort: deletes setup targets older Crystal builds made (`Crystal-ReShade-Setup-<hash16>.exe`).
fn prune_setup_targets(dir: &Path, keep: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path != keep && name.starts_with("Crystal-ReShade-Setup-") && name.ends_with(".exe") {
            let _ = fs::remove_file(&path);
        }
    }
}

async fn setup_download(root: &Path) -> Result<PathBuf, String> {
    let dir = cache(root)?;
    let path = dir.join(format!("ReShade_Setup_{VERSION}.exe"));
    if regular_file(&path, MAX_BINARY).is_ok_and(|b| digest(&b) == SETUP_HASH) {
        return Ok(path);
    }
    if path.exists() {
        return Err(t("The cached ReShade setup was changed. It was kept; open the ReShade folder to move it aside before trying again.").into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(90))
        .user_agent("CrystalLauncher")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| t("Could not download official ReShade setup."))?;
    let mut response = client
        .get(SETUP_URL)
        .send()
        .await
        .map_err(|_| t("Could not download official ReShade setup."))?;
    if !response.status().is_success() {
        return Err(t("Could not download official ReShade setup.").into());
    }
    if response.content_length().is_some_and(|n| n > MAX_BINARY) {
        return Err(t("The ReShade download is too large.").into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| t("Could not download official ReShade setup."))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > MAX_BINARY {
            return Err(t("The ReShade download is too large.").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if digest(&bytes) != SETUP_HASH {
        return Err(t(
            "Official ReShade setup did not match its expected SHA-256. It was not run.",
        )
        .into());
    }
    place_once(&path, &bytes)?;
    Ok(path)
}

fn setup_command(setup: &Path, host: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(setup);
    command
        .arg(host)
        .args(["--api", "dxgi"])
        .current_dir(host.parent().unwrap_or(Path::new(".")))
        .stdin(std::process::Stdio::null());
    command
}

fn run_setup(root: &Path, setup: &Path) -> Result<(), String> {
    // Keep write and delete sharing denied from verification through execution.
    let mut options = OpenOptions::new();
    options.read(true);
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    }
    let mut pinned = options
        .open(setup)
        .map_err(|_| t("Could not read official ReShade setup."))?;
    let mut bytes = Vec::new();
    (&mut pinned)
        .take(MAX_BINARY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| t("Could not read official ReShade setup."))?;
    if bytes.len() as u64 > MAX_BINARY || digest(&bytes) != SETUP_HASH {
        return Err(t(
            "Official ReShade setup did not match its expected SHA-256. It was not run.",
        )
        .into());
    }
    let (host, hash) = crate::pol_less::tools::embedded::embedded().ok_or(t(
        "ReShade setup needs the embedded 32-bit game host. Reinstall Crystal.",
    ))?;
    if !x86_image(host) {
        return Err(t("ReShade needs Crystal's 32-bit game host.").into());
    }
    let dir = cache(root)?;
    // This copy is an installer target for bitness detection. It is never run.
    let target = dir.join(format!("Crystal-ReShade-Setup-{}.exe", &hash[..16]));
    prune_setup_targets(&dir, &target);
    place_once(&target, host)?;
    let mut child = setup_command(setup, &target)
        .spawn()
        .map_err(|_| t("Could not open official ReShade setup."))?;
    let status = child
        .wait()
        .map_err(|_| t("Could not wait for ReShade setup."))?;
    if !status.success() || !installed(root) {
        return Err(
            t("ReShade setup did not finish a supported installation. Run setup again.").into(),
        );
    }
    Ok(())
}

fn ini_text(bytes: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| t("The ReShade preset or configuration is not valid UTF-8."))?;
    if text.contains('\0') {
        return Err(t("The ReShade preset or configuration contains unsupported data.").into());
    }
    Ok(text.trim_start_matches('\u{feff}'))
}

fn set_preset(text: &str, path: &str) -> String {
    let mut output = Vec::new();
    let mut general = false;
    let mut inserted = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            general = trimmed.eq_ignore_ascii_case("[GENERAL]");
            output.push(line.to_owned());
            if general && !inserted {
                output.push(format!("PresetPath={path}"));
                inserted = true;
            }
        } else if !general
            || !line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("PresetPath"))
        {
            output.push(line.to_owned());
        }
    }
    if !inserted {
        output.extend(["[GENERAL]".into(), format!("PresetPath={path}")]);
    }
    output.join("\r\n") + "\r\n"
}

fn save_preset(root: &Path, source: &Path) -> Result<String, String> {
    if !source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ini"))
    {
        return Err(t("Choose a ReShade preset .ini file.").into());
    }
    let bytes = regular_file(source, MAX_INI)?;
    let preset = ini_text(&bytes)?;
    if !preset.lines().any(|line| {
        line.split_once('=').is_some_and(|(key, _)| {
            matches!(
                key.trim().to_ascii_lowercase().as_str(),
                "techniques" | "techniquesorting"
            )
        })
    }) {
        return Err(t(
            "This file does not look like a ReShade preset. Choose the preset, not ReShade.ini.",
        )
        .into());
    }
    let dir = cache(root)?;
    let config_path = dir.join("ReShade.ini");
    let config = regular_file(&config_path, MAX_INI)?;
    let config_text = ini_text(&config)?;
    let presets = dir.join("presets");
    directory(&presets)?;
    let name = source
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| s.len() < 150 && !s.chars().any(char::is_control))
        .ok_or(t("The ReShade preset filename is not supported."))?;
    let destination = presets.join(format!("{}-{name}", &digest(&bytes)[..16]));
    let path = destination
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or(t("The ReShade preset path is not supported."))?;
    place_once(&destination, &bytes)?;
    crate::fsutil::write_atomic(&config_path, set_preset(config_text, path).as_bytes())
        .map_err(|_| t("Could not save the ReShade preset selection."))?;
    Ok(path.into())
}

pub(crate) fn editable(app: &App) -> bool {
    app.preview
        || app.vault_ready
            && !app
                .accounts
                .iter()
                .any(|r| r.busy || r.batch_pending || r.pid.is_some())
            && app.maintenance.is_none()
            && !app.reshade.busy
}

pub(crate) fn update(app: &mut App, msg: Msg) -> Effect<Message> {
    if matches!(msg, Msg::Toggle(_) | Msg::Setup | Msg::ChoosePreset)
        && !app.preview
        && (!editable(app) || app.has_sessions())
    {
        app.status =
            t("Close games and sign out PlayOnline sessions before changing ReShade.").into();
        return Effect::none();
    }
    match msg {
        Msg::Refresh if !app.preview && !app.reshade.busy => {
            app.reshade.generation = app.reshade.generation.wrapping_add(1);
            let generation = app.reshade.generation;
            let check = Effect::perform(
                async {
                    tokio::task::spawn_blocking(|| status(&root()))
                        .await
                        .unwrap_or_default()
                },
                move |(ready, preset)| message(Msg::Checked(generation, ready, preset)),
            );
            // Graphics opened: the dgVoodoo card looks for other copies at the same time.
            return Effect::batch([check, crate::dgvoodoo::update(app, crate::dgvoodoo::Msg::Scan)]);
        }
        Msg::Checked(generation, ready, preset)
            if generation == app.reshade.generation && !app.reshade.busy =>
        {
            app.reshade.installed = ready;
            app.reshade.preset = preset;
        }
        Msg::Toggle(on) if editable(app) => {
            if on && (!app.settings.dgvoodoo || !app.reshade.installed) {
                app.status =
                    t("Enable dgVoodoo and finish ReShade setup before turning ReShade on.").into();
            } else {
                app.settings.reshade.enabled = on;
                if !app.preview {
                    if let Err(error) = crate::save_settings(&app.config, &app.settings) {
                        app.status = error;
                    }
                }
            }
        }
        Msg::Setup if editable(app) && app.settings.dgvoodoo && !app.preview => {
            app.reshade.generation = app.reshade.generation.wrapping_add(1);
            app.reshade.busy = true;
            app.reshade.error = None;
            app.reshade.status = t("Downloading official ReShade setup…").into();
            return Effect::perform(async { setup_download(&root()).await }, |result| {
                message(Msg::Downloaded(result))
            });
        }
        Msg::Downloaded(Ok(setup)) if app.reshade.busy => {
            app.reshade.status =
                t("Choose effects in ReShade setup, click Finish, then close its window.").into();
            return Effect::perform(
                async move {
                    tokio::task::spawn_blocking(move || run_setup(&root(), &setup))
                        .await
                        .unwrap_or_else(|_| Err(t("ReShade setup stopped unexpectedly.").into()))
                },
                |result| message(Msg::Finished(result)),
            );
        }
        Msg::Downloaded(Err(error)) | Msg::Finished(Err(error)) => {
            app.reshade.busy = false;
            app.reshade.status.clear();
            app.reshade.error = Some(error);
        }
        Msg::Finished(Ok(())) => {
            app.reshade.busy = false;
            app.reshade.installed = true;
            app.reshade.status = t("ReShade is ready. Turn it on for fresh game launches; Home opens its in-game settings.").into();
            return update(app, Msg::Refresh);
        }
        Msg::ChoosePreset if editable(app) && app.reshade.installed && !app.preview => {
            app.reshade.generation = app.reshade.generation.wrapping_add(1);
            app.reshade.busy = true;
            app.reshade.error = None;
            return Effect::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .add_filter("ReShade preset", &["ini"])
                        .pick_file()
                        .await
                        .map(|f| f.path().to_path_buf())
                },
                |path| message(Msg::Picked(path)),
            );
        }
        Msg::Picked(Some(path)) if app.reshade.busy => {
            return Effect::perform(
                async move {
                    tokio::task::spawn_blocking(move || save_preset(&root(), &path))
                        .await
                        .unwrap_or_else(|_| {
                            Err(t("Could not save the ReShade preset selection.").into())
                        })
                },
                |result| message(Msg::PresetSaved(result)),
            )
        }
        Msg::Picked(None) => app.reshade.busy = false,
        Msg::PresetSaved(result) => {
            app.reshade.busy = false;
            match result {
                Ok(path) => {
                    app.reshade.preset = path;
                    app.reshade.status =
                        t("Preset selected. Use ReShade setup to install any effects it needs.")
                            .into();
                }
                Err(error) => app.reshade.error = Some(error),
            }
        }
        Msg::OpenFolder if !app.preview => {
            match cache(&root()).and_then(|dir| {
                crate::shell_open(&dir.to_string_lossy())
                    .map_err(|_| t("Could not open the ReShade folder.").to_owned())
            }) {
                Ok(()) => (),
                Err(error) => app.status = error,
            }
        }
        Msg::Website if !app.preview => {
            if let Err(error) = crate::open_url("https://reshade.me/") {
                app.status = error;
            }
        }
        _ => (),
    }
    Effect::none()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let base = std::env::var_os("CRYSTAL_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "reshade-{name}-{}",
            crate::backend::unique_id().unwrap()
        ));
        assert!(path.is_absolute());
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn image(machine: u16, marker: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&machine.to_le_bytes());
        bytes[127] = marker;
        bytes
    }

    #[test]
    fn opt_in_settings_and_launch_plans_round_trip() {
        let mut settings: crate::Settings = serde_json::from_str(r#"{"install_path":""}"#).unwrap();
        assert!(!settings.reshade.enabled);
        settings.reshade.enabled = true;
        let settings: crate::Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        let mut account = crate::blank_account();
        for route in [
            crate::backend::LaunchWith::Vanilla,
            crate::backend::LaunchWith::Windower,
            crate::backend::LaunchWith::Ashita,
        ] {
            account.launch_with = Some(route);
            let plan = settings.launch_plan(&account).unwrap();
            let plan: crate::pol_less::Plan =
                serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
            assert!(plan.settings.reshade.enabled && plan.settings.dgvoodoo);
        }
    }

    #[test]
    fn off_does_no_file_work_and_missing_wrapper_is_rejected() {
        let path = Path::new(r"C:\missing\pol.exe");
        assert_eq!(
            loader_for_launch(path, &Preferences::default(), false).unwrap(),
            path
        );
        assert!(loader_for_launch(path, &Preferences { enabled: true }, false).is_err());
    }

    #[test]
    fn host_bitness_is_checked_without_loading_the_image() {
        assert!(x86_image(&image(0x14c, 1)));
        assert!(!x86_image(&image(0x8664, 1)));
        assert!(!x86_image(b"not a PE"));
        let mut malformed = image(0x14c, 1);
        malformed[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!x86_image(&malformed));
    }

    #[test]
    fn official_setup_targets_x86_host_with_dxgi_and_no_automatic_effect_choices() {
        let root = temp("arguments");
        let setup = root.join("ReShade_Setup.exe");
        let host = root.join("Crystal-ReShade-Setup.exe");
        let command = setup_command(&setup, &host);
        assert_eq!(command.get_program(), setup);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [host.as_os_str(), "--api".as_ref(), "dxgi".as_ref()]
        );
        assert_eq!(command.get_current_dir(), Some(root.as_path()));
    }

    #[test]
    fn separate_host_preserves_existing_renderer_files_and_shared_settings() {
        let root = temp("separate");
        let original = root.join("pol.exe");
        let host = image(0x14c, 1);
        fs::write(&original, &host).unwrap();
        fs::write(root.join("dxgi.dll"), b"another tool's proxy").unwrap();
        fs::write(root.join("D3D8.dll"), b"another tool's wrapper").unwrap();
        let config = cache(&root).unwrap().join("ReShade.ini");
        fs::write(&config, b"[GENERAL]\nPresetPath=keep.ini\n").unwrap();
        let dll = image(0x14c, 2);
        let loader = variant(&root, &host, &dll).unwrap();
        assert_ne!(loader, original);
        assert_eq!(loader.file_name().unwrap(), "pol.exe");
        assert_eq!(fs::read(&loader).unwrap(), host);
        assert_eq!(fs::read(loader.with_file_name("dxgi.dll")).unwrap(), dll);
        assert!(fs::read_to_string(loader.with_file_name("ReShade.ini"))
            .unwrap()
            .contains(&format!("BasePath={}", folder(&root).display())));
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "[GENERAL]\nPresetPath=keep.ini\n"
        );
        assert_eq!(fs::read(&original).unwrap(), host);
        assert_eq!(
            fs::read(root.join("dxgi.dll")).unwrap(),
            b"another tool's proxy"
        );
        assert_eq!(
            fs::read(root.join("D3D8.dll")).unwrap(),
            b"another tool's wrapper"
        );
    }

    #[test]
    fn simultaneous_clients_reuse_the_same_complete_host_without_rewriting_it() {
        let root = temp("concurrent");
        let host = image(0x14c, 1);
        let dll = image(0x14c, 2);
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (root, host, dll) = (root.clone(), host.clone(), dll.clone());
                std::thread::spawn(move || variant(&root, &host, &dll).unwrap())
            })
            .collect();
        let paths: Vec<_> = threads
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect();
        assert!(paths.iter().all(|path| path == &paths[0]));
        let before = fs::metadata(&paths[0]).unwrap().modified().unwrap();
        assert_eq!(variant(&root, &host, &dll).unwrap(), paths[0]);
        assert_eq!(fs::metadata(&paths[0]).unwrap().modified().unwrap(), before);
        assert_eq!(fs::read_dir(paths[0].parent().unwrap()).unwrap().count(), 3);
    }

    #[test]
    fn changed_host_gets_another_directory_and_conflicting_files_are_kept() {
        let root = temp("upgrade");
        let old = image(0x14c, 1);
        let new = image(0x14c, 3);
        let dll = image(0x14c, 2);
        let first = variant(&root, &old, &dll).unwrap();
        let second = variant(&root, &new, &dll).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), old);
        fs::write(&second, b"unexpected host").unwrap();
        assert!(variant(&root, &new, &dll).is_err());
        assert_eq!(fs::read(&second).unwrap(), b"unexpected host");
    }

    #[test]
    fn older_host_folders_are_pruned_and_live_or_foreign_ones_kept() {
        let root = temp("prune");
        let dll = image(0x14c, 2);
        let current = variant(&root, &image(0x14c, 1), &dll).unwrap();
        let hosts = current.parent().unwrap().parent().unwrap().to_path_buf();
        let make = |name: &str, age: u64| {
            let dir = hosts.join(name);
            fs::create_dir_all(&dir).unwrap();
            let when = std::time::SystemTime::now() - Duration::from_secs(age);
            let file = File::create(dir.join("pol.exe")).unwrap();
            file.set_modified(when).unwrap();
            age_dir(&dir, when);
            dir
        };
        fn age_dir(dir: &Path, when: std::time::SystemTime) {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS opens a folder.
            OpenOptions::new().access_mode(0x0100).custom_flags(0x0200_0000).open(dir).unwrap().set_modified(when).unwrap();
        }
        // A folder another launch is still preparing: no pol.exe yet, but new.
        let preparing = hosts.join("00000000000000aa-0123456789abcdef");
        fs::create_dir_all(&preparing).unwrap();
        fs::write(preparing.join("ReShade.ini"), b"x").unwrap();
        // An old folder whose pol.exe went missing.
        let stale = hosts.join("00000000000000bb-0123456789abcdef");
        fs::create_dir_all(&stale).unwrap();
        age_dir(&stale, std::time::SystemTime::now() - Duration::from_secs(7200));
        let old = make("0123456789abcdef-0123456789abcdef", 7200);
        let recent = make("fedcba9876543210-0123456789abcdef", 10);
        let foreign = make("my-notes", 7200);
        prune_hosts(current.parent().unwrap());
        assert!(!old.exists());
        assert!(!stale.exists());
        assert!(preparing.join("ReShade.ini").exists());
        assert!(recent.join("pol.exe").exists());
        assert!(foreign.join("pol.exe").exists());
        assert!(current.exists());
        assert!(host_folder_name("0123456789abcdef-0123456789abcdef"));
        assert!(!host_folder_name("0123456789abcdef_0123456789abcdef"));
    }

    #[test]
    fn preset_copy_preserves_source_other_settings_and_previous_presets() {
        let root = temp("preset");
        let dir = cache(&root).unwrap();
        let config = dir.join("ReShade.ini");
        fs::write(&config, "\u{feff}; user comment\n[GENERAL]\nPresetPath=old.ini\nEffectSearchPaths=.\\reshade-shaders\\Shaders\\**\n[INPUT]\nKeyOverlay=36,0,0,0\n").unwrap();
        let source = root.join("user preset.ini");
        let content = b"Techniques=Sharpen@Example.fx\n[Example.fx]\nStrength=0.25\n";
        fs::write(&source, content).unwrap();
        let selected = save_preset(&root, &source).unwrap();
        assert_ne!(Path::new(&selected), source);
        assert_eq!(fs::read(&source).unwrap(), content);
        assert_eq!(fs::read(&selected).unwrap(), content);
        let edited = fs::read_to_string(config).unwrap();
        assert!(edited.contains("; user comment"));
        assert!(edited.contains("EffectSearchPaths=.\\reshade-shaders\\Shaders\\**"));
        assert!(edited.contains("KeyOverlay=36,0,0,0"));
        assert!(edited.contains(&format!("PresetPath={selected}")));
        assert_eq!(current_preset(&root), selected);
        assert_eq!(save_preset(&root, &source).unwrap(), selected);
    }

    #[test]
    fn configuration_and_oversized_presets_are_rejected_without_changing_settings() {
        let root = temp("bad-preset");
        let config = cache(&root).unwrap().join("ReShade.ini");
        let initial = b"[GENERAL]\nPresetPath=keep.ini\n";
        fs::write(&config, initial).unwrap();
        let source = root.join("wrong.ini");
        fs::write(&source, b"[GENERAL]\nPresetPath=not-a-preset.ini\n").unwrap();
        assert!(save_preset(&root, &source).is_err());
        fs::write(&source, vec![b'X'; MAX_INI as usize + 1]).unwrap();
        assert!(save_preset(&root, &source).is_err());
        assert_eq!(fs::read(config).unwrap(), initial);
    }

    #[test]
    fn preset_key_edit_handles_duplicate_sections_and_preserves_other_keys() {
        let output = set_preset("[GENERAL]\npresetpath=one.ini\nA=1\n[OTHER]\nPresetPath=unrelated\n[general]\nPresetPath=two.ini\nB=2", "new.ini");
        assert_eq!(output.matches("PresetPath=new.ini").count(), 1);
        assert!(
            output.contains("A=1")
                && output.contains("B=2")
                && output.contains("PresetPath=unrelated")
        );
        assert!(!output.contains("one.ini") && !output.contains("two.ini"));
    }

    #[test]
    fn changed_or_64_bit_runtime_does_not_count_as_installed() {
        let root = temp("wrong-runtime");
        let dir = cache(&root).unwrap();
        fs::write(dir.join("ReShade.ini"), b"[GENERAL]\n").unwrap();
        for data in [image(0x14c, 1), image(0x8664, 1)] {
            fs::write(dir.join("dxgi.dll"), data).unwrap();
            assert!(!installed(&root));
        }
    }

    #[test]
    #[ignore = "requires the official ReShade setup fixture; no game login"]
    fn official_setup_fixture_has_the_pinned_x86_runtime_and_configuration() {
        let root = PathBuf::from(
            std::env::var_os("CRYSTAL_RESHADE_FIXTURE").expect("fixture path required"),
        );
        let setup = regular_file(&root.join("ReShade_Setup_6.8.0.exe"), MAX_BINARY).unwrap();
        assert_eq!(digest(&setup), SETUP_HASH);
        let dll = regular_file(
            &root.join("official-setup-fixture").join("dxgi.dll"),
            MAX_BINARY,
        )
        .unwrap();
        assert!(x86_image(&dll));
        assert_eq!(digest(&dll), DLL_HASH);
        ini_text(
            &regular_file(
                &root.join("official-setup-fixture").join("ReShade.ini"),
                MAX_INI,
            )
            .unwrap(),
        )
        .unwrap();
    }
}
