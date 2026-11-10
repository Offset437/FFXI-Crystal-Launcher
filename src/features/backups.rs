//! One-click backup and restore of the game's user data: each FFXI install's `USER` folder
//! (macros, keybinds, per-character UI files), Windower 4's settings, scripts, add-on data and
//! plugin settings, and Ashita v4's config and scripts.
//!
//! Archives are dated zips in `iced-launcher/backups/` beside Crystal's other files. Each holds
//! the files under `ffxi/<index>/USER/...`, `windower/...` and `ashita/...` plus a
//! `manifest.json` with every file's size and SHA-256, so a damaged archive is found before it
//! is trusted. Nothing here touches the network, and nothing here ever writes game executables
//! or DATs: only user data is read, and a restore only writes the files the archive names.
//!
//! Restore is non-destructive: files present now but absent from the archive are left alone.

use crate::backend::Config;
use crate::i18n::{t, tf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// How many archives to keep (oldest removed first).
    pub keep: u32,
    /// Make one before every Update or Repair.
    pub before_update: bool,
}

impl Default for Settings {
    fn default() -> Self { Self { keep: 10, before_update: true } }
}

/// Where the data lives. Missing folders are skipped, not errors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sources {
    /// FFXI install folders (the one holding `USER`). The position is the archive's index.
    pub ffxi: Vec<PathBuf>,
    pub windower: Option<PathBuf>,
    pub ashita: Option<PathBuf>,
}

/// Why a backup was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason { Manual, BeforeUpdate, BeforeRepair, BeforeRestore }

impl Reason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Reason::Manual => "manual",
            Reason::BeforeUpdate => "before-update",
            Reason::BeforeRepair => "before-repair",
            Reason::BeforeRestore => "before-restore",
        }
    }
    fn parse(text: &str) -> Self {
        [Reason::BeforeUpdate, Reason::BeforeRepair, Reason::BeforeRestore]
            .into_iter().find(|r| r.as_str() == text).unwrap_or(Reason::Manual)
    }
}

/// The maintenance step a backup guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Maintenance { Update, Repair }

/// Which sets of files an archive holds, or a restore should write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Parts { pub ffxi: bool, pub windower: bool, pub ashita: bool }

impl Parts {
    pub(crate) const ALL: Parts = Parts { ffxi: true, windower: true, ashita: true };
    fn has(self, name: &str) -> bool {
        match name.split('/').next() {
            Some("ffxi") => self.ffxi,
            Some("windower") => self.windower,
            Some("ashita") => self.ashita,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackupInfo {
    pub path: PathBuf,
    pub file_name: String,
    /// Unix seconds.
    pub created: u64,
    pub reason: Reason,
    /// The archive's size on disk.
    pub size: u64,
    pub files: usize,
    pub parts: Parts,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct VerifyReport {
    pub checked: usize,
    /// In the manifest, not in the archive.
    pub missing: Vec<String>,
    /// In the archive, not in the manifest.
    pub extra: Vec<String>,
    /// Content differs from the manifest's hash or size.
    pub mismatched: Vec<String>,
}

impl VerifyReport {
    pub(crate) fn ok(&self) -> bool { self.missing.is_empty() && self.extra.is_empty() && self.mismatched.is_empty() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestoreReport {
    pub restored: usize,
    /// Archive files with no matching current folder (or outside the chosen parts).
    pub skipped: usize,
    /// The backup of the files as they were just before this restore, when there were any.
    pub safety: Option<BackupInfo>,
}

const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024 * 1024;
/// Programs whose files a restore would pull out from under (compared case-insensitively).
const BUSY: [&str; 5] = ["pol.exe", "ffxi-boot.exe", "windower.exe", "ashita-cli.exe", "ashita.exe"];

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    created: u64,
    reason: String,
    roots: BTreeMap<String, String>,
    files: Vec<FileEntry>,
}

#[derive(Serialize, Deserialize)]
struct FileEntry { path: String, size: u64, sha256: String }

struct Item { name: String, path: PathBuf, size: u64 }

pub(crate) fn backups_dir(config: &Config) -> PathBuf {
    config.experiment.join("iced-launcher").join("backups")
}

fn unreadable() -> String { t("The backup could not be read.").into() }

// ---------------------------------------------------------------- collecting

fn is_link(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    meta.file_type().is_symlink() || meta.file_attributes() & REPARSE_POINT != 0
}

fn add_file(path: &Path, name: String, out: &mut Vec<Item>) {
    // A file that vanished or is a link/oversize is simply not part of the backup.
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.is_file() && !is_link(&meta) && meta.len() <= MAX_FILE {
            out.push(Item { name, path: path.to_owned(), size: meta.len() });
        }
    }
}

/// Every regular file under `dir`, named `rel/...`; `skip` drops names. Links are not followed.
fn walk(dir: &Path, rel: &str, skip: &dyn Fn(&str) -> bool, out: &mut Vec<Item>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(tf("Could not read {name}.", &[("name", &dir.display())])),
    };
    for entry in entries.flatten() {
        let Some(file) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let name = format!("{rel}/{file}");
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if is_link(&meta) { continue; }
        if meta.is_dir() {
            walk(&entry.path(), &name, skip, out)?;
        } else if !skip(&name) {
            add_file(&entry.path(), name, out);
        }
    }
    Ok(())
}

fn collect(sources: &Sources) -> Result<(Vec<Item>, BTreeMap<String, String>), String> {
    let (mut items, mut roots) = (Vec::new(), BTreeMap::new());
    let keep_all = |_: &str| false;
    for (i, install) in sources.ffxi.iter().enumerate() {
        if !install.is_dir() { continue; }
        roots.insert(format!("ffxi/{i}"), install.display().to_string());
        walk(&install.join("USER"), &format!("ffxi/{i}/USER"), &keep_all, &mut items)?;
    }
    if let Some(root) = sources.windower.as_ref().filter(|p| p.is_dir()) {
        roots.insert("windower".into(), root.display().to_string());
        add_file(&root.join("settings.xml"), "windower/settings.xml".into(), &mut items);
        walk(&root.join("scripts"), "windower/scripts", &keep_all, &mut items)?;
        walk(&root.join("plugins").join("settings"), "windower/plugins/settings", &keep_all, &mut items)?;
        if let Ok(addons) = std::fs::read_dir(root.join("addons")) {
            for addon in addons.flatten() {
                let Some(name) = addon.file_name().to_str().map(str::to_owned) else { continue };
                if std::fs::symlink_metadata(addon.path()).is_ok_and(|m| m.is_dir() && !is_link(&m)) {
                    walk(&addon.path().join("data"), &format!("windower/addons/{name}/data"), &keep_all, &mut items)?;
                }
            }
        }
    }
    if let Some(root) = sources.ashita.as_ref().filter(|p| p.is_dir()) {
        roots.insert("ashita".into(), root.display().to_string());
        // Crystal generates these boot files itself; restoring old ones would be wrong.
        let generated = |name: &str| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("ashita/config/boot/crystal-") && lower.ends_with(".ini")
        };
        walk(&root.join("config"), "ashita/config", &generated, &mut items)?;
        walk(&root.join("scripts"), "ashita/scripts", &keep_all, &mut items)?;
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((items, roots))
}

// ---------------------------------------------------------------- helpers

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Hashes at most `limit` bytes of `reader`; returns (bytes read, hex digest).
fn hash_reader(reader: &mut dyn Read, limit: u64) -> std::io::Result<(u64, String)> {
    let (mut hasher, mut buf, mut count) = (Sha256::new(), vec![0u8; 64 * 1024], 0u64);
    let mut reader = reader.take(limit);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
        count += n as u64;
    }
    Ok((count, hex(&hasher.finalize())))
}

/// An archive entry name that can only land inside its target: forward slashes, no empty, `.`
/// or `..` parts, no drive/UNC/absolute form, no colon (drive letters, alternate streams), and
/// none of the characters Windows forbids in a name.
fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
        })
}

fn open_archive(path: &Path) -> Result<zip::ZipArchive<File>, String> {
    zip::ZipArchive::new(File::open(path).map_err(|_| unreadable())?).map_err(|_| unreadable())
}

fn read_manifest(archive: &mut zip::ZipArchive<File>) -> Result<Manifest, String> {
    let entry = archive.by_name(MANIFEST).map_err(|_| unreadable())?;
    let mut bytes = Vec::new();
    entry.take(MAX_MANIFEST).read_to_end(&mut bytes).map_err(|_| unreadable())?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| unreadable())?;
    if manifest.format > FORMAT { return Err(t("This backup was made by a newer Crystal.").into()); }
    Ok(manifest)
}

fn parts_of(manifest: &Manifest) -> Parts {
    let has = |p: &str| manifest.files.iter().any(|f| f.path.split('/').next() == Some(p));
    Parts { ffxi: has("ffxi"), windower: has("windower"), ashita: has("ashita") }
}

fn info_of(path: &Path) -> Result<BackupInfo, String> {
    let manifest = read_manifest(&mut open_archive(path)?)?;
    Ok(BackupInfo {
        path: path.to_owned(),
        file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        created: manifest.created,
        reason: Reason::parse(&manifest.reason),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        files: manifest.files.len(),
        parts: parts_of(&manifest),
    })
}

// ---------------------------------------------------------------- create / list / prune

/// Writes a new archive of everything in `sources` into `dir`. `Err` when there is nothing to
/// back up, the total would pass 2 GiB, or a file cannot be read; no partial archive is left.
pub(crate) fn create(dir: &Path, sources: &Sources, reason: Reason) -> Result<BackupInfo, String> {
    let (items, roots) = collect(sources)?;
    if items.is_empty() { return Err(t("There is no game data to back up.").into()); }
    if items.iter().map(|i| i.size).sum::<u64>() > MAX_TOTAL {
        return Err(t("The game data is over 2 GiB; nothing was backed up.").into());
    }
    std::fs::create_dir_all(dir).map_err(|_| t("Could not create the backups folder."))?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let target = (1u32..).map(|n| {
        let tag = if n == 1 { String::new() } else { format!("-{n}") };
        dir.join(format!("{stamp}{tag}-{}.zip", reason.as_str()))
    }).find(|p| !p.exists()).expect("unbounded range");
    let temp = target.with_extension(format!("zip.{}.tmp", crate::backend::unique_id()?));
    let result = write_archive(&temp, &items, roots, reason, now)
        .and_then(|()| crate::fsutil::replace(&temp, &target).map_err(|_| t("Could not save the backup.").into()));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    info_of(&target)
}

fn write_archive(temp: &Path, items: &[Item], roots: BTreeMap<String, String>, reason: Reason, created: u64) -> Result<(), String> {
    let save = || t("Could not save the backup.");
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(File::create(temp).map_err(|_| save())?));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut files = Vec::with_capacity(items.len());
    for item in items {
        let mut bytes = Vec::with_capacity(item.size as usize);
        File::open(&item.path).and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes))
            .map_err(|_| tf("Could not read {name}.", &[("name", &item.path.display())]))?;
        if bytes.len() as u64 > MAX_FILE { continue; } // grew past the cap since it was listed
        zip.start_file(item.name.as_str(), options).map_err(|_| save())?;
        zip.write_all(&bytes).map_err(|_| save())?;
        files.push(FileEntry { path: item.name.clone(), size: bytes.len() as u64, sha256: hex(&Sha256::digest(&bytes)) });
    }
    let manifest = Manifest { format: FORMAT, created, reason: reason.as_str().into(), roots, files };
    zip.start_file(MANIFEST, options).map_err(|_| save())?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest).map_err(|_| save())?).map_err(|_| save())?;
    let file = zip.finish().map_err(|_| save())?.into_inner().map_err(|_| save())?;
    file.sync_all().map_err(|_| save().into())
}

/// The readable archives in `dir`, newest first. Unreadable files are left out.
pub(crate) fn list(dir: &Path) -> Vec<BackupInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<_> = entries.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")))
        .filter_map(|e| info_of(&e.path()).ok())
        .collect();
    found.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.file_name.cmp(&a.file_name)));
    found
}

/// Removes the oldest archives beyond `keep` (at least 1); returns how many were removed.
pub(crate) fn prune(dir: &Path, keep: u32) -> usize {
    list(dir).into_iter().skip(keep.max(1) as usize)
        .filter(|b| std::fs::remove_file(&b.path).is_ok()).count()
}

/// The automatic backup before an Update or Repair: none when the setting is off or there is
/// nothing to back up; otherwise the new archive, after pruning to `settings.keep`.
pub(crate) fn before_maintenance(config: &Config, sources: &Sources, kind: Maintenance, settings: &Settings)
    -> Result<Option<BackupInfo>, String> {
    if !settings.before_update { return Ok(None); }
    let dir = backups_dir(config);
    let reason = match kind { Maintenance::Update => Reason::BeforeUpdate, Maintenance::Repair => Reason::BeforeRepair };
    match create(&dir, sources, reason) {
        Ok(info) => { prune(&dir, settings.keep); Ok(Some(info)) }
        Err(_) if collect(sources).is_ok_and(|(items, _)| items.is_empty()) => Ok(None),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------- verify / restore

fn verify_archive(path: &Path) -> Result<(Manifest, VerifyReport), String> {
    let mut archive = open_archive(path)?;
    let manifest = read_manifest(&mut archive)?;
    let expected: HashMap<&str, &FileEntry> = manifest.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let (mut report, mut seen) = (VerifyReport::default(), HashSet::new());
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|_| unreadable())?;
        if entry.is_dir() || entry.name() == MANIFEST { continue; }
        let name = entry.name().to_owned();
        match expected.get(name.as_str()) {
            Some(file) if seen.insert(name.clone()) => {
                let (size, digest) = hash_reader(&mut entry, file.size + 1).map_err(|_| unreadable())?;
                report.checked += 1;
                if size != file.size || !digest.eq_ignore_ascii_case(&file.sha256) { report.mismatched.push(name); }
            }
            _ => report.extra.push(name),
        }
    }
    report.missing = manifest.files.iter().filter(|f| !seen.contains(&f.path)).map(|f| f.path.clone()).collect();
    Ok((manifest, report))
}

/// Recomputes every file's SHA-256 against the manifest.
pub(crate) fn verify(path: &Path) -> Result<VerifyReport, String> { verify_archive(path).map(|(_, r)| r) }

/// Program names from the process list (`None`: it could not be read).
fn running_programs() -> Option<Vec<String>> {
    crate::win::process::processes().map(|list| list.into_iter().map(|e| e.name).collect())
}

/// Restores the chosen `parts` of an archive over the current folders. Refuses while the game,
/// PlayOnline, Windower or Ashita runs; refuses a damaged archive or unsafe entry names; makes
/// a `before-restore` backup of the current files first. Files that exist now but are not in the
/// archive are left alone.
pub(crate) fn restore(path: &Path, sources: &Sources, parts: Parts, game_running: bool) -> Result<RestoreReport, String> {
    restore_with(path, sources, parts, game_running, &running_programs)
}

pub(crate) fn restore_with(path: &Path, sources: &Sources, parts: Parts, game_running: bool,
    running: &dyn Fn() -> Option<Vec<String>>) -> Result<RestoreReport, String> {
    if game_running { return Err(t("The game is running. Close it before restoring.").into()); }
    let programs = running().ok_or_else(|| t("Could not check for running programs."))?;
    if let Some(name) = programs.iter().find(|p| BUSY.iter().any(|b| p.eq_ignore_ascii_case(b))) {
        return Err(tf("{name} is running. Close it before restoring.", &[("name", name)]));
    }
    let (manifest, report) = verify_archive(path)?;
    if !report.ok() {
        let bad = report.missing.len() + report.extra.len() + report.mismatched.len();
        return Err(tf("The backup is damaged ({count} files differ). Nothing was restored.", &[("count", &bad)]));
    }
    if manifest.files.iter().any(|f| !valid_entry_name(&f.path)) {
        return Err(t("The backup holds unsafe file names. Nothing was restored.").into());
    }
    let (mut plan, mut skipped) = (Vec::new(), 0);
    for file in &manifest.files {
        match parts.has(&file.path).then(|| target_of(sources, &file.path)).flatten() {
            Some(target) => plan.push((file, target)),
            None => skipped += 1,
        }
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let safety = match create(dir, sources, Reason::BeforeRestore) {
        Ok(info) => Some(info),
        Err(_) if collect(sources).is_ok_and(|(items, _)| items.is_empty()) => None,
        Err(error) => return Err(error),
    };
    let mut archive = open_archive(path)?;
    for (file, target) in &plan {
        let mut bytes = Vec::with_capacity(file.size as usize);
        let entry = archive.by_name(&file.path).map_err(|_| unreadable())?;
        entry.take(file.size + 1).read_to_end(&mut bytes).map_err(|_| unreadable())?;
        if hex(&Sha256::digest(&bytes)) != file.sha256.to_ascii_lowercase() { return Err(unreadable()); }
        let fail = || tf("Could not write {name}.", &[("name", &target.display())]);
        std::fs::create_dir_all(target.parent().ok_or_else(fail)?).map_err(|_| fail())?;
        crate::fsutil::write_atomic(target, &bytes).map_err(|_| fail())?;
    }
    Ok(RestoreReport { restored: plan.len(), skipped, safety })
}

/// The current location an archive entry restores to; `None` when that folder is not set or
/// is missing now (an `ffxi/<i>` entry restores only to `sources.ffxi[i]`).
fn target_of(sources: &Sources, name: &str) -> Option<PathBuf> {
    let (root, rest) = match name.split_once('/')? {
        ("windower", rest) => (sources.windower.as_ref()?, rest),
        ("ashita", rest) => (sources.ashita.as_ref()?, rest),
        ("ffxi", rest) => {
            let (index, rest) = rest.split_once('/')?;
            (sources.ffxi.get(index.parse::<usize>().ok()?)?, rest)
        }
        _ => return None,
    };
    root.is_dir().then(|| rest.split('/').fold(root.clone(), |p, part| p.join(part)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree { root: PathBuf, sources: Sources }

    impl Tree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("xi-backups-{}", crate::backend::unique_id().unwrap()));
            let t = Tree { sources: Sources {
                ffxi: vec![root.join("ffxi")], windower: Some(root.join("win")), ashita: Some(root.join("ash")),
            }, root };
            for (path, body) in [
                ("ffxi/USER/macro.dat", "m1"), ("ffxi/USER/12345678/keys.dat", "k1"), ("ffxi/FFXiMain.dll", "game"),
                ("win/settings.xml", "<s/>"), ("win/scripts/init.txt", "i"), ("win/addons/fa/data/s.xml", "d"),
                ("win/addons/fa/fa.lua", "code"), ("win/plugins/settings/p.xml", "p"), ("win/other.txt", "x"),
                ("ash/config/a.ini", "c"), ("ash/config/boot/crystal-1.ini", "gen"), ("ash/config/boot/mine.ini", "own"),
                ("ash/scripts/s.txt", "s"), ("ash/addons/z.lua", "z"),
            ] {
                let p = t.root.join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, body).unwrap();
            }
            t
        }
        fn dir(&self) -> PathBuf { self.root.join("backups") }
        fn read(&self, rel: &str) -> String { std::fs::read_to_string(self.root.join(rel)).unwrap() }
    }

    impl Drop for Tree {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
    }

    fn names(archive: &Path) -> Vec<String> {
        let mut a = open_archive(archive).unwrap();
        let mut n: Vec<_> = (0..a.len()).map(|i| a.by_index(i).unwrap().name().to_owned()).collect();
        n.sort();
        n
    }

    /// Rewrites an archive with `edit` applied to each (name, bytes), keeping the old manifest.
    fn rewrite(archive: &Path, edit: impl Fn(&str, Vec<u8>) -> Vec<u8>) {
        let mut a = open_archive(archive).unwrap();
        let mut entries = Vec::new();
        for i in 0..a.len() {
            let mut e = a.by_index(i).unwrap();
            let mut b = Vec::new();
            e.read_to_end(&mut b).unwrap();
            entries.push((e.name().to_owned(), b));
        }
        drop(a);
        let mut w = zip::ZipWriter::new(File::create(archive).unwrap());
        for (n, b) in entries {
            w.start_file(n.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(&edit(&n, b)).unwrap();
        }
        w.finish().unwrap();
    }

    /// A well-formed archive (valid manifest) holding exactly `files`.
    fn craft(archive: &Path, files: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(archive).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let mut list = Vec::new();
        for (n, b) in files {
            w.start_file(*n, o).unwrap();
            w.write_all(b).unwrap();
            list.push(FileEntry { path: (*n).into(), size: b.len() as u64, sha256: hex(&Sha256::digest(b)) });
        }
        let m = Manifest { format: FORMAT, created: 1, reason: "manual".into(), roots: BTreeMap::new(), files: list };
        w.start_file(MANIFEST, o).unwrap();
        w.write_all(&serde_json::to_vec(&m).unwrap()).unwrap();
        w.finish().unwrap();
    }

    fn idle() -> Option<Vec<String>> { Some(vec!["explorer.exe".into()]) }

    #[test]
    fn create_list_verify_round_trip_with_the_right_files() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        assert!(info.file_name.ends_with("-manual.zip"));
        assert_eq!(info.parts, Parts::ALL);
        assert_eq!(info.reason, Reason::Manual);
        let all = names(&info.path);
        assert_eq!(all, [
            "ashita/config/a.ini", "ashita/config/boot/mine.ini", "ashita/scripts/s.txt",
            "ffxi/0/USER/12345678/keys.dat", "ffxi/0/USER/macro.dat", MANIFEST,
            "windower/addons/fa/data/s.xml", "windower/plugins/settings/p.xml", "windower/scripts/init.txt", "windower/settings.xml",
        ]);
        assert!(!all.iter().any(|n| n.contains("crystal-")), "generated boot files are excluded");
        assert_eq!(info.files, 9);
        assert_eq!(list(&t.dir()), vec![info.clone()]);
        let report = verify(&info.path).unwrap();
        assert!(report.ok(), "{report:?}");
        assert_eq!(report.checked, 9);
        // No temp file is left behind.
        assert!(std::fs::read_dir(t.dir()).unwrap().all(|e| e.unwrap().path().extension().unwrap() == "zip"));
    }

    #[test]
    fn missing_folders_are_skipped_and_nothing_to_back_up_is_an_error() {
        let t = Tree::new();
        let only_ffxi = Sources { ffxi: vec![t.root.join("nope"), t.root.join("ffxi")], windower: Some(t.root.join("nope")), ashita: None };
        let info = create(&t.dir(), &only_ffxi, Reason::Manual).unwrap();
        assert_eq!(info.parts, Parts { ffxi: true, ..Parts::default() });
        assert!(names(&info.path).contains(&"ffxi/1/USER/macro.dat".to_owned()));
        assert!(create(&t.dir(), &Sources::default(), Reason::Manual).is_err());
    }

    #[test]
    fn a_changed_entry_fails_verify_and_restore() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        rewrite(&info.path, |n, b| if n == "ffxi/0/USER/macro.dat" { b"tampered".to_vec() } else { b });
        let report = verify(&info.path).unwrap();
        assert_eq!(report.mismatched, ["ffxi/0/USER/macro.dat"]);
        let before = list(&t.dir()).len();
        let error = restore_with(&info.path, &t.sources, Parts::ALL, false, &idle).unwrap_err();
        assert!(error.contains("damaged"), "{error}");
        assert_eq!(t.read("ffxi/USER/macro.dat"), "m1");
        assert_eq!(list(&t.dir()).len(), before, "a refused restore makes no safety backup");
    }

    #[test]
    fn an_extra_or_missing_entry_is_reported() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        let mut a = open_archive(&info.path).unwrap();
        let manifest = read_manifest(&mut a).unwrap();
        drop(a);
        rewrite(&info.path, |_, b| b);
        // Drop one file entry and add a stray one.
        let mut w = zip::ZipWriter::new(File::create(t.root.join("x.zip")).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        for f in manifest.files.iter().skip(1) { w.start_file(f.path.as_str(), o).unwrap(); w.write_all(b"?").unwrap(); }
        w.start_file("stray.txt", o).unwrap();
        w.start_file(MANIFEST, o).unwrap();
        w.write_all(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        w.finish().unwrap();
        let r = verify(&t.root.join("x.zip")).unwrap();
        assert_eq!(r.missing, [manifest.files[0].path.clone()]);
        assert_eq!(r.extra, ["stray.txt"]);
    }

    #[test]
    fn unsafe_entry_names_are_rejected() {
        for bad in ["", "/abs", "a/../b", "../x", "a//b", "C:/x", "a/b:s", "\\\\srv\\share", "a\\b", "a/./b", "a/b.", "a/<b"] {
            assert!(!valid_entry_name(bad), "{bad:?}");
        }
        for good in ["ffxi/0/USER/macro.dat", "windower/addons/fa/data/s.xml", "ashita/config/a b.ini"] {
            assert!(valid_entry_name(good), "{good:?}");
        }
        let t = Tree::new();
        std::fs::create_dir_all(t.dir()).unwrap();
        let evil = t.dir().join("evil.zip");
        craft(&evil, &[("ffxi/0/USER/../../evil.txt", b"x")]);
        let error = restore_with(&evil, &t.sources, Parts::ALL, false, &idle).unwrap_err();
        assert!(error.contains("unsafe"), "{error}");
        assert!(!t.root.join("evil.txt").exists());
        assert!(!t.root.join("ffxi").join("evil.txt").exists());
    }

    #[test]
    fn restore_is_refused_while_a_program_runs() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        let before = list(&t.dir()).len();
        assert!(restore_with(&info.path, &t.sources, Parts::ALL, true, &idle).is_err());
        for name in ["POL.exe", "ffxi-boot.exe", "windower.EXE", "Ashita-cli.exe", "ashita.exe"] {
            let busy = || Some(vec!["a.exe".to_owned(), name.to_owned()]);
            let error = restore_with(&info.path, &t.sources, Parts::ALL, false, &busy).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
        assert!(restore_with(&info.path, &t.sources, Parts::ALL, false, &|| None).is_err());
        assert_eq!(list(&t.dir()).len(), before);
    }

    #[test]
    fn restore_writes_files_makes_a_before_restore_backup_and_deletes_nothing() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        std::fs::write(t.root.join("ffxi/USER/macro.dat"), "changed").unwrap();
        std::fs::remove_file(t.root.join("win/settings.xml")).unwrap();
        std::fs::write(t.root.join("ffxi/USER/new.dat"), "new").unwrap();
        let report = restore_with(&info.path, &t.sources, Parts::ALL, false, &idle).unwrap();
        assert_eq!((report.restored, report.skipped), (9, 0));
        assert_eq!(t.read("ffxi/USER/macro.dat"), "m1");
        assert_eq!(t.read("win/settings.xml"), "<s/>");
        assert_eq!(t.read("ffxi/USER/new.dat"), "new", "files not in the archive are kept");
        let safety = report.safety.unwrap();
        assert_eq!(safety.reason, Reason::BeforeRestore);
        assert!(names(&safety.path).contains(&"ffxi/0/USER/new.dat".to_owned()));
        // Restoring only one part leaves the others alone.
        std::fs::write(t.root.join("ffxi/USER/macro.dat"), "again").unwrap();
        std::fs::write(t.root.join("ash/scripts/s.txt"), "again").unwrap();
        let r = restore_with(&info.path, &t.sources, Parts { ashita: true, ..Parts::default() }, false, &idle).unwrap();
        assert_eq!((r.restored, r.skipped), (3, 6));
        assert_eq!(t.read("ffxi/USER/macro.dat"), "again");
        assert_eq!(t.read("ash/scripts/s.txt"), "s");
    }

    #[test]
    fn entries_restore_to_the_current_roots_only_when_present() {
        let t = Tree::new();
        let info = create(&t.dir(), &t.sources, Reason::Manual).unwrap();
        let moved = Sources { ffxi: vec![t.root.join("elsewhere")], windower: None, ashita: Some(t.root.join("ash")) };
        std::fs::create_dir_all(t.root.join("elsewhere")).unwrap();
        let r = restore_with(&info.path, &moved, Parts::ALL, false, &idle).unwrap();
        assert_eq!(r.restored, 5); // 2 ffxi + 3 ashita; windower has no current root
        assert_eq!(t.read("elsewhere/USER/macro.dat"), "m1");
        let two = Sources { ffxi: vec![], ..moved };
        assert_eq!(restore_with(&info.path, &two, Parts::ALL, false, &idle).unwrap().restored, 3);
    }

    #[test]
    fn prune_keeps_the_newest_n_and_at_least_one() {
        let t = Tree::new();
        std::fs::create_dir_all(t.dir()).unwrap();
        for i in 1..=5u64 {
            craft(&t.dir().join(format!("b{i}.zip")), &[("windower/a", b"a")]);
            // craft stamps created = 1; give each its own time through the manifest.
            let p = t.dir().join(format!("b{i}.zip"));
            let mut a = open_archive(&p).unwrap();
            let mut m = read_manifest(&mut a).unwrap();
            drop(a);
            m.created = i;
            let mut w = zip::ZipWriter::new(File::create(&p).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            w.start_file("windower/a", o).unwrap();
            w.write_all(b"a").unwrap();
            w.start_file(MANIFEST, o).unwrap();
            w.write_all(&serde_json::to_vec(&m).unwrap()).unwrap();
            w.finish().unwrap();
        }
        std::fs::write(t.dir().join("junk.zip"), b"not a zip").unwrap();
        assert_eq!(list(&t.dir()).iter().map(|b| b.created).collect::<Vec<_>>(), [5, 4, 3, 2, 1]);
        assert_eq!(prune(&t.dir(), 2), 3);
        assert_eq!(list(&t.dir()).iter().map(|b| b.created).collect::<Vec<_>>(), [5, 4]);
        assert_eq!(prune(&t.dir(), 0), 1, "keep is at least one");
        assert_eq!(list(&t.dir()).len(), 1);
    }

    #[test]
    fn before_maintenance_follows_the_setting_and_prunes() {
        let t = Tree::new();
        let config = Config { experiment: t.root.join("data"), ..Config::default() };
        let off = Settings { before_update: false, ..Settings::default() };
        assert_eq!(before_maintenance(&config, &t.sources, Maintenance::Update, &off).unwrap(), None);
        let on = Settings { keep: 2, before_update: true };
        for _ in 0..3 {
            let info = before_maintenance(&config, &t.sources, Maintenance::Repair, &on).unwrap().unwrap();
            assert_eq!(info.reason, Reason::BeforeRepair);
        }
        assert_eq!(list(&backups_dir(&config)).len(), 2);
        assert_eq!(before_maintenance(&config, &Sources::default(), Maintenance::Update, &on).unwrap(), None);
    }

    #[test]
    fn settings_load_from_old_files() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings { keep: 10, before_update: true });
    }
}
