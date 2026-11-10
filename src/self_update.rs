//! Signed launcher releases, downloaded separately from the game updater.
//! The release signature covers the version, URL, archive length and hash.
mod apply;
pub(crate) mod model;
/// The settings card's status as plain data for the Slint front end.
pub(crate) mod front;

use crate::{
    backend::Config,
    i18n::{t, tf},
    App,
};
use crate::effect::Effect;
use reqwest::{Client, Url};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use crate::pol_less::tools::embedded::{hex, sha256_hex};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const INDEX: &str = "https://api.github.com/repos/Offset437/FFXI-Crystal-Launcher/releases?per_page=10";
const RELEASE_PATH: &str = "/Offset437/FFXI-Crystal-Launcher/releases/download/";
const PUBLIC_KEY: &str = include_str!("../assets/update-public-key.hex");
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_METADATA: usize = 512 * 1024;
const FILES: [&str; 5] = [
    "Crystal-Launcher.exe",
    "README.md",
    "LICENSE",
    "THIRD_PARTY.md",
    "THIRD_PARTY_LICENSES.txt",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    format: u32,
    platform: String,
    version: String,
    url: String,
    size: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Release {
    payload: Payload,
    /// `payload.version`, parsed once by verify().
    version: semver::Version,
    envelope: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct Stage {
    release: Release,
    directory: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum State {
    #[default]
    Idle,
    Checking,
    Current,
    NotPublished,
    Available(Release),
    Downloading,
    Ready(Stage),
    Applying,
    Error(String),
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Check,
    Checked(Result<Option<Release>, String>),
    Download,
    Downloaded(Result<Stage, String>),
    Apply,
    Applied(Result<(), String>),
    AutoCheck(bool),
}

fn last_error_path(data: &Path) -> PathBuf {
    data.join("launcher-updates").join("last-error.txt")
}

pub(crate) fn initial(config: &Config) -> State {
    // Design previews read the real data folder; they must not consume or clean it.
    if std::env::args().any(|a| a.starts_with("--preview")) {
        return State::Idle;
    }
    let state = take_last_error(&config.experiment);
    clean_stale(
        &config.experiment,
        std::env::current_exe().ok().as_deref(),
        matches!(state, State::Error(_)),
    );
    state
}

/// A failed helper run is reported on the next start only, so later starts check again.
fn take_last_error(data: &Path) -> State {
    let path = last_error_path(data);
    let mut text = String::new();
    let Ok(file) = fs::File::open(&path) else {
        return State::Idle;
    };
    let read = file.take(32 * 1024).read_to_string(&mut text).is_ok();
    let _ = fs::remove_file(&path);
    if read && !text.is_empty() {
        State::Error(text)
    } else {
        State::Idle
    }
}

fn clear_last_error(data: &Path) {
    let _ = fs::remove_file(last_error_path(data));
}

/// Best effort: staging folders from earlier runs, and package backups beside the exe once
/// no failure needs them; a backup whose restore failed is never removed. Root-level notes
/// in launcher-updates stay.
fn clean_stale(data: &Path, exe: Option<&Path>, failed: bool) {
    if let Ok(entries) = fs::read_dir(data.join("launcher-updates")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    let Some(folder) = exe.and_then(Path::parent) else {
        return;
    };
    if failed {
        return; // The error text may name the backup holding the previous package.
    }
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".crystal-previous-")
                && entry.file_type().is_ok_and(|t| t.is_dir())
                && !entry.path().join(apply::RESTORE_FAILED).exists()
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
}

fn read_metadata(path: &Path) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.take(MAX_METADATA as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > MAX_METADATA {
        return Err(t("Launcher update metadata is too large.").into());
    }
    Ok(data)
}

fn bytes<const N: usize>(hex: &str) -> Result<[u8; N], String> {
    if hex.len() != N * 2 || !hex.is_ascii() {
        return Err(t("Invalid launcher update signature or checksum.").into());
    }
    let mut output = [0; N];
    for (i, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| (b as char).to_digit(16).map(|x| x as u8);
        output[i] = digit(pair[0])
            .zip(digit(pair[1]))
            .map(|(a, b)| (a << 4) | b)
            .ok_or(t("Invalid launcher update signature or checksum."))?;
    }
    Ok(output)
}

fn trusted_key() -> Result<[u8; 32], String> {
    let key = bytes(PUBLIC_KEY.trim())?;
    if key == [0; 32] {
        return Err(t("Launcher release signing has not been configured.").into());
    }
    Ok(key)
}

fn asset_url(url: &Url, extension: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !url.path().contains('%')
        && url.path().strip_prefix(RELEASE_PATH).is_some_and(|rest| {
            let parts: Vec<_> = rest.split('/').collect();
            parts.len() == 2
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && parts[1].ends_with(extension)
        })
}

fn download_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn client() -> Result<Client, String> {
    Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && download_host(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Launcher update redirect left GitHub's HTTPS release hosts")
            }
        }))
        .user_agent(concat!("Crystal-Launcher/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn verify(envelope: &[u8], key: &[u8; 32]) -> Result<Release, String> {
    if envelope.len() > MAX_METADATA {
        return Err(t("Launcher update metadata is too large.").into());
    }
    let signed: Envelope =
        serde_json::from_slice(envelope).map_err(|_| t("Invalid launcher update metadata."))?;
    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(signed.payload.as_bytes(), &bytes::<64>(&signed.signature)?)
        .map_err(|_| t("Launcher update signature did not verify."))?;
    let payload: Payload = serde_json::from_str(&signed.payload)
        .map_err(|_| t("Invalid launcher update metadata."))?;
    let version = semver::Version::parse(&payload.version)
        .map_err(|_| t("Invalid launcher update version."))?;
    if payload.format != 1
        || payload.platform != "windows-x86_64"
        || version.major > u32::MAX as u64
        || payload.size == 0
        || payload.size > MAX_ARCHIVE
        || !asset_url(
            &Url::parse(&payload.url).map_err(|_| t("Invalid launcher update URL."))?,
            ".zip",
        )
    {
        return Err(t("Launcher update is not a supported Windows release.").into());
    }
    bytes::<32>(&payload.sha256)?;
    Ok(Release {
        payload,
        version,
        envelope: envelope.to_vec(),
    })
}

async fn body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(t("Launcher update metadata is too large.").into());
    }
    let mut output = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(t("Launcher update metadata is too large.").into());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

#[derive(Deserialize)]
struct PublishedRelease {
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

async fn check() -> Result<Option<Release>, String> {
    let key = trusted_key()?;
    let client = client()?;
    let index = client
        .get(INDEX)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if index.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(t("GitHub launcher releases are unavailable. Private builds need the supplied installer.").into());
    }
    let releases: Vec<PublishedRelease> = serde_json::from_slice(
        &body(
            index.error_for_status().map_err(|e| e.to_string())?,
            MAX_METADATA,
        )
        .await?,
    )
    .map_err(|_| t("GitHub returned invalid launcher release metadata."))?;
    let mut candidates = Vec::new();
    // Pre-releases are for testers (builds that are pre-releases themselves); the GitHub API
    // never lists drafts to anonymous callers.
    let tester = !current_version().pre.is_empty();
    for release in releases
        .into_iter()
        .filter(|r| offered(r, tester))
        .take(10)
    {
        let Some(asset) = release
            .assets
            .into_iter()
            .find(|a| a.name == "launcher-update.json")
        else {
            continue;
        };
        candidates.push(fetch_release(&client, &asset.browser_download_url, &key).await);
    }
    newest(candidates, &current_version())
}

/// Whether a published release is considered: never a draft, a flagged pre-release only on
/// a pre-release build (`newest` then compares versions).
fn offered(release: &PublishedRelease, tester: bool) -> bool {
    !release.draft && (tester || !release.prerelease)
}

async fn fetch_release(client: &Client, url: &str, key: &[u8; 32]) -> Result<Release, String> {
    let url = Url::parse(url).map_err(|_| t("Invalid launcher update URL."))?;
    if !asset_url(&url, ".json") {
        return Err(t("Invalid launcher update URL.").into());
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    verify(&body(response, MAX_METADATA).await?, key)
}

/// The highest verified release. A broken release is skipped; its error is reported only
/// when no release verifies. A stable build never picks a signed pre-release version.
fn newest(
    candidates: Vec<Result<Release, String>>,
    current: &semver::Version,
) -> Result<Option<Release>, String> {
    let mut newest: Option<Release> = None;
    let mut first_error = None;
    for candidate in candidates {
        match candidate {
            Ok(release) if current.pre.is_empty() && !release.version.pre.is_empty() => {}
            Ok(release) => {
                if newest.as_ref().is_none_or(|r| release.version > r.version) {
                    newest = Some(release);
                }
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    match (newest, first_error) {
        (None, Some(error)) => Err(error),
        (newest, _) => Ok(newest),
    }
}

fn verify_archive(path: &Path, release: &Release) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() != release.payload.size {
        return Err(t("Launcher update download is incomplete.").into());
    }
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|e| e.to_string())?;
    if hash.finalize().as_slice() != bytes::<32>(&release.payload.sha256)? {
        return Err(t("Launcher update checksum did not match.").into());
    }
    Ok(())
}

/// Exact names only: no paths, duplicate entries, symlinks or unexpected files.
fn unpack(archive: &Path, directory: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    if zip.len() != FILES.len() {
        return Err(t("Launcher update package has unexpected files.").into());
    }
    let mut found = std::collections::BTreeSet::new();
    let mut expanded = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_owned();
        expanded = expanded.saturating_add(entry.size());
        if !FILES.contains(&name.as_str())
            || !found.insert(name.clone())
            || entry.is_dir()
            || entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
            || expanded > MAX_ARCHIVE
        {
            return Err(t("Launcher update package has unexpected files.").into());
        }
        let path = directory.join(&name);
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let count = std::io::copy(&mut (&mut entry).take(MAX_ARCHIVE + 1), &mut output)
            .map_err(|e| e.to_string())?;
        if count > MAX_ARCHIVE || count != entry.size() {
            return Err(t("Launcher update package is incomplete.").into());
        }
        output.sync_all().map_err(|e| e.to_string())?;
    }
    let mut file = fs::File::open(directory.join(FILES[0])).map_err(|e| e.to_string())?;
    let mut header = vec![0; 1024];
    let count = file.read(&mut header).map_err(|e| e.to_string())?;
    header.truncate(count);
    if !windows_executable(&header) {
        return Err(t("Launcher update is not a 64-bit Windows executable.").into());
    }
    Ok(())
}

fn windows_executable(header: &[u8]) -> bool {
    if header.len() < 64 || &header[..2] != b"MZ" {
        return false;
    }
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap()) as usize;
    header
        .get(offset..offset.saturating_add(6))
        .is_some_and(|pe| pe == b"PE\0\0\x64\x86")
}

async fn download(release: Release, data: PathBuf) -> Result<Stage, String> {
    // Recheck the signed fields before touching the staging directory.
    let release = verify(&release.envelope, &trusted_key()?)?;
    let directory = data.join("launcher-updates").join(format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let staged = stage(&release, &directory).await;
    if staged.is_err() {
        let _ = fs::remove_dir_all(&directory);
    }
    staged.map(|()| Stage { release, directory })
}

async fn stage(release: &Release, directory: &Path) -> Result<(), String> {
    let archive = directory.join("release.zip");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive)
        .map_err(|e| e.to_string())?;
    let mut response = client()?
        .get(&release.payload.url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    if response
        .content_length()
        .is_some_and(|n| n != release.payload.size)
    {
        return Err(t("Launcher update download has an unexpected length.").into());
    }
    let mut downloaded = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > release.payload.size {
            return Err(t("Launcher update download has an unexpected length.").into());
        }
        file.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    verify_archive(&archive, release)?;
    fs::write(directory.join("launcher-update.json"), &release.envelope)
        .map_err(|e| e.to_string())?;
    let extracted = directory.join("verified");
    fs::create_dir(&extracted).map_err(|e| e.to_string())?;
    unpack(&archive, &extracted)
}

type Action = Option<(&'static str, Msg)>;

/// Status text and action for the states the banner and the settings card share.
fn shared_status(app: &App) -> Option<(String, Action)> {
    Some(match &app.self_update {
        State::Available(release) => (
            tf(
                "Crystal Launcher {version} is available.",
                &[("version", &release.payload.version)],
            ),
            Some((t("Update and restart"), Msg::Download)),
        ),
        State::Downloading => (
            t("Downloading and verifying the launcher update…").into(),
            None,
        ),
        State::Applying => (t("Restarting Crystal Launcher…").into(), None),
        _ => return None,
    })
}

fn restart_action(app: &App) -> Action {
    app.can_restart_launcher()
        .then_some((t("Restart to update"), Msg::Apply))
}

pub(crate) fn update(state: &mut State, msg: Msg, config: &Config, can_restart: bool) -> Effect<Msg> {
    match msg {
        // Never while busy, and never over a found or downloaded update (that would drop it).
        Msg::Check
            if matches!(
                state,
                State::Idle | State::Current | State::NotPublished | State::Error(_)
            ) =>
        {
            *state = State::Checking;
            return Effect::perform(check(), Msg::Checked);
        }
        Msg::Checked(result) => {
            if result.is_ok() {
                clear_last_error(&config.experiment);
            }
            *state = match result {
                Ok(Some(release)) if release.version > current_version() => {
                    State::Available(release)
                }
                Ok(Some(_)) => State::Current,
                Ok(None) => State::NotPublished,
                Err(error) => State::Error(error),
            };
        }
        Msg::Download => {
            if let State::Available(release) = state {
                let release = release.clone();
                *state = State::Downloading;
                return Effect::perform(
                    download(release, config.experiment.clone()),
                    Msg::Downloaded,
                );
            }
        }
        Msg::Downloaded(result) => {
            if result.is_ok() {
                clear_last_error(&config.experiment);
            }
            *state = match result {
                Ok(stage) => State::Ready(stage),
                Err(error) => State::Error(error),
            };
            if can_restart && matches!(state, State::Ready(_)) {
                return Effect::done(Msg::Apply);
            }
        }
        Msg::Apply if can_restart => {
            if let State::Ready(stage) = state {
                let stage = stage.clone();
                *state = State::Applying;
                return Effect::perform(
                    async move {
                        tokio::task::spawn_blocking(move || apply::start(&stage))
                            .await
                            .unwrap_or_else(|_| {
                                Err(t("The launcher update helper could not start.").into())
                            })
                    },
                    Msg::Applied,
                );
            }
        }
        Msg::Applied(Err(error)) => *state = State::Error(error),
        _ => {}
    }
    Effect::none()
}

/// Release tooling and the post-exit helper run before account storage or UI.
pub(crate) fn cli(args: &[String]) -> Option<Result<(), String>> {
    match args.get(1).map(String::as_str) {
        Some("--apply-launcher-update") => Some(apply::run(args)),
        Some("--create-launcher-update-key") => Some((|| {
            let path = PathBuf::from(
                args.get(2)
                    .ok_or("An absolute private-key path is required.")?,
            );
            if !path.is_absolute() {
                return Err("An absolute private-key path is required.".into());
            }
            let document =
                signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                    .map_err(|_| "Windows could not generate the release signing key.")?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| e.to_string())?;
            file.write_all(document.as_ref())
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
            let key = signature::Ed25519KeyPair::from_pkcs8(document.as_ref())
                .map_err(|_| "Invalid generated key.")?;
            println!("{}", hex(key.public_key().as_ref()));
            Ok(())
        })()),
        Some("--sign-launcher-release") => Some(sign_release(args)),
        _ => None,
    }
}

fn sign_release(args: &[String]) -> Result<(), String> {
    if args.len() != 7 {
        return Err(
            "Expected: private-key version package-folder archive-URL output-folder".into(),
        );
    }
    let key_path = Path::new(&args[2]);
    let version = &args[3];
    let package = Path::new(&args[4]);
    let url = &args[5];
    let output = Path::new(&args[6]);
    if !key_path.is_absolute()
        || !package.is_absolute()
        || !output.is_absolute()
        || version != env!("CARGO_PKG_VERSION")
    {
        return Err("Use absolute paths and the version of this built launcher.".into());
    }
    let url = Url::parse(url).map_err(|e| e.to_string())?;
    if !asset_url(&url, ".zip") {
        return Err("The archive URL must belong to Crystal's GitHub releases.".into());
    }
    let private = zeroize::Zeroizing::new(fs::read(key_path).map_err(|e| e.to_string())?);
    let key = signature::Ed25519KeyPair::from_pkcs8(&private)
        .map_err(|_| "Invalid release signing key.")?;
    if key.public_key().as_ref() != trusted_key()? {
        return Err("Signing key does not match this launcher's public key.".into());
    }
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    let filename = url
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .ok_or("Missing archive name.")?;
    let archive = output.join(filename);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive)
        .map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    for name in FILES {
        let path = package.join(name);
        if !path.is_file() {
            return Err(format!("Package is missing {name}."));
        }
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .map_err(|e| e.to_string())?;
        std::io::copy(
            &mut fs::File::open(path).map_err(|e| e.to_string())?,
            &mut zip,
        )
        .map_err(|e| e.to_string())?;
    }
    zip.finish()
        .map_err(|e| e.to_string())?
        .sync_all()
        .map_err(|e| e.to_string())?;
    let archive_bytes = fs::read(&archive).map_err(|e| e.to_string())?;
    let payload = Payload {
        format: 1,
        platform: "windows-x86_64".into(),
        version: version.clone(),
        url: url.to_string(),
        size: archive_bytes.len() as u64,
        sha256: sha256_hex(&archive_bytes),
    };
    let payload = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let signed = Envelope {
        signature: hex(key.sign(payload.as_bytes()).as_ref()),
        payload,
    };
    let metadata = serde_json::to_vec_pretty(&signed).map_err(|e| e.to_string())?;
    verify(&metadata, &trusted_key()?)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("launcher-update.json"))
        .map_err(|e| e.to_string())?;
    file.write_all(&metadata)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    println!("Signed {filename} and launcher-update.json; nothing was uploaded.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(version: &str) -> (Vec<u8>, [u8; 32]) {
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&[17; 32]).unwrap();
        let payload = Payload { format: 1, platform: "windows-x86_64".into(), version: version.into(),
            url: "https://github.com/Offset437/FFXI-Crystal-Launcher/releases/download/v0.2.0/Crystal-Launcher.zip".into(),
            size: 400, sha256: "ab".repeat(32) };
        let payload = serde_json::to_string(&payload).unwrap();
        let envelope = Envelope {
            signature: hex(key.sign(payload.as_bytes()).as_ref()),
            payload,
        };
        (
            serde_json::to_vec(&envelope).unwrap(),
            key.public_key().as_ref().try_into().unwrap(),
        )
    }

    fn release(version: &str) -> Release {
        let (data, key) = signed(version);
        verify(&data, &key).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "crystal-update-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn one_broken_release_does_not_hide_a_valid_newer_one() {
        let current = semver::Version::parse("0.2.0").unwrap();
        let picked = newest(
            vec![Ok(release("0.5.0")), Err("bad signature".into()), Ok(release("0.4.1"))],
            &current,
        )
        .unwrap()
        .unwrap();
        assert_eq!(picked.payload.version, "0.5.0");
        // A backport published later still loses to the higher version.
        let picked = newest(vec![Ok(release("0.4.2")), Ok(release("0.5.0"))], &current)
            .unwrap()
            .unwrap();
        assert_eq!(picked.payload.version, "0.5.0");
        assert_eq!(
            newest(vec![Err("first".into()), Err("second".into())], &current).unwrap_err(),
            "first"
        );
        assert!(newest(vec![], &current).unwrap().is_none());
    }

    #[test]
    fn stable_builds_skip_signed_pre_release_versions() {
        let stable = semver::Version::parse("0.2.0").unwrap();
        let picked = newest(vec![Ok(release("0.3.0-beta.1")), Ok(release("0.2.1"))], &stable)
            .unwrap()
            .unwrap();
        assert_eq!(picked.payload.version, "0.2.1");
        let beta = semver::Version::parse("0.3.0-beta.1").unwrap();
        let picked = newest(vec![Ok(release("0.3.0-beta.2"))], &beta).unwrap().unwrap();
        assert_eq!(picked.payload.version, "0.3.0-beta.2");
        let flagged: Vec<PublishedRelease> =
            serde_json::from_str(r#"[{"prerelease":true,"assets":[]},{"assets":[]}]"#).unwrap();
        assert!(flagged[0].prerelease && !flagged[1].prerelease);
        assert!(!offered(&flagged[0], false) && offered(&flagged[1], false));
        assert!(offered(&flagged[0], true) && offered(&flagged[1], true), "a beta build is offered the next beta");
    }

    #[test]
    fn a_failed_update_is_reported_once_and_staging_is_cleaned() {
        let root = scratch("stale");
        let data = root.join("data");
        let updates = data.join("launcher-updates");
        fs::create_dir_all(updates.join("123-456/verified")).unwrap();
        fs::write(updates.join("last-registration-warning.txt"), "warning").unwrap();
        fs::write(updates.join("last-error.txt"), "helper failed").unwrap();
        let install = root.join("install");
        fs::create_dir_all(install.join(".crystal-previous-1-2")).unwrap();
        fs::create_dir_all(install.join("keep")).unwrap();
        let exe = install.join("Crystal-Launcher.exe");

        let state = take_last_error(&data);
        assert!(matches!(&state, State::Error(e) if e == "helper failed"));
        clean_stale(&data, Some(&exe), true);
        assert!(!updates.join("123-456").exists());
        assert!(updates.join("last-registration-warning.txt").exists());
        // The failure may name this backup, so it survives the start that reports it.
        assert!(install.join(".crystal-previous-1-2").exists());

        assert!(matches!(take_last_error(&data), State::Idle));
        fs::create_dir_all(install.join(".crystal-previous-3-4")).unwrap();
        fs::write(install.join(".crystal-previous-3-4").join(apply::RESTORE_FAILED), "").unwrap();
        clean_stale(&data, Some(&exe), false);
        assert!(!install.join(".crystal-previous-1-2").exists());
        assert!(install.join(".crystal-previous-3-4").exists(), "its restore failed: the only old copy");
        assert!(install.join("keep").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checking_never_replaces_a_found_or_downloaded_update() {
        let root = scratch("ready");
        let config = Config {
            experiment: root.clone(),
            repository: root.clone(),
            game_install: None,
            dat_overlays: vec![],
        };
        let stage = Stage {
            release: release("9.0.0"),
            directory: root.join("stage"),
        };
        for mut state in [
            State::Ready(stage),
            State::Available(release("9.0.0")),
            State::Checking,
            State::Downloading,
            State::Applying,
        ] {
            let before = format!("{state:?}");
            let _ = update(&mut state, Msg::Check, &config, false);
            assert_eq!(format!("{state:?}"), before);
        }
        let mut state = State::Error("old".into());
        let _ = update(&mut state, Msg::Check, &config, false);
        assert!(matches!(state, State::Checking));
        fs::create_dir_all(root.join("launcher-updates")).unwrap();
        fs::write(last_error_path(&root), "old").unwrap();
        let _ = update(&mut state, Msg::Checked(Ok(None)), &config, false);
        assert!(matches!(state, State::NotPublished));
        assert!(!last_error_path(&root).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signature_rejects_tampering_and_other_keys() {
        let (data, key) = signed("0.2.0");
        assert_eq!(verify(&data, &key).unwrap().payload.version, "0.2.0");
        let mut envelope: Envelope = serde_json::from_slice(&data).unwrap();
        envelope.payload = envelope.payload.replace("0.2.0", "0.3.0");
        assert!(verify(&serde_json::to_vec(&envelope).unwrap(), &key).is_err());
        assert!(verify(&data, &[0; 32]).is_err());
    }

    #[test]
    fn only_crystals_https_release_assets_are_accepted() {
        let good = "https://github.com/Offset437/FFXI-Crystal-Launcher/releases/download/v1/Crystal-Launcher.zip";
        assert!(asset_url(&Url::parse(good).unwrap(), ".zip"));
        for bad in [
            good.replace("https:", "http:"),
            good.replace("Offset437", "someone"),
            format!("{good}?target=other"),
            good.replace("github.com", "github.com.evil.example"),
            good.replace("/v1/", "/%2e%2e/"),
            good.replace("https://", "https://user@"),
        ] {
            assert!(!asset_url(&Url::parse(&bad).unwrap(), ".zip"), "{bad}");
        }
    }

    #[test]
    fn wrong_platform_and_unbounded_packages_fail_even_when_signed() {
        let (data, key) = signed("0.2.0");
        let signer = signature::Ed25519KeyPair::from_seed_unchecked(&[17; 32]).unwrap();
        let original: Envelope = serde_json::from_slice(&data).unwrap();
        for replacement in [
            original.payload.replace("windows-x86_64", "linux"),
            original
                .payload
                .replace("\"size\":400", "\"size\":999999999999"),
        ] {
            let envelope = Envelope {
                signature: hex(signer.sign(replacement.as_bytes()).as_ref()),
                payload: replacement,
            };
            assert!(verify(&serde_json::to_vec(&envelope).unwrap(), &key).is_err());
        }
    }

    #[test]
    fn executable_header_is_checked_without_unbounded_offsets() {
        let mut header = [0u8; 256];
        header[..2].copy_from_slice(b"MZ");
        header[60..64].copy_from_slice(&128u32.to_le_bytes());
        header[128..134].copy_from_slice(b"PE\0\0\x64\x86");
        assert!(windows_executable(&header));
        header[132..134].copy_from_slice(&0x14cu16.to_le_bytes());
        assert!(!windows_executable(&header));
        header[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!windows_executable(&header));
    }

    #[test]
    fn downloaded_archives_reject_same_size_changes_and_truncation() {
        let root = std::env::temp_dir().join(format!(
            "crystal-update-hash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let archive = root.join("release.zip");
        let (metadata, key) = signed("0.2.0");
        let mut release = verify(&metadata, &key).unwrap();
        let original = vec![7; 400];
        release.payload.sha256 = sha256_hex(&original);
        fs::write(&archive, &original).unwrap();
        verify_archive(&archive, &release).unwrap();
        let mut changed = original;
        changed[99] ^= 1;
        fs::write(&archive, &changed).unwrap();
        assert!(verify_archive(&archive, &release).is_err());
        fs::write(&archive, &changed[..100]).unwrap();
        assert!(verify_archive(&archive, &release).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn extraction_accepts_the_package_and_rejects_paths_and_foreign_files() {
        let root = std::env::temp_dir().join(format!(
            "crystal-update-zip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let mut header = [0u8; 256];
        header[..2].copy_from_slice(b"MZ");
        header[60..64].copy_from_slice(&128u32.to_le_bytes());
        header[128..134].copy_from_slice(b"PE\0\0\x64\x86");
        for (index, replacement) in [None, Some("../README.md"), Some("surprise.exe")]
            .iter()
            .enumerate()
        {
            let archive = root.join(format!("case-{index}.zip"));
            let directory = root.join(format!("case-{index}"));
            fs::create_dir(&directory).unwrap();
            let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
            for (file_index, name) in FILES.iter().enumerate() {
                let name = if file_index == 1 {
                    replacement.unwrap_or(name)
                } else {
                    name
                };
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(if file_index == 0 { &header } else { b"notice" })
                    .unwrap();
            }
            zip.finish().unwrap();
            let result = unpack(&archive, &directory);
            assert_eq!(result.is_ok(), replacement.is_none());
        }
        assert!(!root.join("README.md").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
