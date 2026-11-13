//! dgVoodoo 2 for the game: Settings → Graphics turns it on, and Crystal downloads, verifies
//! and sets it up on its own, like the official launchers do with their extras.
//!
//! FFXiMain imports `d3d8.dll` by name, so a `D3D8.dll` in the folder of the exe that hosts the
//! game wins over the system one (DLL search order). The game runs inside Crystal's own loader,
//! `crystal\pol.exe` (or a versioned `crystal\<hash8>\pol.exe`, or the Settings override), so the
//! wrapper goes next to whichever loader a launch actually runs.
//!
//! * The release zip is pinned by URL and SHA-256. Nothing is extracted from a zip that doesn't
//!   match, and every extracted file must match its own pinned hash too, so no unverified DLL is
//!   ever written where an elevated game could load it.
//! * Only the files FFXI needs are kept, in `<data>\crystal\dgvoodoo\<version>\` with the
//!   settings app (`dgVoodooCpl.exe`) and the user's `dgVoodoo.conf`, which the settings app edits
//!   in its own folder.
//! * Each launch copies the files next to the loader when they are missing or differ. A file
//!   Crystal didn't put there is renamed aside first, never deleted. With the option off, only
//!   files that match what Crystal placed are removed.

use crate::i18n::{t, tf};
use crate::{App, Message};
use crate::effect::Effect;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// The pinned dgVoodoo 2 release: the newest on GitHub with a recorded digest. 2.87.6 fixes
/// D3D8 exclusive-mode and fullscreen-restore bugs. Its tag has no `v` prefix, unlike 2.87.5's.
pub(crate) const VERSION: &str = "2.87.6";
const URL: &str = "https://github.com/dege-diosg/dgVoodoo2/releases/download/2.87.6/dgVoodoo2_87_6.zip";

/// One file taken from the zip: where it is in the zip, what it is called on disk, its SHA-256.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pinned {
    pub entry: &'static str,
    pub name: &'static str,
    pub sha256: &'static str,
}

/// A release: the zip's own hash and size cap, the DLLs, the settings app and the stock conf.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Release {
    pub version: &'static str,
    pub zip_sha256: &'static str,
    pub max_zip: u64,
    /// What goes next to the loader. FFXiMain imports only `d3d8.dll`.
    pub dlls: &'static [Pinned],
    /// (name, SHA-256) of DLLs earlier Crystal releases placed next to the loader. Matching
    /// copies are Crystal's own, so they are replaced or removed, never set aside.
    pub retired: &'static [(&'static str, &'static str)],
    pub cpl: Pinned,
    pub conf: Pinned,
}

pub(crate) const CONF: &str = "dgVoodoo.conf";
/// The release's own conf, kept in the cache for "Reset to Crystal defaults".
const STOCK: &str = "dgVoodoo.conf.stock";
/// Next to the loader: what Crystal placed there and each file's hash, so only those are touched.
const MANIFEST: &str = "crystal-dgvoodoo.txt";
/// In the cache: the settings a carried-over conf keeps, shown once in Settings → Graphics.
const NOTE: &str = "crystal-conf-note.txt";

pub(crate) static PINNED: Release = Release {
    version: VERSION,
    zip_sha256: "9afa493396feec7108f42a2d97661eef96bbaac5addf0dfb5cfbc3001cd76a0a",
    max_zip: 32 * 1024 * 1024,
    dlls: &[
        Pinned { entry: "MS/x86/D3D8.dll", name: "D3D8.dll", sha256: "25a0ceeb85fcc4675c6f964442eecc14fedd504effd39c931a627249ad4685e2" },
    ],
    // 2.87.5, which Crystal placed with DDraw.dll and D3DImm.dll (used only by the POL viewer).
    retired: &[
        ("D3D8.dll", "d03e2562178db1fcf3493fc0a0b23a465e35c55d73adb3ec095be866cd662704"),
        ("D3DImm.dll", "93c534f2d17419ea78f15551f7e0aac78b3c503733a840914fa063708a5afe8e"),
        ("DDraw.dll", "612a24408a090a3c6f3886557fa18034ee742e94ad0a40ebdf854d2816176c2e"),
    ],
    cpl: Pinned { entry: "dgVoodooCpl.exe", name: "dgVoodooCpl.exe", sha256: "db43fd68878de5a088897cc2dbabc4b2254cfc32bb29a3d2c2dc60e5512eb9a1" },
    conf: Pinned { entry: "dgVoodoo.conf", name: CONF, sha256: "45cc0eb8e6ed4f067aca1b2e4ac1268dfbd164bbbd06d264c53e8a60642c4dbf" },
};

/// Crystal's changes to the release's stock conf, by section. Everything else stays as shipped,
/// including AppControlledScreenMode and DisableAltEnterToToggleScreenMode (true), Resolution
/// (unforced) and the DirectX ForceVerticalSync (false).
pub(crate) const DEFAULTS: &[(&str, &str, &str)] = &[
    ("General", "OutputAPI", "d3d11_fl11_0"),
    ("General", "CaptureMouse", "false"),
    ("GeneralExt", "FullscreenAttributes", "fake"),
    ("DirectX", "VRAM", "1024"),
    ("DirectX", "dgVoodooWatermark", "false"),
    ("DirectX", "FastVideoMemoryAccess", "true"),
];

/// What Crystal 1.0.8 seeded: the stock conf with the watermark off.
const WATERMARK_OFF: &[(&str, &str, &str)] = &[("DirectX", "dgVoodooWatermark", "false")];

/// SHA-256 of confs Crystal seeded before (each one a default nobody tuned): 2.87.5's stock with
/// the watermark off, and the untouched stock. Kept so a later stock change still recognizes them.
const PAST_SEEDS: &[&str] = &[
    "74931eed2e09702a75343b9aa5fe141c350d14381e3c2db1bcdc9954c57b48e4",
    "45cc0eb8e6ed4f067aca1b2e4ac1268dfbd164bbbd06d264c53e8a60642c4dbf",
];

/// Settings-side installs and launch-time installs never write the cache at the same time, and
/// launches running together never place into (or clear) a loader folder at the same time.
static WRITING: Mutex<()> = Mutex::new(());

use crate::pol_less::tools::embedded::sha256_hex;

fn file_hash(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| sha256_hex(&b))
}

fn has_hash(path: &Path, hash: &str) -> bool {
    file_hash(path).is_some_and(|h| h == hash)
}

/// Crystal's per-user data folder (the one `crystal\pol.exe` lives under).
pub(crate) fn data_root() -> PathBuf {
    crate::pol_less::tools::embedded::data_dir()
}

/// `<root>\crystal\dgvoodoo\<version>`.
pub(crate) fn cache_dir(root: &Path, release: &Release) -> PathBuf {
    root.join("crystal").join("dgvoodoo").join(release.version)
}

/// Every DLL (and the settings app) is in the cache and matches its pinned hash.
pub(crate) fn installed(root: &Path, release: &Release) -> bool {
    let dir = cache_dir(root, release);
    release.dlls.iter().all(|p| has_hash(&dir.join(p.name), p.sha256))
        && has_hash(&dir.join(release.cpl.name), release.cpl.sha256)
        && has_hash(&dir.join(STOCK), release.conf.sha256)
        && dir.join(CONF).is_file()
}

/// Creates the parent folder, then replaces `path` whole, so a reader never sees half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("no parent folder"))?)?;
    Ok(crate::fsutil::write_atomic(path, bytes)?)
}

/// The wanted entries' bytes, by their on-disk name. Entry names compare without case and with
/// either slash; anything missing is an error.
pub(crate) fn pick_files(zip: &[u8], wanted: &[Pinned]) -> Result<Vec<(Pinned, Vec<u8>)>, String> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(io::Cursor::new(zip))
        .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
    let norm = |name: &str| name.replace('\\', "/").to_ascii_lowercase();
    let mut found = Vec::with_capacity(wanted.len());
    for pin in wanted {
        let want = norm(pin.entry);
        let index = (0..archive.len())
            .find(|&i| archive.name_for_index(i).is_some_and(|n| norm(n) == want))
            .ok_or_else(|| tf("The dgVoodoo download has no {file}.", &[("file", &pin.entry)]))?;
        let mut entry = archive
            .by_index(index)
            .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
        if entry.size() > 16 * 1024 * 1024 {
            return Err(tf("The dgVoodoo download has no {file}.", &[("file", &pin.entry)]));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| tf("The dgVoodoo download is not a valid zip: {error}", &[("error", &e)]))?;
        found.push((*pin, bytes));
    }
    Ok(found)
}

// ---- the conf ----

/// A conf line split into its section, key and value, for the lines that set a value.
fn entries(conf: &str) -> Vec<(&str, &str, &str)> {
    let mut section = "";
    let mut out = Vec::new();
    for line in conf.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.split_once(']')).map(|(n, _)| n.trim()) {
            section = name;
        } else if let Some((key, value)) = line.split_once('=').filter(|_| !line.starts_with(';')) {
            out.push((section, key.trim(), value.trim()));
        }
    }
    out
}

/// The values a conf sets, compared without case, spacing, comments or order.
fn meaning(conf: &str) -> std::collections::BTreeMap<(String, String), String> {
    entries(conf)
        .into_iter()
        .map(|(s, k, v)| ((s.to_ascii_lowercase(), k.to_ascii_lowercase()), v.to_ascii_lowercase()))
        .collect()
}

/// `conf` with each (section, key) of `edits` set to its value. Only the value changes: the
/// key's alignment, the comments and the line endings stay as they are.
pub(crate) fn set_values(conf: &str, edits: &[(&str, &str, &str)]) -> String {
    let mut out = String::with_capacity(conf.len() + 64);
    let mut section = String::new();
    for line in conf.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some((name, _)) = trimmed.strip_prefix('[').and_then(|l| l.split_once(']')) {
            section = name.trim().to_owned();
        }
        let edit = line.split_once('=').filter(|_| !trimmed.starts_with(';')).and_then(|(head, rest)| {
            let key = head.trim();
            edits
                .iter()
                .find(|(s, k, _)| s.eq_ignore_ascii_case(&section) && k.eq_ignore_ascii_case(key))
                .map(|(_, _, value)| (head, rest, *value))
        });
        match edit {
            Some((head, rest, value)) => {
                let ending = if rest.ends_with("\r\n") { "\r\n" } else if rest.ends_with('\n') { "\n" } else { "" };
                let body = &rest[..rest.len() - ending.len()];
                let pad = &body[..body.len() - body.trim_start().len()];
                out.push_str(&format!("{head}={pad}{value}{ending}"));
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Crystal's starting conf: the release's stock one with `DEFAULTS` applied.
pub(crate) fn baseline(stock: &str) -> String {
    set_values(stock, DEFAULTS)
}

/// Whether `conf` is a default nobody tuned: a conf Crystal seeded before, the stock one, or
/// today's baseline, byte for byte or by its values.
fn is_default(conf: &[u8], stock: &str) -> bool {
    if PAST_SEEDS.contains(&sha256_hex(conf).as_str()) {
        return true;
    }
    let have = meaning(&String::from_utf8_lossy(conf));
    [stock.to_owned(), set_values(stock, WATERMARK_OFF), baseline(stock)].iter().any(|d| meaning(d) == have)
}

/// The user's settings in `conf` that differ from what Crystal used to seed, as `Key = value`.
fn changes(conf: &str, stock: &str) -> Vec<String> {
    let reference = meaning(&set_values(stock, WATERMARK_OFF));
    entries(conf)
        .into_iter()
        .filter(|(s, k, v)| {
            reference.get(&(s.to_ascii_lowercase(), k.to_ascii_lowercase())).is_none_or(|r| !r.eq_ignore_ascii_case(v))
        })
        .map(|(_, k, v)| if v.is_empty() { format!("{k} =") } else { format!("{k} = {v}") })
        .collect()
}

/// The first unused `<name>.crystal-backup[-n]` in `dir`.
fn backup_path(dir: &Path, name: &str) -> PathBuf {
    (0..)
        .map(|n| dir.join(if n == 0 { format!("{name}.crystal-backup") } else { format!("{name}.crystal-backup-{n}") }))
        .find(|p| !p.exists())
        .expect("an unused name")
}

/// Brings the cache's conf up to Crystal's defaults when it is a default nobody tuned (a copy
/// goes to `.crystal-backup` first). A tuned conf is kept; when it was just carried over to a
/// new version (`seeded`), the settings it keeps are noted once for Settings → Graphics.
fn adopt_conf(dir: &Path, stock: &str, seeded: bool) -> io::Result<()> {
    let path = dir.join(CONF);
    let current = fs::read(&path)?;
    let base = baseline(stock);
    if is_default(&current, stock) {
        if meaning(&String::from_utf8_lossy(&current)) != meaning(&base) {
            fs::copy(&path, backup_path(dir, CONF))?;
            write_atomic(&path, base.as_bytes())?;
        }
    } else if seeded {
        let kept = changes(&String::from_utf8_lossy(&current), stock);
        if !kept.is_empty() {
            write_atomic(&dir.join(NOTE), kept.join("\n").as_bytes())?;
        }
    }
    Ok(())
}

/// The note `adopt_conf` left, once: reading it removes it.
fn take_note(root: &Path, release: &Release) -> Vec<String> {
    let path = cache_dir(root, release).join(NOTE);
    let text = fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() {
        let _ = fs::remove_file(&path);
    }
    text.lines().map(str::to_owned).filter(|l| !l.is_empty()).take(12).collect()
}

/// The pinned stock conf from the cache.
fn cached_stock(dir: &Path, release: &Release) -> Option<String> {
    fs::read(dir.join(STOCK)).ok().filter(|b| sha256_hex(b) == release.conf.sha256).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Puts `bytes` in as the cache's conf, the current one copied to `.crystal-backup` first
/// unless it already says the same.
fn replace_conf(root: &Path, release: &Release, bytes: &[u8]) -> Result<(), String> {
    let dir = cache_dir(root, release);
    let path = dir.join(CONF);
    let fail = |e: io::Error| tf("Could not save dgVoodoo in {folder}: {error}", &[("folder", &dir.display()), ("error", &e)]);
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(current) = fs::read(&path) {
        if meaning(&String::from_utf8_lossy(&current)) == meaning(&String::from_utf8_lossy(bytes)) {
            return Ok(());
        }
        fs::copy(&path, backup_path(&dir, CONF)).map_err(fail)?;
    }
    write_atomic(&path, bytes).map_err(fail)
}

/// Settings → Graphics "Reset to Crystal defaults": the baseline, after a backup.
pub(crate) fn reset_conf(root: &Path, release: &Release) -> Result<(), String> {
    let stock = cached_stock(&cache_dir(root, release), release).ok_or_else(|| t("dgVoodoo is not installed yet.").to_owned())?;
    replace_conf(root, release, baseline(&stock).as_bytes())
}

/// Settings → Graphics "Import settings": another install's conf, after a backup.
pub(crate) fn import_conf(root: &Path, release: &Release, from: &Path) -> Result<(), String> {
    let bytes = others::read_conf(from).ok_or_else(|| tf("{file} is not a dgVoodoo settings file.", &[("file", &from.display())]))?;
    replace_conf(root, release, &bytes)
}

/// Puts a `dgVoodoo.conf` in `dir` unless one is there already (never overwrites the user's).
/// It starts from one the user already has in Crystal's folder (an older version's), then
/// PlayOnline's, then Crystal's baseline. True when it put one there.
pub(crate) fn seed_conf(dir: &Path, pol_conf: Option<&Path>, release_conf: &[u8]) -> io::Result<bool> {
    let target = dir.join(CONF);
    if target.is_file() {
        return Ok(false);
    }
    let readable = |p: &Path| fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= 1024 * 1024);
    // An older version's conf in Crystal's own dgvoodoo folder, newest first.
    let mut older: Vec<(std::time::SystemTime, PathBuf)> = dir
        .parent()
        .and_then(|p| fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(CONF))
        .filter(|p| p.parent() != Some(dir) && readable(p))
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    older.sort();
    let bytes = if let Some((_, path)) = older.pop() {
        fs::read(&path)?
    } else if let Some(path) = pol_conf.filter(|p| readable(p)) {
        fs::read(path)?
    } else {
        baseline(&String::from_utf8_lossy(release_conf)).into_bytes()
    };
    write_atomic(&target, &bytes)?;
    Ok(true)
}

/// PlayOnline's own `dgVoodoo.conf`, from the Viewer's install folder in the registry.
pub(crate) fn playonline_conf() -> Option<PathBuf> {
    crate::patch::install_folder(crate::updater::Product::Viewer).map(|dir| dir.join(CONF)).filter(|p| p.is_file())
}

/// Checks the zip against the pinned hash, then writes only the needed files to the cache and
/// seeds the conf. Nothing is written for a zip that doesn't match.
pub(crate) fn install_from_zip(root: &Path, zip: &[u8], release: &Release, pol_conf: Option<&Path>) -> Result<(), String> {
    if sha256_hex(zip) != release.zip_sha256 {
        return Err(t("The dgVoodoo download doesn't match its pinned SHA-256, so nothing was installed.").into());
    }
    let mut wanted: Vec<Pinned> = release.dlls.to_vec();
    wanted.push(release.cpl);
    wanted.push(release.conf);
    let files = pick_files(zip, &wanted)?;
    for (pin, bytes) in &files {
        if sha256_hex(bytes) != pin.sha256 {
            return Err(tf("{file} in the dgVoodoo download doesn't match its pinned SHA-256, so nothing was installed.", &[("file", &pin.name)]));
        }
    }
    let dir = cache_dir(root, release);
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let save_error = |e: io::Error| tf("Could not save dgVoodoo in {folder}: {error}", &[("folder", &dir.display()), ("error", &e)]);
    let mut release_conf: &[u8] = &[];
    for (pin, bytes) in &files {
        // The stock conf is kept under its own name; the user's conf is never overwritten here.
        let name = if pin.entry == release.conf.entry {
            release_conf = bytes;
            STOCK
        } else {
            pin.name
        };
        if !has_hash(&dir.join(name), pin.sha256) {
            write_atomic(&dir.join(name), bytes).map_err(save_error)?;
        }
    }
    let seeded = seed_conf(&dir, pol_conf, release_conf).map_err(save_error)?;
    adopt_conf(&dir, &String::from_utf8_lossy(release_conf), seeded).map_err(save_error)?;
    prune_old_caches(root, release);
    Ok(())
}

/// Best effort: removes older versions' DLLs and settings app from Crystal's dgVoodoo folder.
/// Their confs stay (the newest one seeds the next version).
fn prune_old_caches(root: &Path, release: &Release) {
    let current = cache_dir(root, release);
    let Some(parent) = current.parent() else { return };
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let dir = entry.path();
        if dir == current || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for name in ["D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodooCpl.exe", STOCK] {
            let _ = fs::remove_file(dir.join(name));
        }
    }
}

// ---- next to the loader ----

fn read_manifest(dir: &Path) -> Vec<(String, String)> {
    fs::read_to_string(dir.join(MANIFEST))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(h, n)| (n.trim().to_owned(), h.trim().to_ascii_lowercase())))
        .filter(|(n, h)| !n.contains(['/', '\\']) && h.len() == 64)
        .collect()
}

fn write_manifest(dir: &Path, entries: &[(String, String)]) -> io::Result<()> {
    let body: String = entries.iter().map(|(n, h)| format!("{h} {n}\n")).collect();
    let path = dir.join(MANIFEST);
    if fs::read(&path).is_ok_and(|current| current == body.as_bytes()) {
        return Ok(());
    }
    write_atomic(&path, body.as_bytes())
}

/// Why `place` did not set dgVoodoo up.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlaceError {
    /// The cache lacks a pinned DLL (or it differs), or the conf.
    NotInstalled,
    Io(String),
}

impl From<PlaceError> for String {
    fn from(error: PlaceError) -> Self {
        match error {
            PlaceError::NotInstalled => t("dgVoodoo is not installed yet.").into(),
            PlaceError::Io(error) => error,
        }
    }
}

/// Makes the loader's folder hold the cached DLLs and conf. Copies a file only when it is missing
/// or differs; a differing file Crystal didn't place is renamed to `<name>.crystal-backup[-n]`.
/// Each cache file is read once: the bytes whose hash was checked are the bytes written.
pub(crate) fn place(root: &Path, release: &Release, loader_dir: &Path) -> Result<(), PlaceError> {
    let cache = cache_dir(root, release);
    let mut sources: Vec<(String, Vec<u8>, String)> = Vec::new();
    for pin in release.dlls {
        let bytes = fs::read(cache.join(pin.name)).map_err(|_| PlaceError::NotInstalled)?;
        if sha256_hex(&bytes) != pin.sha256 {
            return Err(PlaceError::NotInstalled);
        }
        sources.push((pin.name.to_owned(), bytes, pin.sha256.to_owned()));
    }
    let conf = fs::read(cache.join(CONF)).map_err(|_| PlaceError::NotInstalled)?;
    let conf_hash = sha256_hex(&conf);
    sources.push((CONF.to_owned(), conf, conf_hash));
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let ours = read_manifest(loader_dir);
    let mut manifest = Vec::new();
    let fail = |e: io::Error| PlaceError::Io(tf("Could not set up dgVoodoo in {folder}: {error}", &[("folder", &loader_dir.display()), ("error", &e)]));
    for (name, bytes, hash) in sources {
        let target = loader_dir.join(&name);
        match file_hash(&target) {
            Some(current) if current == hash => {}
            current => {
                let mine = current.as_ref().is_some_and(|h| crystals(release, &ours, &name, h));
                if current.is_some() && !mine {
                    fs::rename(&target, backup_path(loader_dir, &name)).map_err(fail)?;
                }
                write_atomic(&target, &bytes).map_err(fail)?;
            }
        }
        manifest.push((name, hash));
    }
    // DLLs an earlier release placed that this one doesn't (DDraw.dll, D3DImm.dll): removed when
    // they are still Crystal's copies. One a running game holds stays until a later launch.
    for (name, _) in release.retired.iter().filter(|(n, _)| !release.dlls.iter().any(|p| p.name.eq_ignore_ascii_case(n))) {
        let target = loader_dir.join(name);
        if file_hash(&target).is_some_and(|h| crystals(release, &ours, name, &h)) {
            let _ = fs::remove_file(&target);
        }
    }
    write_manifest(loader_dir, &manifest).map_err(fail)
}

/// Whether `name` with hash `hash` next to a loader is a copy Crystal placed: a pinned or
/// retired DLL, or what the folder's manifest recorded.
fn crystals(release: &Release, manifest: &[(String, String)], name: &str, hash: &str) -> bool {
    release.dlls.iter().any(|p| p.name.eq_ignore_ascii_case(name) && p.sha256 == hash)
        || release.retired.iter().any(|(n, h)| n.eq_ignore_ascii_case(name) && *h == hash)
        || manifest.iter().any(|(n, h)| n.eq_ignore_ascii_case(name) && h == hash)
}
/// Removes, from a loader's folder, only files that match the pinned DLLs or what Crystal's
/// manifest says it placed (the conf). Anything else stays. The DLLs go first: while one cannot
/// be removed (a running game has it loaded) the conf and the manifest stay, so that game keeps
/// its settings and a later attempt can still recognize the conf and older release's DLLs.
pub(crate) fn remove_from(release: &Release, loader_dir: &Path) -> Result<Vec<String>, String> {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let ours = read_manifest(loader_dir);
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    let failure = |path: &Path, error: io::Error| {
        tf("Could not remove {file}: {error}", &[("file", &path.display()), ("error", &error)])
    };
    let mut names: Vec<String> = release.dlls.iter().map(|p| p.name.to_owned()).collect();
    for (name, _) in release.retired {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push((*name).to_owned());
        }
    }
    names.push(CONF.to_owned());
    for name in names {
        if name == CONF && !errors.is_empty() {
            break;
        }
        let target = loader_dir.join(&name);
        let current = match fs::read(&target) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                errors.push(failure(&target, error));
                continue;
            }
        };
        if crystals(release, &ours, &name, &current) {
            match fs::remove_file(&target) {
                Ok(()) => removed.push(name),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => errors.push(failure(&target, error)),
            }
        }
    }
    if errors.is_empty() {
        let manifest = loader_dir.join(MANIFEST);
        match fs::remove_file(&manifest) {
            Ok(()) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => errors.push(failure(&manifest, error)),
        }
    }
    if errors.is_empty() { Ok(removed) } else { Err(errors.join("\n")) }
}

/// The folders a loader can run from: `crystal\`, each versioned `crystal\<hash8>\`, and the
/// Settings override's folder.
pub(crate) fn loader_dirs(root: &Path, override_path: &str) -> Vec<PathBuf> {
    let crystal = root.join("crystal");
    let mut dirs = vec![crystal.clone()];
    for entry in fs::read_dir(&crystal).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit()) && entry.path().join("pol.exe").is_file() {
            dirs.push(entry.path());
        }
    }
    let custom = override_path.trim();
    if !custom.is_empty() {
        if let Some(parent) = Path::new(custom).parent() {
            if !dirs.iter().any(|d| d == parent) {
                dirs.push(parent.to_owned());
            }
        }
    }
    dirs
}

/// Removes the placed files everywhere a loader can run from, ReShade's host folders included.
fn remove_everywhere(release: &Release, root: &Path, override_path: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    let hosts = fs::read_dir(crate::reshade::folder(root).join("hosts"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path());
    for dir in loader_dirs(root, override_path).into_iter().chain(hosts) {
        if let Err(error) = remove_from(release, &dir) {
            errors.push(error);
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

/// Deletes the cached DLLs, settings app and stock conf. The user's `dgVoodoo.conf` stays for
/// next time.
fn uninstall(root: &Path, override_path: &str) -> Result<(), String> {
    remove_everywhere(&PINNED, root, override_path)?;
    let dir = cache_dir(root, &PINNED);
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    for name in PINNED.dlls.iter().chain([&PINNED.cpl]).map(|p| p.name).chain([STOCK]) {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| tf("Could not remove {file}: {error}", &[("file", &path.display()), ("error", &e)]))?;
        }
    }
    Ok(())
}

/// Run in the launch (the session worker), with the exact loader it starts: puts dgVoodoo next to
/// it, or takes Crystal's copy away when the option is off. Never fails a launch; a problem comes
/// back as a line for the launch status.
pub(crate) fn sync_for_launch(loader: &Path, enabled: bool) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let dir = loader.parent()?;
    if !enabled {
        return remove_from(&PINNED, dir).err();
    }
    // Placed is not loaded: the loader's own notice (`loader_notice`) says whether it is active.
    match place(&data_root(), &PINNED, dir) {
        Ok(()) => Some(tf("dgVoodoo {version} is placed next to the game host.", &[("version", &VERSION)])),
        Err(PlaceError::NotInstalled) => Some(t("dgVoodoo is not installed yet, so this launch uses the game's own DirectX 8.").into()),
        Err(PlaceError::Io(error)) => Some(tf("{error} The game starts without dgVoodoo.", &[("error", &error)])),
    }
}

/// The loader's graphics notice ("graphics: dgVoodoo <ver> active" or "graphics: Windows D3D8
/// (dgVoodoo not loaded)", sent once the game's renderer is loaded) as a launch history line.
pub(crate) fn loader_notice(message: &str) -> Option<String> {
    let rest = message.strip_prefix("graphics: ")?;
    if let Some(version) = rest.strip_prefix("dgVoodoo ").and_then(|r| r.strip_suffix(" active")) {
        return Some(tf("dgVoodoo {version} is active.", &[("version", &version.trim())]));
    }
    rest.starts_with("Windows D3D8").then(|| t("dgVoodoo is not loaded; the game uses Windows' DirectX 8.").to_owned())
}

// ---- download ----

fn trusted_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com" || host.ends_with(".githubusercontent.com")
}

/// Downloads the pinned zip into memory, reporting (received, expected) as it goes.
async fn download(release: &Release, mut progress: impl FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
    let unreachable = || t("Couldn't reach GitHub to download dgVoodoo. Check your internet connection.").to_owned();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .user_agent("CrystalLauncher/0.1")
        .build()
        .map_err(|_| unreachable())?;
    // The stall guard below covers the body; this one covers the headers (and redirects).
    let mut response = tokio::time::timeout(Duration::from_secs(30), client.get(URL).send())
        .await
        .map_err(|_| unreachable())?
        .map_err(|_| unreachable())?;
    if !response.status().is_success() {
        return Err(tf("GitHub answered {status} for the dgVoodoo download.", &[("status", &response.status())]));
    }
    if !response.url().host_str().is_some_and(trusted_host) {
        return Err(t("The dgVoodoo download was redirected away from GitHub, so it was stopped.").into());
    }
    let total = response.content_length();
    let too_large = || t("The dgVoodoo download is larger than expected, so it was stopped.").to_owned();
    if total.is_some_and(|size| size > release.max_zip) {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut last = Instant::now();
    progress(0, total);
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| t("The dgVoodoo download stalled. Check your internet connection and try again.").to_owned())?
            .map_err(|_| unreachable())?;
        let Some(chunk) = chunk else { break };
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > release.max_zip {
            return Err(too_large());
        }
        if last.elapsed() >= Duration::from_millis(120) {
            last = Instant::now();
            progress(bytes.len() as u64, total);
        }
    }
    progress(bytes.len() as u64, total);
    Ok(bytes)
}

/// One install at a time: launches and the Settings page started together wait for one download.
static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Download, verify and install, off the UI thread.
async fn download_and_install(root: PathBuf, progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    let _installing = INSTALLING.lock().await;
    let check = root.clone();
    if tokio::task::spawn_blocking(move || installed(&check, &PINNED)).await.unwrap_or(false) {
        return Ok(());
    }
    let zip = download(&PINNED, progress).await?;
    tokio::task::spawn_blocking(move || install_from_zip(&root, &zip, &PINNED, playonline_conf().as_deref()))
        .await
        .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
}

/// For a launch with the option on: installs first when needed, reporting progress as launch
/// status lines. Runs on the launch's worker thread.
pub(crate) fn ensure_installed_blocking(mut status: impl FnMut(String)) -> Result<(), String> {
    let root = data_root();
    if installed(&root, &PINNED) {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| t("The dgVoodoo install stopped unexpectedly.").to_owned())?;
    status(t("Downloading dgVoodoo…").into());
    let mut shown = u64::MAX;
    let install = download_and_install(root, move |done, total| {
        if let Some(total) = total.filter(|t| *t > 0) {
            let percent = done.saturating_mul(100) / total;
            if percent != shown {
                shown = percent;
                status(tf("Downloading dgVoodoo… {percent}%", &[("percent", &percent)]));
            }
        }
    });
    // The connect, header and per-chunk stall timeouts in `download` bound a hung
    // connection; a slow one that still makes progress is allowed to finish.
    runtime.block_on(install)
}

// ---- Settings → Graphics ----

#[derive(Clone, Debug, PartialEq)]
enum Job {
    Idle,
    Running { id: u64, done: u64, total: Option<u64> },
    Failed(String),
}

pub(crate) struct State {
    installed: bool,
    job: Job,
    next_id: u64,
    preview: bool,
    /// The tuned settings a carried-over conf kept, shown this session only.
    kept: Vec<String>,
    /// Other dgVoodoo copies, and a conf to import.
    others: others::Report,
}

impl State {
    pub(crate) fn busy(&self) -> bool { matches!(self.job, Job::Running { .. }) }
    /// Reads the cache; with the option on and nothing installed yet, the install starts now.
    pub(crate) fn new(enabled: bool) -> Self {
        let mut state = Self { installed: false, job: Job::Idle, next_id: 1, preview: false, kept: Vec::new(), others: others::Report::default() };
        if crate::preview::flags().is_some() {
            state.preview = true;
            match std::env::var("XI_PREVIEW_DGVOODOO").unwrap_or_default().as_str() {
                "installed" => state.installed = true,
                "notes" => {
                    state.installed = true;
                    state.kept = vec!["VRAM = 512".into(), "Resolution = 2x".into()];
                    state.others.found = vec![
                        others::Found { place: others::Place::SysWow64, dir: r"C:\Windows\SysWOW64".into(), files: vec!["D3D8.dll".into()], version: "2.79.3".into() },
                        others::Found { place: others::Place::Other, dir: r"C:\Windower".into(), files: vec!["D3D8.dll".into(), "DDraw.dll".into()], version: "2.86.5".into() },
                    ];
                    state.others.import = Some(r"C:\Windower\dgVoodoo.conf".into());
                }
                "downloading" => state.job = Job::Running { id: 0, done: 4_170_000, total: Some(9_268_078) },
                "failed" => {
                    state.job = Job::Failed(t("Couldn't reach GitHub to download dgVoodoo. Check your internet connection.").into())
                }
                _ => {}
            }
            return state;
        }
        // Unit tests never read or download into the real data folder.
        if cfg!(test) {
            return state;
        }
        state.installed = installed(&data_root(), &PINNED);
        state.kept = take_note(&data_root(), &PINNED);
        if enabled && !state.installed {
            state.start();
        }
        state
    }
    fn start(&mut self) {
        if !matches!(self.job, Job::Running { .. }) {
            self.job = Job::Running { id: self.next_id, done: 0, total: None };
            self.next_id += 1;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Toggle(bool),
    Progress(u64, u64, Option<u64>),
    Finished(u64, Result<(), String>),
    OpenSettings,
    Remove,
    Removed(Result<(), String>),
    /// Looks for other dgVoodoo copies (when Graphics opens).
    Scan,
    Scanned(others::Report),
    /// Back to Crystal's defaults, the current conf backed up first.
    Reset,
    /// The conf `others` found, the current one backed up first.
    Import,
    ConfSaved(Result<String, String>),
}

/// The folders where another dgVoodoo copy matters: SysWOW64, the game folder, PlayOnline's,
/// and the configured Windower and Ashita folders.
fn other_folders(settings: &crate::Settings) -> Vec<(others::Place, PathBuf)> {
    use others::Place;
    let folder = |path: &str| {
        let path = PathBuf::from(path.trim());
        if path.is_file() { path.parent().map(Path::to_path_buf) } else { Some(path).filter(|p| !p.as_os_str().is_empty()) }
    };
    let game = folder(&settings.install_path).or_else(|| crate::patch::install_folder(crate::updater::Product::Ffxi));
    let viewer = crate::patch::install_folder(crate::updater::Product::Viewer);
    let mut list = vec![(Place::SysWow64, crate::win::system32().with_file_name("SysWOW64"))];
    list.extend(game.map(|p| (Place::Game, p)));
    list.extend([viewer, folder(&settings.windower_path), folder(&settings.ashita_path)].into_iter().flatten().map(|p| (Place::Other, p)));
    list
}

/// Scans for other copies off the UI thread; a conf to import is looked for only while
/// Crystal's own conf is still a default.
fn scan_effect(settings: &crate::Settings) -> Effect<Message> {
    let folders = other_folders(settings);
    Effect::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let dir = cache_dir(&data_root(), &PINNED);
                let stock = cached_stock(&dir, &PINNED)
                    .filter(|stock| fs::read(dir.join(CONF)).is_ok_and(|conf| is_default(&conf, stock)));
                others::scan(&folders, stock.as_deref())
            })
            .await
            .unwrap_or_default()
        },
        |report| dg(Msg::Scanned(report)),
    )
}

/// Runs a conf change off the UI thread, then reports `done` and scans again.
fn conf_effect(work: impl FnOnce() -> Result<(), String> + Send + 'static, done: &'static str) -> Effect<Message> {
    Effect::perform(
        async move {
            tokio::task::spawn_blocking(work)
                .await
                .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
                .map(|()| done.to_owned())
        },
        |result| dg(Msg::ConfSaved(result)),
    )
}

fn dg(msg: Msg) -> Message {
    Message::Dgvoodoo(msg)
}

pub(crate) fn update(app: &mut App, msg: Msg) -> Effect<Message> {
    if app.reshade.busy && matches!(msg, Msg::Toggle(_) | Msg::Remove) {
        app.status = t("Finish ReShade setup before changing dgVoodoo.").into();
        return Effect::none();
    }
    // A running game has D3D8.dll loaded: removing only what is not locked would leave later
    // "off" launches on dgVoodoo without its settings.
    if matches!(msg, Msg::Toggle(false) | Msg::Remove) && !app.preview && app.has_sessions() {
        app.status = t("Close games and sign out PlayOnline sessions before changing dgVoodoo.").into();
        return Effect::none();
    }
    if matches!(msg, Msg::Toggle(false) | Msg::Remove) && app.settings.reshade.enabled {
        if !app.preview && app.has_sessions() {
            app.status = t("Close games and sign out PlayOnline sessions before changing ReShade.").into();
            return Effect::none();
        }
        app.settings.reshade.enabled = false;
    }
    let preview = app.dgvoodoo.preview;
    let save = |app: &mut App| {
        if !preview {
            if let Err(error) = crate::save_settings(&app.config, &app.settings) {
                app.status = error;
            }
        }
    };
    match msg {
        Msg::Toggle(on) => {
            app.settings.dgvoodoo = on;
            save(app);
            if preview {
                return Effect::none();
            }
            if on {
                // A removal error from turning it off no longer applies.
                app.dgvoodoo.job = Job::Idle;
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
                if !app.dgvoodoo.installed {
                    app.dgvoodoo.start();
                }
            } else {
                // Stops a download in progress (its job stops) and takes Crystal's
                // files away from the loader folders; the cache stays for turning it back on.
                app.dgvoodoo.job = Job::Idle;
                let loader = app.settings.loader_path.clone();
                return Effect::perform(
                    async move {
                        tokio::task::spawn_blocking(move || remove_everywhere(&PINNED, &data_root(), &loader))
                            .await
                            .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
                    },
                    |result| dg(Msg::Removed(result)),
                );
            }
        }
        Msg::Progress(id, done, total) => {
            if let Job::Running { id: running, .. } = app.dgvoodoo.job {
                if running == id {
                    app.dgvoodoo.job = Job::Running { id, done, total };
                }
            }
        }
        Msg::Finished(id, result) => {
            if matches!(app.dgvoodoo.job, Job::Running { id: running, .. } if running == id) {
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
                app.dgvoodoo.kept.extend(take_note(&data_root(), &PINNED));
                app.dgvoodoo.job = match result {
                    Ok(()) => Job::Idle,
                    Err(error) => Job::Failed(error),
                };
                // Whether a conf to import is offered depends on the new install's conf.
                if !preview {
                    return scan_effect(&app.settings);
                }
            }
        }
        Msg::OpenSettings => {
            if preview {
                return Effect::none();
            }
            let dir = cache_dir(&data_root(), &PINNED);
            let cpl = dir.join(PINNED.cpl.name);
            // Only the pinned settings app runs.
            let started = if has_hash(&cpl, PINNED.cpl.sha256) {
                std::process::Command::new(&cpl).current_dir(&dir).spawn().map(|_| ()).map_err(|e| e.to_string())
            } else {
                Err(t("dgVoodoo is not installed yet.").into())
            };
            if let Err(error) = started {
                app.status = tf("Could not open dgVoodoo's settings: {error}", &[("error", &error)]);
            }
        }
        Msg::Remove => {
            app.settings.dgvoodoo = false;
            save(app);
            app.dgvoodoo.job = Job::Idle;
            if preview {
                app.dgvoodoo.installed = false;
                return Effect::none();
            }
            let loader = app.settings.loader_path.clone();
            return Effect::perform(
                async move {
                    tokio::task::spawn_blocking(move || uninstall(&data_root(), &loader))
                        .await
                        .unwrap_or_else(|_| Err(t("The dgVoodoo install stopped unexpectedly.").into()))
                },
                |result| dg(Msg::Removed(result)),
            );
        }
        Msg::Removed(result) => {
            if !preview {
                app.dgvoodoo.installed = installed(&data_root(), &PINNED);
            }
            if !app.settings.dgvoodoo {
                if let Err(error) = result {
                    app.dgvoodoo.job = Job::Failed(error);
                }
            }
        }
        Msg::Scan if !preview => return scan_effect(&app.settings),
        Msg::Scan => {}
        Msg::Scanned(report) => app.dgvoodoo.others = report,
        Msg::Reset if !preview && app.dgvoodoo.installed => {
            return conf_effect(|| reset_conf(&data_root(), &PINNED), t("dgVoodoo settings are back to Crystal's defaults. The old file was kept as a backup."));
        }
        Msg::Import if !preview && app.dgvoodoo.installed => {
            if let Some(from) = app.dgvoodoo.others.import.clone() {
                return conf_effect(move || import_conf(&data_root(), &PINNED, &from), t("dgVoodoo settings imported. The old file was kept as a backup."));
            }
        }
        Msg::Reset | Msg::Import => {}
        Msg::ConfSaved(result) => {
            app.status = result.unwrap_or_else(|error| error);
            app.dgvoodoo.kept.clear();
            return scan_effect(&app.settings);
        }
    }
    Effect::none()
}

/// The running download's id, while one should run.
pub(crate) fn work(state: &State) -> Option<u64> {
    match state.job {
        Job::Running { id, .. } if !state.preview => Some(id),
        _ => None,
    }
}

/// Download `id`'s progress and outcome as messages, in order.
pub(crate) fn run(id: u64) -> impl futures_util::Stream<Item = Message> + Send + 'static {
    crate::runtime::channel(16, async move |mut out: futures_channel::mpsc::Sender<Message>| {
        use futures_util::SinkExt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let work = download_and_install(data_root(), move |done, total| {
            let _ = tx.send((done, total));
        });
        tokio::pin!(work);
        let result = loop {
            tokio::select! {
                result = &mut work => break result,
                Some((done, total)) = rx.recv() => { let _ = out.send(dg(Msg::Progress(id, done, total))).await; }
            }
        };
        let _ = out.send(dg(Msg::Finished(id, result))).await;
    })
}

/// The card as plain data for the Slint front end.
pub(crate) mod front;
pub(crate) mod others;

fn mb(value: u64) -> String {
    format!("{:.1} MB", value as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let base = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("crystal-dgvoodoo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The stock conf's shape: the keys Crystal changes, in their real sections, with the real
    /// alignment, comments and a key (ForceVerticalSync) that two sections share.
    const CONF_TEXT: &str = "Version                              = 0x287\r\n\
        [General]\r\n\
        ;       OutputAPI: \"d3d11_fl11_0\", \"bestavailable\"\r\n\
        OutputAPI                            = bestavailable\r\n\
        CaptureMouse                         = true\r\n\
        [GeneralExt]\r\n\
        FullscreenAttributes                 = \r\n\
        [Glide]\r\n\
        ForceVerticalSync                   = true\r\n\
        [DirectX]\r\n\
        VRAM                                = 256\r\n\
        Resolution                          = unforced\r\n\
        AppControlledScreenMode             = true\r\n\
        DisableAltEnterToToggleScreenMode   = true\r\n\
        ForceVerticalSync                   = false\r\n\
        dgVoodooWatermark                   = true\r\n\
        FastVideoMemoryAccess               = false\r\n";

    /// What an earlier release placed next to the loader.
    const OLD_D3D8: &[u8] = b"old d3d8";
    const OLD_D3DIMM: &[u8] = b"old d3dimm";
    const OLD_DDRAW: &[u8] = b"old ddraw";

    /// A tiny release zip built here: the DLLs, the settings app, the conf, plus noise.
    fn synthetic() -> (Vec<u8>, Release) {
        let files: [(&str, &[u8]); 7] = [
            ("3Dfx/x86/Glide.dll", b"glide"),
            ("MS/x86/D3D8.dll", b"d3d8 bytes"),
            ("MS/x86/D3DImm.dll", b"d3dimm bytes"),
            ("MS/x86/DDraw.dll", b"ddraw bytes"),
            ("MS/x64/D3D9.dll", b"wrong arch"),
            ("dgVoodooCpl.exe", b"cpl bytes"),
            ("dgVoodoo.conf", CONF_TEXT.as_bytes()),
        ];
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in files {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let pin = |entry: &'static str, name: &'static str, data: &[u8]| Pinned { entry, name, sha256: leak(sha256_hex(data)) };
        let dlls: &'static [Pinned] = Box::leak(Box::new([pin("MS/x86/D3D8.dll", "D3D8.dll", b"d3d8 bytes")]));
        let retired: &'static [(&'static str, &'static str)] = Box::leak(Box::new([
            ("D3D8.dll", leak(sha256_hex(OLD_D3D8))),
            ("D3DImm.dll", leak(sha256_hex(OLD_D3DIMM))),
            ("DDraw.dll", leak(sha256_hex(OLD_DDRAW))),
        ]));
        let release = Release {
            version: "9.9.9",
            zip_sha256: leak(sha256_hex(&bytes)),
            max_zip: 1024 * 1024,
            dlls,
            retired,
            cpl: pin("dgVoodooCpl.exe", "dgVoodooCpl.exe", b"cpl bytes"),
            conf: pin("dgVoodoo.conf", CONF, CONF_TEXT.as_bytes()),
        };
        (bytes, release)
    }

    #[test]
    fn a_zip_that_does_not_match_its_hash_installs_nothing() {
        let root = temp("mismatch");
        let (mut bytes, release) = synthetic();
        *bytes.last_mut().unwrap() ^= 0xff;
        assert!(install_from_zip(&root, &bytes, &release, None).is_err());
        assert!(!root.join("crystal").exists(), "nothing is extracted from an unverified zip");
        // The real pin refuses anything but the real release.
        assert!(install_from_zip(&root, b"not the release", &PINNED, None).is_err());
        assert!(!root.join("crystal").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_that_does_not_match_its_own_pin_installs_nothing() {
        let root = temp("filepin");
        let (bytes, mut release) = synthetic();
        let mut dlls = release.dlls.to_vec();
        dlls[0].sha256 = "00".repeat(32).leak();
        release.dlls = Box::leak(dlls.into_boxed_slice());
        assert!(install_from_zip(&root, &bytes, &release, None).is_err());
        assert!(!root.join("crystal").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_the_needed_files_are_picked_from_the_zip() {
        let (bytes, release) = synthetic();
        let mut wanted = release.dlls.to_vec();
        wanted.push(release.cpl);
        let picked = pick_files(&bytes, &wanted).unwrap();
        let names: Vec<&str> = picked.iter().map(|(p, _)| p.name).collect();
        assert_eq!(names, ["D3D8.dll", "dgVoodooCpl.exe"]);
        assert_eq!(picked[0].1, b"d3d8 bytes", "the x86 DLL, not another architecture's");
        // Entry names match without case or slash direction.
        let odd = Pinned { entry: r"ms\X86\ddraw.DLL", name: "DDraw.dll", sha256: "" };
        assert_eq!(pick_files(&bytes, &[odd]).unwrap()[0].1, b"ddraw bytes");
        let missing = Pinned { entry: "MS/x86/D3D9.dll", name: "D3D9.dll", sha256: "" };
        assert!(pick_files(&bytes, &[missing]).is_err());

        let root = temp("install");
        install_from_zip(&root, &bytes, &release, None).unwrap();
        let dir = cache_dir(&root, &release);
        let mut files: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        // Only D3D8.dll: FFXiMain imports nothing else, and DDraw/D3DImm serve only the POL viewer.
        assert_eq!(files, ["D3D8.dll", "dgVoodoo.conf", "dgVoodoo.conf.stock", "dgVoodooCpl.exe"]);
        assert!(installed(&root, &release));
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), baseline(CONF_TEXT));
        assert_eq!(fs::read_to_string(dir.join(STOCK)).unwrap(), CONF_TEXT);
        // The stock copy is what "installed" checks, so a damaged one is repaired.
        fs::write(dir.join(STOCK), "edited").unwrap();
        assert!(!installed(&root, &release));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_baseline_changes_only_crystals_keys_in_their_sections() {
        let conf = baseline(CONF_TEXT);
        let want = [
            ("general", "outputapi", "d3d11_fl11_0"),
            ("general", "capturemouse", "false"),
            ("generalext", "fullscreenattributes", "fake"),
            ("directx", "vram", "1024"),
            ("directx", "dgvoodoowatermark", "false"),
            ("directx", "fastvideomemoryaccess", "true"),
            // Kept as shipped.
            ("directx", "appcontrolledscreenmode", "true"),
            ("directx", "disablealtentertotogglescreenmode", "true"),
            ("directx", "resolution", "unforced"),
            ("directx", "forceverticalsync", "false"),
            ("glide", "forceverticalsync", "true"),
        ];
        let values = meaning(&conf);
        for (s, k, v) in want {
            assert_eq!(values.get(&(s.into(), k.into())).map(String::as_str), Some(v), "[{s}] {k}");
        }
        // Alignment, comments and line endings stay; an empty value gets its value.
        assert!(conf.contains("\r\nVRAM                                = 1024\r\n"));
        assert!(conf.contains("\r\nFullscreenAttributes                 = fake\r\n"));
        assert!(conf.contains(";       OutputAPI: \"d3d11_fl11_0\", \"bestavailable\"\r\n"));
        assert_eq!(conf.lines().count(), CONF_TEXT.lines().count());
        assert_eq!(set_values("a = 1\ndgVoodooWatermark   = true\n3DfxWatermark = true", WATERMARK_OFF), "a = 1\ndgVoodooWatermark   = true\n3DfxWatermark = true", "outside [DirectX] nothing changes");
        assert_eq!(set_values("[DirectX]\ndgVoodooWatermark   = true\n", WATERMARK_OFF), "[DirectX]\ndgVoodooWatermark   = false\n");
    }

    #[test]
    fn defaults_are_recognized_by_bytes_or_values_and_tuned_confs_are_not() {
        assert!(is_default(CONF_TEXT.as_bytes(), CONF_TEXT));
        assert!(is_default(set_values(CONF_TEXT, WATERMARK_OFF).as_bytes(), CONF_TEXT));
        assert!(is_default(baseline(CONF_TEXT).as_bytes(), CONF_TEXT));
        // dgVoodooCpl rewrites the file in its own layout: same values, still a default.
        let reformatted = set_values(CONF_TEXT, WATERMARK_OFF).replace("                   = ", "=").replace("\r\n", "\n");
        assert!(is_default(reformatted.as_bytes(), CONF_TEXT));
        let tuned = set_values(CONF_TEXT, &[("DirectX", "VRAM", "512"), ("DirectX", "dgVoodooWatermark", "false")]);
        assert!(!is_default(tuned.as_bytes(), CONF_TEXT));
        assert_eq!(changes(&tuned, CONF_TEXT), ["VRAM = 512"]);
        // The untouched real stock is a past seed too, known by its hash alone.
        assert!(PAST_SEEDS.contains(&PINNED.conf.sha256));
    }

    #[test]
    fn a_default_conf_carried_to_a_new_version_becomes_the_baseline_with_a_backup() {
        let root = temp("migrate-default");
        let old = root.join("crystal").join("dgvoodoo").join("2.87.5");
        fs::create_dir_all(&old).unwrap();
        let seeded = set_values(CONF_TEXT, WATERMARK_OFF);
        fs::write(old.join(CONF), &seeded).unwrap();
        fs::write(old.join("D3D8.dll"), OLD_D3D8).unwrap();
        fs::write(old.join("dgVoodooCpl.exe"), b"old cpl").unwrap();
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        let dir = cache_dir(&root, &release);
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), baseline(CONF_TEXT));
        assert_eq!(fs::read_to_string(dir.join("dgVoodoo.conf.crystal-backup")).unwrap(), seeded);
        assert!(take_note(&root, &release).is_empty(), "a default needs no note");
        // The old version keeps only its conf.
        let left: Vec<String> = fs::read_dir(&old).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, [CONF]);
        // A repair install leaves the baseline alone (no second backup).
        fs::remove_file(dir.join("D3D8.dll")).unwrap();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        assert!(!dir.join("dgVoodoo.conf.crystal-backup-1").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_tuned_conf_is_carried_forward_noted_once_and_reset_on_request() {
        let root = temp("migrate-tuned");
        let old = root.join("crystal").join("dgvoodoo").join("2.87.5");
        fs::create_dir_all(&old).unwrap();
        let tuned = set_values(CONF_TEXT, &[("DirectX", "VRAM", "512"), ("DirectX", "dgVoodooWatermark", "false"), ("General", "OutputAPI", "d3d12_fl12_0")]);
        fs::write(old.join(CONF), &tuned).unwrap();
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        let dir = cache_dir(&root, &release);
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), tuned, "the user's settings are kept");
        assert!(!dir.join("dgVoodoo.conf.crystal-backup").exists());
        assert_eq!(take_note(&root, &release), ["OutputAPI = d3d12_fl12_0", "VRAM = 512"]);
        assert!(take_note(&root, &release).is_empty(), "shown once");

        reset_conf(&root, &release).unwrap();
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), baseline(CONF_TEXT));
        assert_eq!(fs::read_to_string(dir.join("dgVoodoo.conf.crystal-backup")).unwrap(), tuned);
        // Resetting again changes nothing and backs nothing up.
        reset_conf(&root, &release).unwrap();
        assert!(!dir.join("dgVoodoo.conf.crystal-backup-1").exists());

        // Importing another install's conf backs up the current one first.
        let other = root.join("Windower").join(CONF);
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, &tuned).unwrap();
        import_conf(&root, &release, &other).unwrap();
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), tuned);
        assert_eq!(fs::read_to_string(dir.join("dgVoodoo.conf.crystal-backup-1")).unwrap(), baseline(CONF_TEXT));
        fs::write(&other, "not a conf").unwrap();
        assert!(import_conf(&root, &release, &other).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_next_launch_removes_the_ddraw_and_d3dimm_crystal_placed_and_keeps_the_users() {
        let (root, release) = installed_root("retired");
        // 1.0.8 placed 2.87.5's three DLLs and recorded them.
        let ours = root.join("crystal").join("0123abcd");
        fs::create_dir_all(&ours).unwrap();
        for (name, bytes) in [("D3D8.dll", OLD_D3D8), ("D3DImm.dll", OLD_D3DIMM), ("DDraw.dll", OLD_DDRAW)] {
            fs::write(ours.join(name), bytes).unwrap();
        }
        place(&root, &release, &ours).unwrap();
        let mut left: Vec<String> = fs::read_dir(&ours).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["D3D8.dll", MANIFEST, CONF], "the old D3D8.dll is replaced, not set aside");
        assert!(has_hash(&ours.join("D3D8.dll"), release.dlls[0].sha256));
        assert_eq!(read_manifest(&ours).into_iter().map(|(n, _)| n).collect::<Vec<_>>(), ["D3D8.dll", CONF]);
        // A DDraw.dll the user put there is not Crystal's: it stays.
        let custom = root.join("custom");
        fs::create_dir_all(&custom).unwrap();
        fs::write(custom.join("DDraw.dll"), b"the user's ddraw").unwrap();
        place(&root, &release, &custom).unwrap();
        assert_eq!(fs::read(custom.join("DDraw.dll")).unwrap(), b"the user's ddraw");
        // Turning dgVoodoo off also takes an old release's leftovers away.
        let off = root.join("off");
        fs::create_dir_all(&off).unwrap();
        fs::write(off.join("DDraw.dll"), OLD_DDRAW).unwrap();
        assert_eq!(remove_from(&release, &off).unwrap(), ["DDraw.dll"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_loaders_graphics_notice_becomes_a_history_line() {
        assert_eq!(loader_notice("graphics: dgVoodoo 2.87.6 active").as_deref(), Some("dgVoodoo 2.87.6 is active."));
        assert_eq!(loader_notice("graphics: Windows D3D8 (dgVoodoo not loaded)").as_deref(), Some("dgVoodoo is not loaded; the game uses Windows' DirectX 8."));
        assert_eq!(loader_notice("FFXIDB minimap settings protection is off"), None);
    }

    #[test]
    fn conf_precedence_crystal_then_playonline_then_default() {
        let root = temp("conf");
        let dir = root.join("crystal").join("dgvoodoo").join("2.0");
        fs::create_dir_all(&dir).unwrap();
        let pol = root.join("PlayOnlineViewer").join(CONF);
        fs::create_dir_all(pol.parent().unwrap()).unwrap();
        fs::write(&pol, "tuned in PlayOnline").unwrap();
        // Nothing else: the release default, watermark off.
        seed_conf(&dir, None, CONF_TEXT.as_bytes()).unwrap();
        assert!(fs::read_to_string(dir.join(CONF)).unwrap().contains("dgVoodooWatermark                   = false\r\n"));
        fs::remove_file(dir.join(CONF)).unwrap();
        // PlayOnline's beats the default.
        seed_conf(&dir, Some(&pol), CONF_TEXT.as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "tuned in PlayOnline");
        // An existing Crystal conf is never overwritten.
        fs::write(dir.join(CONF), "edited in Crystal").unwrap();
        seed_conf(&dir, Some(&pol), CONF_TEXT.as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "edited in Crystal");
        // A new version starts from the older version's Crystal conf, over PlayOnline's.
        let newer = root.join("crystal").join("dgvoodoo").join("3.0");
        fs::create_dir_all(&newer).unwrap();
        seed_conf(&newer, Some(&pol), CONF_TEXT.as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(newer.join(CONF)).unwrap(), "edited in Crystal");
        fs::remove_dir_all(&root).unwrap();
    }

    fn installed_root(name: &str) -> (PathBuf, Release) {
        let root = temp(name);
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        (root, release)
    }

    #[test]
    fn missing_or_changed_settings_app_requires_repair_and_keeps_the_conf() {
        let root = temp("repair-settings");
        let (bytes, release) = synthetic();
        install_from_zip(&root, &bytes, &release, None).unwrap();
        let cache = cache_dir(&root, &release);
        let settings = cache.join(release.cpl.name);
        fs::write(cache.join(CONF), "user settings").unwrap();
        assert!(installed(&root, &release));
        fs::remove_file(&settings).unwrap();
        assert!(!installed(&root, &release));
        install_from_zip(&root, &bytes, &release, None).unwrap();
        assert!(installed(&root, &release));
        fs::write(&settings, b"damaged settings app").unwrap();
        assert!(!installed(&root, &release));
        install_from_zip(&root, &bytes, &release, None).unwrap();
        assert!(installed(&root, &release));
        assert_eq!(fs::read_to_string(cache.join(CONF)).unwrap(), "user settings");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn files_go_next_to_the_standard_and_the_versioned_loader() {
        let (root, release) = installed_root("place");
        let standard = root.join("crystal");
        let versioned = root.join("crystal").join("0123abcd");
        fs::create_dir_all(&versioned).unwrap();
        fs::write(standard.join("pol.exe"), b"loader").unwrap();
        fs::write(versioned.join("pol.exe"), b"loader 2").unwrap();
        let names = ["D3D8.dll", CONF, MANIFEST];
        let modified = |dir: &Path| -> Vec<std::time::SystemTime> {
            names.iter().map(|n| fs::metadata(dir.join(n)).unwrap().modified().unwrap()).collect()
        };
        for dir in [&standard, &versioned] {
            place(&root, &release, dir).unwrap();
            for pin in release.dlls {
                assert!(has_hash(&dir.join(pin.name), pin.sha256));
            }
            assert!(dir.join(CONF).is_file());
            assert!(fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("crystal-backup")));
            // A second launch writes nothing (the manifest included).
            let before = modified(dir);
            std::thread::sleep(Duration::from_millis(20));
            place(&root, &release, dir).unwrap();
            assert_eq!(modified(dir), before);
        }
        assert_eq!(loader_dirs(&root, ""), vec![standard.clone(), versioned.clone()]);
        // An edited conf in the cache reaches the loader at the next launch; the DLLs stay.
        let before = modified(&standard);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(cache_dir(&root, &release).join(CONF), "edited").unwrap();
        place(&root, &release, &standard).unwrap();
        let after = modified(&standard);
        assert_eq!(after[0], before[0]);
        assert_ne!(after[1], before[1]);
        assert_eq!(fs::read_to_string(standard.join(CONF)).unwrap(), "edited");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_foreign_file_next_to_the_loader_is_set_aside_not_deleted() {
        let (root, release) = installed_root("foreign");
        let dir = root.join("custom");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("D3D8.dll"), b"someone else's d3d8").unwrap();
        place(&root, &release, &dir).unwrap();
        assert_eq!(fs::read(dir.join("D3D8.dll.crystal-backup")).unwrap(), b"someone else's d3d8");
        assert!(has_hash(&dir.join("D3D8.dll"), release.dlls[0].sha256));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removal_leaves_files_that_are_not_crystals_alone() {
        let (root, release) = installed_root("remove");
        let dir = root.join("crystal");
        place(&root, &release, &dir).unwrap();
        // The user adds their own DDraw.dll and edits the conf by hand next to the loader.
        fs::write(dir.join("DDraw.dll"), b"user ddraw").unwrap();
        fs::write(dir.join(CONF), "hand edited").unwrap();
        fs::write(dir.join("notes.txt"), "mine").unwrap();
        let removed = remove_from(&release, &dir).unwrap();
        assert_eq!(removed, ["D3D8.dll"]);
        assert_eq!(fs::read(dir.join("DDraw.dll")).unwrap(), b"user ddraw");
        assert_eq!(fs::read_to_string(dir.join(CONF)).unwrap(), "hand edited");
        assert!(dir.join("notes.txt").is_file());
        assert!(!dir.join(MANIFEST).exists());
        // Crystal's untouched conf is removed with the DLLs.
        let other = root.join("other");
        fs::create_dir_all(&other).unwrap();
        place(&root, &release, &other).unwrap();
        let mut removed = remove_from(&release, &other).unwrap();
        removed.sort();
        assert_eq!(removed, ["D3D8.dll", "dgVoodoo.conf"]);
        assert_eq!(fs::read_dir(&other).unwrap().count(), 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn locked_files_keep_the_ownership_needed_to_retry_removal() {
        use std::os::windows::fs::OpenOptionsExt;
        // Both a delete-only sharing violation and a file we cannot hash must
        // leave enough ownership information for a later release to retry.
        for sharing in [0, 3] {
            let (root, release) = installed_root(&format!("locked-removal-{sharing}"));
            let dir = root.join("crystal");
            place(&root, &release, &dir).unwrap();
            let manifest = fs::read(dir.join(MANIFEST)).unwrap();
            let locked_dll = fs::OpenOptions::new().read(true).share_mode(sharing)
                .open(dir.join("D3D8.dll")).unwrap();
            let locked_conf = fs::OpenOptions::new().read(true).share_mode(sharing)
                .open(dir.join(CONF)).unwrap();
            let error = remove_from(&release, &dir).unwrap_err();
            assert!(error.contains("D3D8.dll"), "{error}");
            // While a DLL stays (a running game has it loaded), its conf stays with it.
            assert!(dir.join(CONF).is_file());
            assert_eq!(fs::read(dir.join(MANIFEST)).unwrap(), manifest);
            drop(locked_dll);
            drop(locked_conf);

            // The current release no longer pins that old DLL. Its saved
            // manifest is the only proof that lets the retry remove it.
            let mut newer = release;
            let mut dlls = newer.dlls.to_vec();
            dlls[0].sha256 = Box::leak("00".repeat(32).into_boxed_str());
            newer.dlls = Box::leak(dlls.into_boxed_slice());
            let mut removed = remove_from(&newer, &dir).unwrap();
            removed.sort();
            assert_eq!(removed, ["D3D8.dll", "dgVoodoo.conf"]);
            assert!(!dir.join(MANIFEST).exists());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn settings_without_the_field_turn_dgvoodoo_on() {
        let old: crate::Settings = serde_json::from_str(r#"{"install_path":"C:\\x"}"#).unwrap();
        assert!(old.dgvoodoo, "on by default, also for older settings files");
        assert!(crate::Settings::default().dgvoodoo);
        let off: crate::Settings = serde_json::from_str(r#"{"install_path":"C:\\x","dgvoodoo":false}"#).unwrap();
        assert!(!off.dgvoodoo);
        let saved = serde_json::to_string(&off).unwrap();
        assert!(!serde_json::from_str::<crate::Settings>(&saved).unwrap().dgvoodoo);
        // A launch plan serialised by an older launcher (no field) still reads, as off.
        let tools: crate::pol_less::ToolSettings =
            serde_json::from_str(r#"{"ashita":"","windower":"","boot_ini":"","windower_profile":"","loader":""}"#).unwrap();
        assert!(!tools.dgvoodoo);
    }

    /// The real pinned release, offline: `DGVOODOO_ZIP=<path to dgVoodoo2_87_6.zip>`.
    #[test]
    #[ignore]
    fn the_real_release_installs_with_every_pin() {
        let Some(path) = std::env::var_os("DGVOODOO_ZIP") else { return };
        let root = temp("real");
        install_from_zip(&root, &fs::read(path).unwrap(), &PINNED, None).unwrap();
        assert!(installed(&root, &PINNED));
        let cache = cache_dir(&root, &PINNED);
        assert!(has_hash(&cache.join(PINNED.cpl.name), PINNED.cpl.sha256));
        // Every key Crystal sets exists in the real stock conf, in that section.
        let stock = fs::read_to_string(cache.join(STOCK)).unwrap();
        let keys = meaning(&stock);
        for (s, k, _) in DEFAULTS {
            assert!(keys.contains_key(&(s.to_ascii_lowercase(), k.to_ascii_lowercase())), "[{s}] {k}");
        }
        let conf = fs::read_to_string(cache.join(CONF)).unwrap();
        assert_eq!(conf, baseline(&stock));
        assert!(conf.lines().any(|l| l.starts_with("VRAM") && l.trim_end().ends_with("= 1024")));
        let loader = root.join("crystal");
        place(&root, &PINNED, &loader).unwrap();
        assert!(loader.join("D3D8.dll").is_file() && loader.join(CONF).is_file() && !loader.join("DDraw.dll").exists());
        // The real D3D8.dll is recognized as dgVoodoo by its version resource.
        assert_eq!(others::version(&loader.join("D3D8.dll")).as_deref(), Some(VERSION));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_pin_is_the_github_release() {
        assert!(URL.starts_with("https://github.com/dege-diosg/dgVoodoo2/releases/download/2.87.6/"));
        assert_eq!(PINNED.zip_sha256.len(), 64);
        assert!(trusted_host("github.com") && trusted_host("release-assets.githubusercontent.com"));
        assert!(!trusted_host("github.com.example.net"));
    }

    #[test]
    fn a_missing_cache_reports_not_installed() {
        let root = temp("not-installed");
        let (_, release) = synthetic();
        assert_eq!(place(&root, &release, &root.join("crystal")), Err(PlaceError::NotInstalled));
        fs::remove_dir_all(&root).unwrap();
    }

    /// Launches running together place into one loader folder without
    /// tripping over each other's temporary files.
    #[test]
    fn concurrent_places_into_one_folder_all_succeed() {
        let (root, release) = installed_root("concurrent");
        let dir = root.join("crystal");
        fs::write(cache_dir(&root, &release).join(CONF), "edited").unwrap();
        let results: Vec<Result<(), PlaceError>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..6).map(|_| scope.spawn(|| place(&root, &release, &dir))).collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert!(fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removal_also_clears_reshade_host_folders() {
        let (root, release) = installed_root("hosts");
        let host = crate::reshade::folder(&root).join("hosts").join("0123456789abcdef-0123456789abcdef");
        fs::create_dir_all(&host).unwrap();
        place(&root, &release, &host).unwrap();
        remove_everywhere(&release, &root, "").unwrap();
        assert!(!host.join("D3D8.dll").exists());
        assert!(!host.join(CONF).exists());
        fs::remove_dir_all(&root).unwrap();
    }
}