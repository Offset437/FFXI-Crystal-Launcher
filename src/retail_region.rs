//! Experimental EU and JP retail sign-in (owner approval 2026-11-12).
//!
//! US stays exactly as verified: no value here changes a US launch. An EU or JP account
//! signs in with its own install's values, and one launch-log line lists them all. Every
//! value below was read from Square Enix's own files: the three official installers
//! (FFXIFullSetup_US / _EU / _JP: the Registry and File tables of PlayOnlineViewer.msi and
//! FINAL_FANTASY_XI.msi, and their app, polcore, env.dat and sqpolcts.bin files, statically
//! unpacked and decoded, never run) and the installed US Viewer.
//!
//! | value | US | EU | JP |
//! |---|---|---|---|
//! | registry root (HKLM, 32-bit view) | `SOFTWARE\PlayOnlineUS` | `SOFTWARE\PlayOnlineEU` | `SOFTWARE\PlayOnline` |
//! | Viewer folder | `InstallFolder\1000` | same | same |
//! | game folder | `InstallFolder\0001` | same | same |
//! | publisher key | `SquareEnix` | `SquareEnix` | `SQUARE` |
//! | core in `viewer\com` | `polcore.dll` | `polcoreEU.dll` | `polcore.dll` |
//! | Viewer app in `viewer\com` | `app.dll` | `appEU.dll` | `app.dll` |
//! | core CLSID | `{3501F5DD-…}` | `{E5966FB3-…}` | `{07974581-…}` |
//! | env.dat | `data\utf\env.dat` | `EU\env.dat`, else `data\utf\env.dat` | `data\utf\env.dat` |
//! | `PATCH_SERVER_DOMAIN` (installer) | `pt008.pol.com` | `pt008.pol.com` | `pt004.pol.com` |
//! | `POL_UCS_AREA_KBN` (installer) | `01` | `02` | `00` |
//! | Viewer check tag (title 1000) | `W2U` | `W2U` | `W20` |
//! | game check tag (title 0001) | `W20` | `W20` | `W20` |
//! | game check host | `pc001W2U.pol.com` | `pc001W2U.pol.com` | `pc001W20.pol.com` |
//! | member login host | `ci000.pol.com` | same | same |
//!
//! The US, EU and JP cores are one build apart from their COM identifiers, and the three
//! Viewer apps share their code and strings, so the env.dat key, the function table and the
//! member login host are the same in all three.
//!
//! The Viewer check asks the server about the Viewer's own version, and the US/EU and the
//! Japanese Viewers are separate products on the patch server: asking as `W2U` for a Japanese
//! Viewer returns the US/EU Viewer's version. The tag is therefore read from the install's
//! own contents table (`data\doc\sqpolcts.bin`), the file the stock Viewer reads it from. An
//! install whose table cannot be read is not checked and not refused.
//!
//! EU/JP hosts never park, and an EU/JP update is left to the official PlayOnline Viewer:
//! Crystal's updater deploys the US Viewer's files (`foreign_viewer`).

use crate::i18n::{t, tf};
use crate::win::reg;
use std::path::{Path, PathBuf};

/// A retail install's region, in Crystal's account order (`Account::region`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetailRegion { Us, Eu, Jp }

impl RetailRegion {
    pub(crate) const ALL: [Self; 3] = [Self::Us, Self::Eu, Self::Jp];
    /// 0 US, 1 EU, 2 JP (`win::reg::REGIONS`, the account editor's order).
    pub(crate) fn from_account(region: u8) -> Option<Self> {
        match region { 0 => Some(Self::Us), 1 => Some(Self::Eu), 2 => Some(Self::Jp), _ => None }
    }
    pub(crate) fn index(self) -> u8 {
        match self { Self::Us => 0, Self::Eu => 1, Self::Jp => 2 }
    }
    /// The session/park line's `region` (crystal-loader proto.rs).
    pub(crate) fn wire(self) -> &'static str {
        match self { Self::Us => "US", Self::Eu => "EU", Self::Jp => "JP" }
    }
    /// `SOFTWARE\<root>`: PlayOnlineUS, PlayOnlineEU, PlayOnline.
    pub(crate) fn registry_root(self) -> &'static str {
        reg::REGIONS[usize::from(self.index())].1
    }
    /// The regional core in `viewer\com` (crystal-loader `game::polcore_name`).
    pub(crate) fn polcore(self) -> &'static str {
        if self == Self::Eu { "polcoreEU.dll" } else { "polcore.dll" }
    }
    /// The core's COM class (each region's PlayOnlineViewer.msi registers it with
    /// `InprocServer32` = the core's full path; crystal-loader game.rs `CLSID_POLCORE`).
    fn core_clsid(self) -> &'static str {
        match self {
            Self::Us => "{3501F5DD-7894-42DF-866A-A2B6527D8049}",
            Self::Eu => "{E5966FB3-C97B-42EB-84BF-37F95EE54A9F}",
            Self::Jp => "{07974581-0DF6-4EF0-BD05-604B3ADA9BE9}",
        }
    }
    /// The env.dat files the stock Viewer opens for this region, under the Viewer folder, in
    /// its order (app 10019717-100197b7): an EU Viewer tries `EU\env.dat` in its own folder
    /// and falls back to `data\utf\env.dat` when that does not open; the others read
    /// `data\utf\env.dat`. The EU installer puts its file at `PlayOnlineViewer\EU\env.dat`.
    pub(crate) fn env_dat(self) -> &'static [&'static str] {
        if self == Self::Eu { &[r"EU\env.dat", r"data\utf\env.dat"] } else { &[r"data\utf\env.dat"] }
    }
    /// The name this region's Viewer looks up for the game's version check (`dns`).
    pub(crate) fn game_patch_host(self) -> &'static str {
        if self == Self::Jp { crate::dns::GAME_PATCH_HOST_JP } else { crate::dns::GAME_PATCH_HOST }
    }
}

/// The game's patch host for an account's region number (US for anything unknown).
pub(crate) fn game_patch_host(account_region: u8) -> &'static str {
    RetailRegion::from_account(account_region).unwrap_or(RetailRegion::Us).game_patch_host()
}

/// Whether the US Viewer's local readiness (`viewer::gate`: installed, updated, no
/// interrupted update, a readable core) decides this sign-in: always, except for a retail
/// EU or JP account, which uses its own region's Viewer and nothing of the US one.
pub(crate) fn us_viewer_gates(retail: bool, account_region: u8) -> bool {
    !(retail && matches!(RetailRegion::from_account(account_region), Some(RetailRegion::Eu | RetailRegion::Jp)))
}

/// The game check's gate for an account's region: unchanged for US; an EU/JP "update
/// required" is reworded for the official Viewer (`official_game_update`).
pub(crate) fn game_gate(account_region: u8, gated: Result<String, String>) -> Result<String, String> {
    match (RetailRegion::from_account(account_region), gated) {
        (Some(region @ (RetailRegion::Eu | RetailRegion::Jp)), Err(message)) => Err(official_game_update(region.wire(), &message).unwrap_or(message)),
        (_, gated) => gated,
    }
}

/// An EU or JP install's own game folder: its region's `InstallFolder\0001` (the value its
/// FINAL_FANTASY_XI.msi writes) holding the game's two modules. `None` for US, whose folder
/// Crystal finds as it always has.
pub(crate) fn game_folder(account_region: u8) -> Option<PathBuf> {
    let region = RetailRegion::from_account(account_region).filter(|region| *region != RetailRegion::Us)?;
    reg::install_folders("0001", &[(region.index(), region.registry_root())]).into_iter().map(|(_, path)| path)
        .find(|path| ["FFXi.dll", "FFXiMain.dll"].iter().all(|name| path.join(name).is_file()))
}

/// The Viewer's contents table, under the Viewer folder (polcore 1458 kind 2).
const CONTENTS: &str = r"data\doc\sqpolcts.bin";

/// The values an experimental (EU/JP) sign-in uses that differ from a US one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Experimental {
    pub region: RetailRegion,
    /// The region's PlayOnline Viewer folder.
    pub viewer: PathBuf,
    /// The registry value that named `viewer` (`HKLM\SOFTWARE\WOW6432Node\…\InstallFolder\1000`).
    pub source: String,
    pub env_file: PathBuf,
    /// `PATCH_SERVER_DOMAIN`: the Viewer check's (:54000) host.
    pub patch_host: String,
    /// `POL_UCS_AREA_KBN`: slot 1309's argument, sent as the loader's `ucs_area`.
    pub ucs_area: String,
    /// The Viewer check's platform tag from this install's own contents table (`W2U\0` or
    /// `W20\0`), or why it is unknown: no check is sent then, and the sign-in is not refused.
    pub viewer_tag: Result<[u8; 4], String>,
}

/// What a sign-in targets: the verified US install, or an experimental EU/JP one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Target {
    #[default]
    Us,
    Experimental(Experimental),
}

impl Target {
    /// The target for an account's region: US as always; EU/JP read from their install, or
    /// refused with every place that was checked.
    pub(crate) fn for_account(region: u8) -> Result<Self, String> {
        match RetailRegion::from_account(region) {
            Some(RetailRegion::Us) => Ok(Self::Us),
            Some(region) => {
                let (viewer, source) = choose_viewer(region, &registered_viewers(region)).map_err(|error| {
                    // Another edition installed here: say which, so the account's region is set to it.
                    let us = crate::discovery::find(crate::discovery::Product::PlayOnline, None);
                    mismatch(region.index(), &installed_editions(us.as_deref())).unwrap_or(error)
                })?;
                Experimental::read(region, &viewer, source).map(Self::Experimental)
            }
            None => Err(t("The account's region is not US, EU or JP.").into()),
        }
    }
    pub(crate) fn experimental(&self) -> Option<&Experimental> {
        match self { Self::Us => None, Self::Experimental(values) => Some(values) }
    }
    /// The launch log's one line of region values (none for US). Nothing secret.
    pub(crate) fn log_lines(&self) -> Vec<String> {
        self.experimental().map(Experimental::summary).into_iter().collect()
    }
}

impl Experimental {
    /// Reads `region`'s values under `viewer`, which `source` named.
    pub(crate) fn read(region: RetailRegion, viewer: &Path, source: String) -> Result<Self, String> {
        // The first env.dat that opens, as the stock Viewer picks it.
        let mut opened = None;
        let mut failures = Vec::new();
        for relative in region.env_dat() {
            let path = viewer.join(relative);
            match read_small(&path) {
                Ok(cipher) => { opened = Some((path, cipher)); break; }
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        let Some((env_file, cipher)) = opened else {
            return Err(tf("PlayOnline ({region}): cannot read env.dat ({tried}).", &[("region", &region.wire()), ("tried", &failures.join("; "))]));
        };
        let body = crate::patch::decode_env_dat(&cipher).map_err(|error| tf("PlayOnline ({region}): {path} could not be decoded ({error}).",
            &[("region", &region.wire()), ("path", &env_file.display()), ("error", &error)]))?;
        let (patch_host, ucs_area) = parse_env(&body).map_err(|key| tf("PlayOnline ({region}): {path} has no usable {key}.",
            &[("region", &region.wire()), ("path", &env_file.display()), ("key", &key)]))?;
        Ok(Self { region, viewer: viewer.to_path_buf(), source, env_file, patch_host, ucs_area, viewer_tag: viewer_tag(viewer) })
    }

    /// The Viewer check as the log and a refusal name it: `pt004.pol.com:54000 W20/1000`.
    pub(crate) fn viewer_check(&self) -> Option<String> {
        let tag = self.viewer_tag.as_ref().ok()?;
        Some(format!("{}:{} {}/1000", self.patch_host, crate::updater::Product::Viewer.port(), tag_text(tag)))
    }

    /// One line: region, where the Viewer folder was registered, the folder, its core, its
    /// env.dat with the two settings read from it, both version checks, and what is left out.
    fn summary(&self) -> String {
        let name = self.region.wire();
        let relative = |path: &Path| path.strip_prefix(&self.viewer).unwrap_or(path).display().to_string();
        let viewer_check = match (&self.viewer_tag, self.viewer_check()) {
            (_, Some(check)) => format!("Viewer check {check} (tag from {CONTENTS})"),
            (Err(why), _) => format!("Viewer check not sent ({CONTENTS}: {why})"),
            (Ok(_), None) => "Viewer check not sent".into(),
        };
        format!("Region {name} (experimental): Viewer {} [{}]; core viewer\\com\\{}; {}: PATCH_SERVER_DOMAIN {}, POL_UCS_AREA_KBN {}; {viewer_check}; FFXI check {}:{} W20/0001; not parked; updates by the official PlayOnline Viewer.",
            self.viewer.display(), self.source, self.region.polcore(), relative(&self.env_file), self.patch_host, self.ucs_area,
            self.region.game_patch_host(), crate::updater::Product::Ffxi.port())
    }
}

/// A platform tag as text (`W2U`).
fn tag_text(tag: &[u8; 4]) -> String {
    String::from_utf8_lossy(tag).trim_end_matches('\0').to_owned()
}

/// The Viewer check's platform tag for the Viewer in `viewer`: its contents table's flag for
/// title 1000 (`patch::contents_platform_flag`), 1 → `W2U`, 0 → `W20`, as the stock Viewer's
/// tag picker maps it (app 1027b731, no `DebugPatch` key).
fn viewer_tag(viewer: &Path) -> Result<[u8; 4], String> {
    let cipher = read_small(&viewer.join(CONTENTS)).map_err(|error| error.to_string())?;
    match crate::patch::contents_platform_flag(&cipher, 1000)? {
        1 => Ok(*b"W2U\0"),
        0 => Ok(*b"W20\0"),
        other => Err(format!("title 1000 has platform flag {other}, not 0 or 1")),
    }
}

/// A PlayOnline settings file (a few KB); never more than 64 KB.
fn read_small(path: &Path) -> std::io::Result<Vec<u8>> {
    if std::fs::metadata(path)?.len() > 64 * 1024 {
        return Err(std::io::Error::other("the file is larger than 64 KB"));
    }
    std::fs::read(path)
}

/// An EU/JP W20's "update required" refusal reworded for the official Viewer: not the US
/// wording, so the window never offers Crystal's Update (which deploys US files). `None`
/// for any other message.
pub(crate) fn official_game_update(region: &str, message: &str) -> Option<String> {
    let report = crate::patch::parse_update_required(message)?;
    Some(tf("FINAL FANTASY XI ({region}) needs an update (installed {local}, official {remote}). Update with the official PlayOnline Viewer.",
        &[("region", &region), ("local", &report.local_version), ("remote", &report.remote_version)]))
}

/// `NAME,value` lines (`\r\n`): the two settings an EU/JP sign-in needs, validated. `Err`
/// names the first missing or unusable one.
fn parse_env(body: &[u8]) -> Result<(String, String), &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "PATCH_SERVER_DOMAIN")?;
    let value = |key: &str| text.lines().find_map(|line| line.trim_end_matches('\r').split_once(',').filter(|(name, _)| *name == key).map(|(_, v)| v.trim()));
    let host = value("PATCH_SERVER_DOMAIN").filter(|host| valid_patch_host(host)).ok_or("PATCH_SERVER_DOMAIN")?;
    let area = value("POL_UCS_AREA_KBN").filter(|area| area.len() == 2 && area.bytes().all(|b| b.is_ascii_digit())).ok_or("POL_UCS_AREA_KBN")?;
    Ok((host.to_ascii_lowercase(), area.to_owned()))
}

/// A Square Enix PlayOnline host name (`*.pol.com`), nothing else: a damaged or foreign
/// file never sends the Viewer's version check anywhere else.
fn valid_patch_host(host: &str) -> bool {
    host.len() <= 253
        && host.to_ascii_lowercase().strip_suffix(".pol.com").is_some_and(|label| !label.is_empty())
        && host.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !label.starts_with('-') && !label.ends_with('-'))
}

/// One registry value that may name a region's Viewer folder: where it is, and the Viewer
/// folder it gives (`None` when the value is not set).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Registered {
    source: String,
    folder: Option<PathBuf>,
}

/// Every place `region`'s Viewer folder is registered, best first: its `InstallFolder\1000`
/// (HKLM 32-bit view, where its 32-bit installer writes; HKLM 64-bit view; HKCU), the folder
/// of its registered core (the COM class the installer registers for `viewer\com\<core>`),
/// and `PlayOnlineViewer` beside its game folder (`InstallFolder\0001`; both installers put
/// the two under one `SquareEnix` folder).
fn registered_viewers(region: RetailRegion) -> Vec<Registered> {
    use reg::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY};
    let root = region.registry_root();
    let text = |hive, key: &str, name: &str, view| reg::string(hive, key, name, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | view, 2048)
        .map(|units| PathBuf::from(crate::win::from_wide(&units))).filter(|path| !path.as_os_str().is_empty());
    let install = format!(r"SOFTWARE\{root}\InstallFolder");
    let class = format!(r"SOFTWARE\Classes\CLSID\{}\InprocServer32", region.core_clsid());
    let wow_class = format!(r"SOFTWARE\Classes\WOW6432Node\CLSID\{}\InprocServer32", region.core_clsid());
    // `…\PlayOnlineViewer\viewer\com\<core>` → `…\PlayOnlineViewer`.
    let from_core = |core: PathBuf| core.ancestors().nth(3).map(Path::to_path_buf);
    let beside_game = |game: PathBuf| {
        let trimmed = PathBuf::from(game.to_string_lossy().trim_end_matches(['\\', '/']).to_owned());
        trimmed.parent().map(|parent| parent.join("PlayOnlineViewer"))
    };
    vec![
        Registered { source: format!(r"HKLM\SOFTWARE\WOW6432Node\{root}\InstallFolder\1000"), folder: text(HKEY_LOCAL_MACHINE, &install, "1000", RRF_SUBKEY_WOW6432KEY) },
        Registered { source: format!(r"HKLM\SOFTWARE\{root}\InstallFolder\1000"), folder: text(HKEY_LOCAL_MACHINE, &install, "1000", RRF_SUBKEY_WOW6464KEY) },
        Registered { source: format!(r"HKCU\SOFTWARE\{root}\InstallFolder\1000"), folder: text(HKEY_CURRENT_USER, &install, "1000", RRF_SUBKEY_WOW6432KEY)
            .or_else(|| text(HKEY_CURRENT_USER, &install, "1000", RRF_SUBKEY_WOW6464KEY)) },
        Registered { source: format!(r"HKLM\{wow_class}"), folder: text(HKEY_LOCAL_MACHINE, &class, "", RRF_SUBKEY_WOW6432KEY).and_then(from_core) },
        Registered { source: format!(r"HKCU\{wow_class}"), folder: text(HKEY_CURRENT_USER, &class, "", RRF_SUBKEY_WOW6432KEY).and_then(from_core) },
        Registered { source: format!(r"HKLM\SOFTWARE\WOW6432Node\{root}\InstallFolder\0001 (PlayOnlineViewer beside the game)"),
            folder: text(HKEY_LOCAL_MACHINE, &install, "0001", RRF_SUBKEY_WOW6432KEY).and_then(beside_game) },
    ]
}

/// The first registered folder that holds `region`'s own core, with the value that named it.
/// `Err` lists every value and what was wrong with it, so the status line alone tells which
/// assumption failed on that PC.
fn choose_viewer(region: RetailRegion, registered: &[Registered]) -> Result<(PathBuf, String), String> {
    let core = region.polcore();
    let mut checked = Vec::new();
    for entry in registered {
        let Some(folder) = &entry.folder else {
            checked.push(format!("{}: not set", entry.source));
            continue;
        };
        let folder = PathBuf::from(folder.to_string_lossy().trim_end_matches(['\\', '/']).to_owned());
        match core_problem(&folder, core) {
            None => return Ok((folder, entry.source.clone())),
            Some(problem) => checked.push(format!("{} = {}: {problem}", entry.source, folder.display())),
        }
    }
    Err(tf("PlayOnline ({region}) was not found: no registered folder holds viewer\\com\\{core}. Checked: {checked}.",
        &[("region", &region.wire()), ("core", &core), ("checked", &checked.join("; "))]))
}

/// Why `folder` is not a Viewer folder with `core`: `None` when `viewer\com\<core>` is there.
fn core_problem(folder: &Path, core: &str) -> Option<String> {
    if !folder.is_absolute() { return Some("not an absolute path".into()); }
    if !folder.is_dir() { return Some("the folder does not exist".into()); }
    let com = folder.join("viewer").join("com");
    if com.join(core).is_file() { return None; }
    let Ok(entries) = std::fs::read_dir(&com) else { return Some("it has no viewer\\com folder".into()) };
    let mut names: Vec<String> = entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.to_ascii_lowercase().ends_with(".dll")).take(8).collect();
    names.sort();
    Some(if names.is_empty() { format!("viewer\\com has no {core} (no DLL there)") } else { format!("viewer\\com has no {core} (it holds {})", names.join(", ")) })
}

/// The edition of the PlayOnline Viewer in `folder`, from its own files: EU when it holds
/// the EU core and not `polcore.dll`; else what its `data\utf\env.dat` says
/// (`POL_UCS_AREA_KBN` 00 Japan, 01 US). `None` when the files do not tell. A folder that
/// holds both cores (the US and EU installers share a default folder, and the EU one also
/// ships the US env.dat there) is a US Viewer as well, so it reads as US.
pub(crate) fn edition(folder: &Path) -> Option<RetailRegion> {
    let com = folder.join("viewer").join("com");
    if com.join(RetailRegion::Eu.polcore()).is_file() && !com.join(RetailRegion::Us.polcore()).is_file() {
        return Some(RetailRegion::Eu);
    }
    env_area(folder)
}

/// What `folder`'s `data\utf\env.dat` says: `POL_UCS_AREA_KBN` 00 Japan, 01 US.
fn env_area(folder: &Path) -> Option<RetailRegion> {
    let body = crate::patch::decode_env_dat(&read_small(&folder.join(r"data\utf\env.dat")).ok()?).ok()?;
    match parse_env(&body).ok()?.1.as_str() {
        "00" => Some(RetailRegion::Jp),
        "01" => Some(RetailRegion::Us),
        _ => None,
    }
}

/// The core of the Viewer in `folder`, whichever edition it is: `viewer\com\polcore.dll`
/// (US, JP) or `polcoreEU.dll` (EU). `Err` says plainly that neither is there.
pub(crate) fn core_file(folder: &Path) -> Result<PathBuf, String> {
    let com = folder.join("viewer").join("com");
    [RetailRegion::Us, RetailRegion::Eu].into_iter().map(|region| com.join(region.polcore())).find(|core| core.is_file())
        .ok_or_else(|| tf("PlayOnline's core file is missing from {folder}. Reinstall PlayOnline or check your antivirus.", &[("folder", &com.display())]))
}

/// The editions whose core `folder` holds: EU for `polcoreEU.dll`; for `polcore.dll`, JP when
/// its env.dat says so (or, unreadable, when the JP registry root named the folder), else US.
fn folder_editions(folder: &Path, named_by: Option<RetailRegion>) -> Vec<RetailRegion> {
    let com = folder.join("viewer").join("com");
    let mut found = Vec::new();
    if com.join(RetailRegion::Us.polcore()).is_file() {
        let jp = env_area(folder).map_or(named_by == Some(RetailRegion::Jp), |area| area == RetailRegion::Jp);
        found.push(if jp { RetailRegion::Jp } else { RetailRegion::Us });
    }
    if com.join(RetailRegion::Eu.polcore()).is_file() { found.push(RetailRegion::Eu); }
    found
}

/// The editions found in `folders` (each with the registry root that named it), in account order.
fn editions_in(folders: impl IntoIterator<Item = (PathBuf, Option<RetailRegion>)>) -> Vec<RetailRegion> {
    let mut editions = Vec::new();
    for (folder, named_by) in folders {
        for edition in folder_editions(&folder, named_by) {
            if !editions.contains(&edition) { editions.push(edition); }
        }
    }
    editions.sort_by_key(|region| region.index());
    editions
}

/// The PlayOnline editions installed on this PC: every Viewer folder each region's registry
/// names (`registered_viewers`), plus `found` (the US Viewer discovery located). Registry reads
/// and file checks only.
pub(crate) fn installed_editions(found: Option<&Path>) -> Vec<RetailRegion> {
    let registered = RetailRegion::ALL.into_iter()
        .flat_map(|region| registered_viewers(region).into_iter().filter_map(move |entry| entry.folder.map(|folder| (folder, Some(region)))));
    editions_in(found.map(|folder| (folder.to_path_buf(), None)).into_iter().chain(registered))
}

/// The region of the PlayOnline install in `folder`, which `named` (an account region) named:
/// `named` when the folder holds its core, else the folder's one edition.
pub(crate) fn folder_region(folder: &Path, named: u8) -> u8 {
    let editions = folder_editions(folder, RetailRegion::from_account(named));
    match editions.as_slice() {
        [only] if RetailRegion::from_account(named) != Some(*only) => only.index(),
        _ => named,
    }
}

/// The one installed edition, when exactly one is: a new account's region.
pub(crate) fn only_edition(installed: &[RetailRegion]) -> Option<RetailRegion> {
    match installed { [only] => Some(*only), _ => None }
}

/// An account whose region is not an installed edition: what to set it to. `None` when it
/// matches, or when no edition was found (nothing to compare with).
pub(crate) fn mismatch(account_region: u8, installed: &[RetailRegion]) -> Option<String> {
    let region = RetailRegion::from_account(account_region)?;
    if installed.is_empty() || installed.contains(&region) { return None; }
    Some(match only_edition(installed) {
        Some(edition) => tf("This PlayOnline is the {edition} version. Set this account to {edition}.", &[("edition", &edition.wire())]),
        None => tf("PlayOnline ({region}) is not installed. This PC has {editions}.", &[("region", &region.wire()),
            ("editions", &installed.iter().map(|edition| edition.wire()).collect::<Vec<_>>().join(", "))]),
    })
}

/// `folder` is an EU or Japanese PlayOnline Viewer: never the US Viewer Crystal checks,
/// gates on and updates (its updater deploys the US Viewer's files).
pub(crate) fn foreign_viewer(folder: &Path) -> Option<RetailRegion> {
    edition(folder).filter(|region| *region != RetailRegion::Us)
}

/// Why Crystal's updater leaves the Viewer in `folder` alone, when it is an EU or JP one.
pub(crate) fn refuse_update(folder: &Path) -> Result<(), String> {
    match foreign_viewer(folder) {
        Some(region) => Err(tf("{folder} is the {region} PlayOnline Viewer. Crystal updates the US Viewer only; update this one with the official PlayOnline Viewer.",
            &[("folder", &folder.display()), ("region", &region.wire())])),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings each region's official installer carries (FFXIFullSetup_US/EU/JP,
    /// PlayOnlineViewer.msi: `data\utf\env.dat`, `EU\env.dat`, `data\utf\env.dat`).
    const US_ENV: &[u8] = b"Version,1\r\nPATCH_SERVER_DOMAIN,pt008.pol.com\r\nPATCH_RESERVE_VAL,0000\r\nPOL_UCS_AREA_KBN,01\r\nPOL_UCS_LOGIN_PF,02\r\n";
    const EU_ENV: &[u8] = b"Version,1\r\nPATCH_SERVER_DOMAIN,pt008.pol.com\r\nPATCH_RESERVE_VAL,0000\r\nPOL_UCS_AREA_KBN,02\r\nPOL_UCS_LOGIN_PF,02\r\n";
    const JP_ENV: &[u8] = b"Version,1\r\nPATCH_SERVER_DOMAIN,pt004.pol.com\r\nPATCH_RESERVE_VAL,0000\r\nPOL_UCS_AREA_KBN,00\r\nPOL_UCS_LOGIN_PF,02\r\n";
    /// Title 1 (FFXI) and title 1000 (the Viewer) as each installer's sqpolcts.bin flags them.
    const US_CONTENTS: &[(u32, u8)] = &[(0, 1), (1, 0), (2, 1), (1000, 1)];
    const JP_CONTENTS: &[(u32, u8)] = &[(0, 0), (1, 0), (2, 0), (1000, 0)];

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-region-{tag}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    fn put(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    /// A Viewer folder laid out as `region`'s installer lays it out.
    fn viewer(root: &Path, region: RetailRegion) -> PathBuf {
        let folder = root.join(region.wire()).join("SquareEnix").join("PlayOnlineViewer");
        put(&folder, "pol.exe", b"");
        match region {
            RetailRegion::Us => {
                put(&folder, r"viewer\com\polcore.dll", b"");
                put(&folder, r"viewer\com\app.dll", b"");
                put(&folder, r"data\utf\env.dat", &crate::patch::encode_env_dat(US_ENV));
                put(&folder, CONTENTS, &crate::patch::encode_contents(US_CONTENTS));
            }
            RetailRegion::Eu => {
                put(&folder, r"viewer\com\polcoreEU.dll", b"");
                put(&folder, r"viewer\com\appEU.dll", b"");
                // The EU installer also ships the US file at data\utf\env.dat.
                put(&folder, r"data\utf\env.dat", &crate::patch::encode_env_dat(US_ENV));
                put(&folder, r"EU\env.dat", &crate::patch::encode_env_dat(EU_ENV));
                put(&folder, CONTENTS, &crate::patch::encode_contents(US_CONTENTS));
            }
            RetailRegion::Jp => {
                put(&folder, r"viewer\com\polcore.dll", b"");
                put(&folder, r"viewer\com\app.dll", b"");
                put(&folder, r"data\utf\env.dat", &crate::patch::encode_env_dat(JP_ENV));
                put(&folder, CONTENTS, &crate::patch::encode_contents(JP_CONTENTS));
            }
        }
        folder
    }
    fn entry(source: &str, folder: Option<&Path>) -> Registered {
        Registered { source: source.into(), folder: folder.map(Path::to_path_buf) }
    }

    #[test]
    fn region_tables_follow_crystals_account_order() {
        assert_eq!(RetailRegion::from_account(0), Some(RetailRegion::Us));
        assert_eq!(RetailRegion::from_account(1), Some(RetailRegion::Eu));
        assert_eq!(RetailRegion::from_account(2), Some(RetailRegion::Jp));
        assert_eq!(RetailRegion::from_account(3), None);
        let row = |r: RetailRegion| (r.wire(), r.registry_root(), r.polcore(), r.env_dat(), r.game_patch_host(), reg::publisher(r.registry_root()));
        assert_eq!(row(RetailRegion::Us), ("US", "PlayOnlineUS", "polcore.dll", &[r"data\utf\env.dat"][..], "pc001W2U.pol.com", "SquareEnix"));
        assert_eq!(row(RetailRegion::Eu), ("EU", "PlayOnlineEU", "polcoreEU.dll", &[r"EU\env.dat", r"data\utf\env.dat"][..], "pc001W2U.pol.com", "SquareEnix"));
        assert_eq!(row(RetailRegion::Jp), ("JP", "PlayOnline", "polcore.dll", &[r"data\utf\env.dat"][..], "pc001W20.pol.com", "SQUARE"));
        assert_eq!([0, 1, 2, 9].map(game_patch_host), ["pc001W2U.pol.com", "pc001W2U.pol.com", "pc001W20.pol.com", "pc001W2U.pol.com"]);
        // The loader creates the core with the same class (crystal-loader game.rs).
        assert_eq!(RetailRegion::Eu.core_clsid(), "{E5966FB3-C97B-42EB-84BF-37F95EE54A9F}");
        assert_eq!(RetailRegion::Jp.core_clsid(), "{07974581-0DF6-4EF0-BD05-604B3ADA9BE9}");
    }

    /// US: nothing here is read, sent or logged, and its checks are the stock ones byte for byte.
    #[test]
    fn a_us_account_is_the_unchanged_us_target() {
        use crate::updater::{polp, Product};
        assert_eq!(Target::for_account(0).unwrap(), Target::Us);
        assert!(Target::Us.log_lines().is_empty() && Target::Us.experimental().is_none());
        assert!(Target::for_account(7).is_err());
        // The Viewer check: W2U/1000 to pt008.pol.com:54000; the game check: W20/0001 to
        // pc001W2U.pol.com:53001, each the stock 88-byte frame.
        assert_eq!((Product::Viewer.server(), Product::Ffxi.server()), ("pt008.pol.com:54000", "pc001W2U.pol.com:53001"));
        assert_eq!((crate::dns::VIEWER_PATCH_HOST, crate::dns::GAME_PATCH_HOST, RetailRegion::Us.game_patch_host()), ("pt008.pol.com", "pc001W2U.pol.com", "pc001W2U.pol.com"));
        let viewer = Product::Viewer.request("20110829_E");
        assert_eq!(viewer, polp::version(&polp::Tags { platform: *b"W2U\0", title: *b"1000" }, b"20110829_E"));
        assert_eq!((viewer.len(), &viewer[8..24], &viewer[24..34]), (88, &b"POLP\x07\0\0\0W2U\x001000"[..], &b"20110829_E"[..]));
        let game = Product::Ffxi.request("30260904_1");
        assert_eq!((game.len(), &game[8..24]), (88, &b"POLP\x07\0\0\0W20\x000001"[..]));
        // Its game settings key and its Viewer are what they were.
        assert_eq!(reg::ffxi_settings_key("PlayOnlineUS"), r"SOFTWARE\PlayOnlineUS\SquareEnix\FinalFantasyXI");
        let root = scratch("us");
        let folder = viewer_folder_for_test(&root);
        assert_eq!((edition(&folder), foreign_viewer(&folder)), (Some(RetailRegion::Us), None));
        assert!(refuse_update(&folder).is_ok() && crate::discovery::is_install(crate::discovery::Product::PlayOnline, &folder));
        // A Viewer folder with no readable env.dat (a damaged or partial US install) stays US.
        std::fs::remove_file(folder.join(r"data\utf\env.dat")).unwrap();
        assert_eq!(foreign_viewer(&folder), None);
        assert!(crate::discovery::is_install(crate::discovery::Product::PlayOnline, &folder));
        std::fs::remove_dir_all(root).unwrap();
    }
    fn viewer_folder_for_test(root: &Path) -> PathBuf { viewer(root, RetailRegion::Us) }

    #[test]
    fn an_eu_install_gives_its_own_values() {
        let root = scratch("eu");
        let folder = viewer(&root, RetailRegion::Eu);
        let source = r"HKLM\SOFTWARE\WOW6432Node\PlayOnlineEU\InstallFolder\1000";
        let (found, named) = choose_viewer(RetailRegion::Eu, &[entry(source, Some(&folder))]).unwrap();
        assert_eq!((found.as_path(), named.as_str()), (folder.as_path(), source));
        let values = Experimental::read(RetailRegion::Eu, &found, named).unwrap();
        // EU\env.dat in the Viewer folder, not the US file beside it in data\utf.
        assert_eq!(values.env_file, folder.join(r"EU\env.dat"));
        assert_eq!((values.patch_host.as_str(), values.ucs_area.as_str(), values.viewer_tag.clone()), ("pt008.pol.com", "02", Ok(*b"W2U\0")));
        assert_eq!(values.viewer_check().as_deref(), Some("pt008.pol.com:54000 W2U/1000"));
        let lines = Target::Experimental(values).log_lines();
        assert_eq!(lines.len(), 1, "one compact line");
        for expected in ["Region EU (experimental)", &folder.display().to_string(), source, r"core viewer\com\polcoreEU.dll",
            r"EU\env.dat: PATCH_SERVER_DOMAIN pt008.pol.com, POL_UCS_AREA_KBN 02", "Viewer check pt008.pol.com:54000 W2U/1000",
            "FFXI check pc001W2U.pol.com:53001 W20/0001", "not parked"] {
            assert!(lines[0].contains(expected), "{expected} in {}", lines[0]);
        }
        // Without EU\env.dat the stock Viewer falls back to data\utf\env.dat; so does Crystal.
        std::fs::remove_file(folder.join(r"EU\env.dat")).unwrap();
        let fallback = Experimental::read(RetailRegion::Eu, &folder, source.into()).unwrap();
        assert_eq!((fallback.env_file, fallback.ucs_area.as_str()), (folder.join(r"data\utf\env.dat"), "01"));
        assert_eq!((edition(&folder), foreign_viewer(&folder)), (Some(RetailRegion::Eu), Some(RetailRegion::Eu)), "its core alone marks it EU");
        assert!(!crate::discovery::is_install(crate::discovery::Product::PlayOnline, &folder) && refuse_update(&folder).is_err());
        // The US and EU installers share a default folder: with both installed there, the
        // folder is the US Viewer too, and stays one for US accounts and Crystal's updater.
        put(&folder, r"EU\env.dat", &crate::patch::encode_env_dat(EU_ENV));
        put(&folder, r"viewer\com\polcore.dll", b"");
        put(&folder, r"data\utf\env.dat", &crate::patch::encode_env_dat(US_ENV));
        assert_eq!((edition(&folder), foreign_viewer(&folder)), (Some(RetailRegion::Us), None));
        assert!(crate::discovery::is_install(crate::discovery::Product::PlayOnline, &folder) && refuse_update(&folder).is_ok());
        assert_eq!(choose_viewer(RetailRegion::Eu, &[entry(source, Some(&folder))]).unwrap().0, folder, "and the EU Viewer for an EU account");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The tester's case: a current Japanese Viewer was told to update because Crystal asked
    /// about it as the US/EU Viewer (W2U). Its own contents table says W20.
    #[test]
    fn a_japanese_install_gives_its_own_values_and_its_own_viewer_tag() {
        let root = scratch("jp");
        let folder = viewer(&root, RetailRegion::Jp);
        let source = r"HKLM\SOFTWARE\WOW6432Node\PlayOnline\InstallFolder\1000";
        let values = Experimental::read(RetailRegion::Jp, &folder, source.into()).unwrap();
        assert_eq!(values.env_file, folder.join(r"data\utf\env.dat"));
        assert_eq!((values.patch_host.as_str(), values.ucs_area.as_str(), values.viewer_tag.clone()), ("pt004.pol.com", "00", Ok(*b"W20\0")));
        assert_eq!(values.viewer_check().as_deref(), Some("pt004.pol.com:54000 W20/1000"));
        let line = values.summary();
        for expected in ["Region JP (experimental)", r"SOFTWARE\WOW6432Node\PlayOnline\InstallFolder\1000", r"core viewer\com\polcore.dll",
            "PATCH_SERVER_DOMAIN pt004.pol.com, POL_UCS_AREA_KBN 00", "Viewer check pt004.pol.com:54000 W20/1000", "FFXI check pc001W20.pol.com:53001 W20/0001"] {
            assert!(line.contains(expected), "{expected} in {line}");
        }
        assert_eq!((edition(&folder), foreign_viewer(&folder)), (Some(RetailRegion::Jp), Some(RetailRegion::Jp)));
        // Never the US Viewer: not found as one, and Crystal's updater refuses it.
        assert!(!crate::discovery::is_install(crate::discovery::Product::PlayOnline, &folder));
        assert!(refuse_update(&folder).unwrap_err().contains("JP PlayOnline Viewer"));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A contents table that is missing or unreadable leaves the tag unknown: the sign-in is
    /// prepared all the same, and its log says why no Viewer check goes out.
    #[test]
    fn an_unknown_viewer_tag_never_refuses_the_sign_in() {
        let root = scratch("tag");
        let folder = viewer(&root, RetailRegion::Jp);
        for damage in [None, Some(&[7u8; 64][..]), Some(&crate::patch::encode_contents(&[(1, 0)])[..]), Some(&crate::patch::encode_contents(&[(1000, 5)])[..])] {
            match damage {
                None => std::fs::remove_file(folder.join(CONTENTS)).unwrap(),
                Some(bytes) => std::fs::write(folder.join(CONTENTS), bytes).unwrap(),
            }
            let values = Experimental::read(RetailRegion::Jp, &folder, "test".into()).unwrap();
            assert!(values.viewer_tag.is_err() && values.viewer_check().is_none(), "{:?}", values.viewer_tag);
            assert!(values.summary().contains("Viewer check not sent (data\\doc\\sqpolcts.bin: "), "{}", values.summary());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The tester's case: an installed EU PlayOnline was reported "not installed" with no way
    /// to tell which assumption failed. Every registered place is tried, and a failure names
    /// each one with what was found there.
    #[test]
    fn the_viewer_is_found_wherever_it_is_registered_and_a_miss_names_every_place() {
        let root = scratch("find");
        let eu = viewer(&root, RetailRegion::Eu);
        let us = viewer(&root, RetailRegion::Us);
        let missing = root.join("gone");
        let hklm = r"HKLM\SOFTWARE\WOW6432Node\PlayOnlineEU\InstallFolder\1000";
        // A trailing separator, as some installers write it.
        let slashed = PathBuf::from(format!("{}\\", eu.display()));
        assert_eq!(choose_viewer(RetailRegion::Eu, &[entry(hklm, Some(&slashed))]).unwrap().0, eu);
        // A stale InstallFolder entry: the registered core's folder still finds it.
        let (found, source) = choose_viewer(RetailRegion::Eu, &[entry(hklm, Some(&missing)), entry("HKLM 64", None), entry("core class", Some(&eu))]).unwrap();
        assert_eq!((found, source.as_str()), (eu.clone(), "core class"));
        // Another region's Viewer is never taken: the EU core is not in it.
        let error = choose_viewer(RetailRegion::Eu, &[entry(hklm, Some(&us)), entry("HKLM 64", None), entry("HKCU", Some(&missing)), entry("relative", Some(Path::new("PlayOnlineViewer")))]).unwrap_err();
        for expected in ["PlayOnline (EU) was not found", r"viewer\com\polcoreEU.dll", &format!("{hklm} = {}: viewer\\com has no polcoreEU.dll (it holds app.dll, polcore.dll)", us.display()),
            "HKLM 64: not set", &format!("HKCU = {}: the folder does not exist", missing.display()), "relative = PlayOnlineViewer: not an absolute path"] {
            assert!(error.contains(expected), "{expected} in {error}");
        }
        std::fs::create_dir_all(missing.join("viewer")).unwrap();
        assert!(choose_viewer(RetailRegion::Jp, &[entry("x", Some(&missing))]).unwrap_err().contains(r"it has no viewer\com folder"));
        assert_eq!(choose_viewer(RetailRegion::Jp, &[entry("x", Some(&us))]).unwrap().0, us, "polcore.dll is Japan's core name too");
        assert!(choose_viewer(RetailRegion::Jp, &[]).unwrap_err().contains("Checked: ."));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The registry is read in the places each installer writes (read-only; whatever this PC
    /// holds, the list has the same shape).
    #[test]
    fn every_registered_place_is_named_for_each_region() {
        for (region, root, class) in [(RetailRegion::Eu, "PlayOnlineEU", "{E5966FB3-C97B-42EB-84BF-37F95EE54A9F}"), (RetailRegion::Jp, "PlayOnline", "{07974581-0DF6-4EF0-BD05-604B3ADA9BE9}")] {
            let sources: Vec<String> = registered_viewers(region).into_iter().map(|entry| entry.source).collect();
            assert_eq!(sources, [
                format!(r"HKLM\SOFTWARE\WOW6432Node\{root}\InstallFolder\1000"),
                format!(r"HKLM\SOFTWARE\{root}\InstallFolder\1000"),
                format!(r"HKCU\SOFTWARE\{root}\InstallFolder\1000"),
                format!(r"HKLM\SOFTWARE\Classes\WOW6432Node\CLSID\{class}\InprocServer32"),
                format!(r"HKCU\SOFTWARE\Classes\WOW6432Node\CLSID\{class}\InprocServer32"),
                format!(r"HKLM\SOFTWARE\WOW6432Node\{root}\InstallFolder\0001 (PlayOnlineViewer beside the game)"),
            ]);
        }
    }

    #[test]
    fn env_values_come_from_the_installs_own_file_or_the_sign_in_is_refused() {
        let root = scratch("env");
        let folder = viewer(&root, RetailRegion::Eu);
        // A damaged file, one with a broken trailer, or one without the settings is refused.
        std::fs::write(folder.join(r"EU\env.dat"), [7u8; 64]).unwrap();
        assert!(Experimental::read(RetailRegion::Eu, &folder, "test".into()).unwrap_err().contains("could not be decoded"));
        std::fs::write(folder.join(r"EU\env.dat"), crate::patch::encode_env_dat(b"Version,1\r\nPOL_UCS_AREA_KBN,03\r\n")).unwrap();
        assert!(Experimental::read(RetailRegion::Eu, &folder, "test".into()).unwrap_err().contains("PATCH_SERVER_DOMAIN"));
        std::fs::remove_file(folder.join(r"EU\env.dat")).unwrap();
        std::fs::remove_file(folder.join(r"data\utf\env.dat")).unwrap();
        let error = Experimental::read(RetailRegion::Eu, &folder, "test".into()).unwrap_err();
        assert!(error.contains("cannot read env.dat") && error.contains(r"EU\env.dat") && error.contains(r"data\utf\env.dat"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn env_settings_are_validated() {
        assert_eq!(parse_env(US_ENV).unwrap(), ("pt008.pol.com".into(), "01".into()));
        assert_eq!(parse_env(b"PATCH_SERVER_DOMAIN,PT004.pol.com\r\nPOL_UCS_AREA_KBN,00\r\n").unwrap(), ("pt004.pol.com".into(), "00".into()));
        for bad in [&b"PATCH_SERVER_DOMAIN,evil.example.com\r\nPOL_UCS_AREA_KBN,01\r\n"[..], b"PATCH_SERVER_DOMAIN,.pol.com\r\nPOL_UCS_AREA_KBN,01\r\n",
            b"PATCH_SERVER_DOMAIN,a b.pol.com\r\nPOL_UCS_AREA_KBN,01\r\n", b"PATCH_SERVER_DOMAIN_X,pt008.pol.com\r\nPOL_UCS_AREA_KBN,01\r\n"] {
            assert_eq!(parse_env(bad).unwrap_err(), "PATCH_SERVER_DOMAIN", "{}", String::from_utf8_lossy(bad));
        }
        for bad in [&b"PATCH_SERVER_DOMAIN,pt008.pol.com\r\n"[..], b"PATCH_SERVER_DOMAIN,pt008.pol.com\r\nPOL_UCS_AREA_KBN,1\r\n",
            b"PATCH_SERVER_DOMAIN,pt008.pol.com\r\nPOL_UCS_AREA_KBN,0a\r\n"] {
            assert_eq!(parse_env(bad).unwrap_err(), "POL_UCS_AREA_KBN");
        }
    }

    #[test]
    fn the_contents_table_gives_each_titles_platform_flag() {
        use crate::patch::{contents_platform_flag, encode_contents};
        let us = encode_contents(US_CONTENTS);
        assert_eq!((contents_platform_flag(&us, 1000), contents_platform_flag(&us, 1)), (Ok(1), Ok(0)));
        let jp = encode_contents(JP_CONTENTS);
        assert_eq!((contents_platform_flag(&jp, 1000), contents_platform_flag(&jp, 1)), (Ok(0), Ok(0)));
        assert!(contents_platform_flag(&us, 77).unwrap_err().contains("no record for title 77"));
        // A damaged trailer (the last block holds the length and checksum) or a cut file is refused.
        let mut broken = us.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0x80;
        assert!(contents_platform_flag(&broken, 1000).is_err() && contents_platform_flag(&us[..us.len() - 8], 1000).is_err());
        // The env.dat key does not open it, nor its key an env.dat.
        assert!(contents_platform_flag(&crate::patch::encode_env_dat(US_ENV), 1000).is_err() && crate::patch::decode_env_dat(&us).is_err());
    }

    #[test]
    fn an_eu_or_jp_update_refusal_points_to_the_official_viewer_never_to_crystals_update() {
        let refusal = "Update required: installed 30260904_1, official 30261006_0.";
        assert!(crate::patch::is_update_refusal(refusal) && crate::patch::parse_update_required(refusal).is_some());
        let official = official_game_update("EU", refusal).unwrap();
        assert!(official.contains("30260904_1") && official.contains("30261006_0") && official.contains("official PlayOnline Viewer"), "{official}");
        assert!(!crate::patch::is_update_refusal(&official) && crate::patch::parse_update_required(&official).is_none(), "the window offers no Update");
        assert_eq!(official_game_update("JP", "Game version could not be compared."), None);
        let viewer = crate::viewer::official_update_required("JP", "20260929_0", "20261101_0", "pt004.pol.com:54000 W20/1000");
        assert!(viewer.contains("pt004.pol.com:54000 W20/1000") && crate::viewer::outdated_versions(&viewer).is_none(), "{viewer}");
        assert!(!crate::viewer::local_update_required(&viewer), "{viewer}");
    }

    /// The US Viewer's readiness (installed, updated against the remembered US version, its
    /// core) gates every sign-in it always gated, and never an EU or JP retail account's.
    #[test]
    fn the_us_viewer_gate_and_the_us_update_wording_stay_with_us_accounts() {
        assert!(us_viewer_gates(true, 0) && us_viewer_gates(false, 0) && us_viewer_gates(false, 1) && us_viewer_gates(false, 2) && us_viewer_gates(true, 9));
        assert!(!us_viewer_gates(true, 1) && !us_viewer_gates(true, 2));
        let refusal = "Update required: installed 30260904_1, official 30261006_0.";
        assert_eq!(game_gate(0, Err(refusal.into())), Err(refusal.into()), "US: Crystal's Update, as before");
        assert_eq!(game_gate(0, Ok("fine".into())), Ok("fine".into()));
        for region in [1, 2] {
            let reworded = game_gate(region, Err(refusal.into())).unwrap_err();
            assert!(reworded.contains("official PlayOnline Viewer") && !crate::patch::is_update_refusal(&reworded), "{reworded}");
            assert_eq!(game_gate(region, Err("The network is down.".into())), Err("The network is down.".into()));
            assert_eq!(game_gate(region, Ok("fine".into())), Ok("fine".into()));
        }
        assert_eq!(game_folder(0), None, "US: found as it always was, nothing read here");
    }

    #[test]
    fn env_codec_round_trips_and_refuses_a_bad_trailer() {
        let cipher = crate::patch::encode_env_dat(US_ENV);
        assert_eq!(crate::patch::decode_env_dat(&cipher).unwrap(), US_ENV);
        let mut broken = cipher.clone();
        broken[3] ^= 1;
        assert!(crate::patch::decode_env_dat(&broken).is_err());
        assert!(crate::patch::decode_env_dat(&cipher[..cipher.len() - 1]).is_err());
    }

    /// The installed US env.dat and contents table decode to the values Crystal's US path
    /// already uses (run on a machine with the US Viewer: `cargo test retail_region -- --ignored`).
    #[test]
    #[ignore = "reads the installed US PlayOnline Viewer"]
    fn the_installed_us_viewer_gives_the_verified_us_values() {
        let viewer = reg::install_folders("1000", &[(0, "PlayOnlineUS")]).into_iter().map(|(_, p)| p)
            .find(|p| p.join(r"data\utf\env.dat").is_file()).expect("an installed US Viewer");
        let values = Experimental::read(RetailRegion::Us, &viewer, "test".into()).unwrap();
        assert_eq!((values.patch_host.as_str(), values.ucs_area.as_str()), (crate::dns::VIEWER_PATCH_HOST, "01"));
        assert_eq!(values.viewer_tag, Ok(crate::updater::Product::Viewer.tags().platform), "the stock capture's W2U");
        assert_eq!(edition(&viewer), Some(RetailRegion::Us));
    }

    /// The values in a region's official installer files, extracted to a folder laid out as
    /// the Viewer folder (`CRYSTAL_REGION_VIEWER_EU` / `_JP` name it). Reads files only.
    #[test]
    #[ignore = "reads an EU or JP PlayOnline Viewer folder named by CRYSTAL_REGION_VIEWER_EU / _JP"]
    fn an_official_eu_or_jp_viewer_folder_gives_the_tabled_values() {
        for (region, variable, expected) in [(RetailRegion::Eu, "CRYSTAL_REGION_VIEWER_EU", ("pt008.pol.com", "02", *b"W2U\0", r"EU\env.dat")),
            (RetailRegion::Jp, "CRYSTAL_REGION_VIEWER_JP", ("pt004.pol.com", "00", *b"W20\0", r"data\utf\env.dat"))] {
            let Some(folder) = std::env::var_os(variable).map(PathBuf::from) else { continue };
            let values = Experimental::read(region, &folder, variable.into()).unwrap();
            assert_eq!((values.patch_host.as_str(), values.ucs_area.as_str(), values.viewer_tag.clone().unwrap(), values.env_file.clone()),
                (expected.0, expected.1, expected.2, folder.join(expected.3)), "{}", values.summary());
            assert_eq!(edition(&folder), Some(region));
        }
    }

    /// Each edition is told by its core file (JP's polcore.dll by its env.dat, else its
    /// registry root), and the core checked is the one that is there.
    #[test]
    fn each_folders_edition_and_core_come_from_its_own_core_file() {
        crate::i18n::set(crate::i18n::Lang::En);
        let root = scratch("editions");
        for region in [RetailRegion::Us, RetailRegion::Eu, RetailRegion::Jp] {
            let folder = viewer(&root, region);
            assert_eq!(folder_editions(&folder, None), vec![region], "{region:?}");
            assert_eq!(core_file(&folder).unwrap(), folder.join("viewer").join("com").join(region.polcore()));
            assert_eq!(folder_region(&folder, 0), region.index(), "a folder named by the US root reads as its own edition");
            assert_eq!(folder_region(&folder, region.index()), region.index());
        }
        // A JP folder whose env.dat cannot be read is JP when the JP root named it, else US.
        let jp = viewer(&root, RetailRegion::Jp);
        std::fs::remove_file(jp.join(r"data\utf\env.dat")).unwrap();
        assert_eq!((folder_editions(&jp, Some(RetailRegion::Jp)), folder_editions(&jp, None)), (vec![RetailRegion::Jp], vec![RetailRegion::Us]));
        // A shared US+EU folder holds both editions; each root keeps its own.
        let shared = viewer(&root, RetailRegion::Eu);
        put(&shared, r"viewer\com\polcore.dll", b"");
        assert_eq!(folder_editions(&shared, None), vec![RetailRegion::Us, RetailRegion::Eu]);
        assert_eq!((folder_region(&shared, 0), folder_region(&shared, 1)), (0, 1));
        // Neither core: said plainly, never a raw "file not found".
        let empty = root.join("empty");
        std::fs::create_dir_all(empty.join(r"viewer\com")).unwrap();
        let missing = core_file(&empty).unwrap_err();
        assert_eq!(missing, format!("PlayOnline's core file is missing from {}. Reinstall PlayOnline or check your antivirus.", empty.join(r"viewer\com").display()));
        assert!(folder_editions(&empty, Some(RetailRegion::Eu)).is_empty() && folder_region(&empty, 1) == 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A US account on an EU-only PC and an EU account on a US-only PC are told what to set;
    /// a matching region, or nothing found, says nothing. The new account's region is the
    /// installed edition only when there is exactly one.
    #[test]
    fn an_account_region_that_is_not_installed_says_what_to_set() {
        use RetailRegion::{Eu, Jp, Us};
        crate::i18n::set(crate::i18n::Lang::En);
        let root = scratch("mismatch");
        let (us, eu) = (viewer(&root, Us), viewer(&root, Eu));
        let eu_only = editions_in([(eu.clone(), Some(Us))]);
        assert_eq!(eu_only, vec![Eu], "an EU folder is EU whatever root names it");
        assert_eq!(mismatch(0, &eu_only).as_deref(), Some("This PlayOnline is the EU version. Set this account to EU."));
        let us_only = editions_in([(us.clone(), None)]);
        assert_eq!(mismatch(1, &us_only).as_deref(), Some("This PlayOnline is the US version. Set this account to US."));
        assert_eq!((mismatch(1, &eu_only), mismatch(0, &us_only), mismatch(0, &[])), (None, None, None));
        let both = editions_in([(us, Some(Us)), (eu, Some(Eu))]);
        assert_eq!(mismatch(2, &both).as_deref(), Some("PlayOnline (JP) is not installed. This PC has US, EU."));
        assert_eq!((only_edition(&eu_only), only_edition(&both), only_edition(&[])), (Some(Eu), None, None));
        assert_eq!(only_edition(&[Jp]).map(RetailRegion::index), Some(2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
