//! The official Windows client distribution. No unofficial repack or silent MSI install.
//! File lengths describe the current official archive, not a cryptographic checksum.
use crate::i18n::{t, tf};
use crate::operation::{Operation, Part, PartState, Stage};
use reqwest::{header, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const INSTRUCTIONS_URL: &str =
    "https://www.playonline.com/ff11us/download/media/install_win.html";
const BASE_URL: &str = "https://gdl.square-enix.com/ffxi/download/us/";
// Downloads use current server metadata, never stored lengths: historical lengths
// cannot establish provenance, so untracked local volumes must be downloaded officially.
const PARTS: [&str; 5] = [
    "FFXIFullSetup_US.part1.exe",
    "FFXIFullSetup_US.part2.rar",
    "FFXIFullSetup_US.part3.rar",
    "FFXIFullSetup_US.part4.rar",
    "FFXIFullSetup_US.part5.rar",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Metadata {
    url: String,
    length: u64,
    etag: Option<String>,
    modified: Option<String>,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    format: u32,
}

impl Metadata {
    fn validator(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|s| !s.starts_with("W/"))
            .or(self.modified.as_deref())
    }

    fn can_resume(&self, current: &Self) -> bool {
        self.url == current.url
            && self.format == 1
            && current.format == 1
            && self.length == current.length
            && self.validator().is_some()
            && self.validator() == current.validator()
    }
}

fn official_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("gdl.square-enix.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

fn client() -> Result<Client, String> {
    Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error(t("Too many installer redirects"))
            } else if official_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error(t("Installer redirect left the official HTTPS download host"))
            }
        }))
        // Square Enix's download host sees a browser, as for a player downloading it.
        .user_agent(crate::services::browser_user_agent())
        .build()
        .map_err(|e| e.to_string())
}

/// `future`, unless the operation is cancelled first (then the cancel message).
async fn cancellable<F: Future>(future: F, op: &Operation) -> Result<F::Output, String> {
    op.check_cancelled()?;
    tokio::select! {
        result = future => Ok(result),
        message = op.cancelled_wait() => Err(message),
    }
}

fn text_header(
    headers: &header::HeaderMap,
    key: header::HeaderName,
) -> Result<Option<String>, String> {
    headers
        .get(key)
        .map(|v| {
            v.to_str()
                .map(str::to_owned)
                .map_err(|_| t("Malformed installer HTTP header").to_owned())
        })
        .transpose()
}

fn length_header(headers: &header::HeaderMap) -> Result<u64, String> {
    text_header(headers, header::CONTENT_LENGTH)?
        .ok_or(t("Official server omitted Content-Length"))?
        .parse::<u64>()
        .map_err(|_| t("Invalid installer Content-Length").to_owned())
}

fn identity_encoding(headers: &header::HeaderMap) -> Result<(), String> {
    if let Some(value) = text_header(headers, header::CONTENT_ENCODING)? {
        if !value.eq_ignore_ascii_case("identity") {
            return Err(t("Official installer returned an encoded body; byte resumption is unsafe").into());
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

fn content_range(value: &str) -> Result<ContentRange, String> {
    let fail = || t("Invalid installer Content-Range").to_owned();
    let value = value.strip_prefix("bytes ").ok_or_else(fail)?;
    let (range, total) = value.split_once('/').ok_or_else(fail)?;
    let (start, end) = range.split_once('-').ok_or_else(fail)?;
    let result = ContentRange {
        start: start.parse().map_err(|_| fail())?,
        end: end.parse().map_err(|_| fail())?,
        total: total.parse().map_err(|_| fail())?,
    };
    if result.start > result.end || result.end >= result.total {
        return Err(fail());
    }
    Ok(result)
}

// A 200 response to Range is legal: the server ignored Range/If-Range, so start
// over. A 206 must describe exactly the remaining bytes before any append.
fn response_offset(
    status: StatusCode,
    headers: &header::HeaderMap,
    offset: u64,
    expected: u64,
) -> Result<u64, String> {
    identity_encoding(headers)?;
    if status == StatusCode::OK {
        if length_header(headers)? != expected || headers.contains_key(header::CONTENT_RANGE) {
            return Err(t("Official installer full response length changed").into());
        }
        return Ok(0);
    }
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(tf(
            "Official installer HTTP {status}; partial download preserved",
            &[("status", &status)],
        ));
    }
    let range = content_range(
        &text_header(headers, header::CONTENT_RANGE)?
            .ok_or(t("Partial installer response omitted Content-Range"))?,
    )?;
    if range.start != offset
        || range.total != expected
        || range.end != expected - 1
        || length_header(headers)? != expected - offset
    {
        return Err(t("Official installer range does not match the requested remaining bytes").into());
    }
    Ok(offset)
}

fn file_length(path: &Path) -> Result<Option<u64>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && !is_reparse_file(&meta) => {
            Ok(Some(meta.len()))
        }
        Ok(_) => Err(tf(
            "Installer path must be a regular file: {path}",
            &[("path", &path.display())],
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn is_reparse_file(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn safe_directory(dir: &Path) -> Result<PathBuf, String> {
    if !dir.is_absolute() {
        return Err(t("Choose an absolute installer download folder").into());
    }
    fs::create_dir_all(dir).map_err(|e| tf("Create installer folder: {error}", &[("error", &e)]))?;
    // The user may deliberately choose a junction-backed download directory.
    // Resolve that directory once; individual children must be ordinary files.
    fs::canonicalize(dir).map_err(|e| tf("Resolve installer folder: {error}", &[("error", &e)]))
}

fn download_lock(dir: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    use std::os::windows::fs::OpenOptionsExt;
    // A permanent small lock file; the OS releases the exclusive handle on
    // crash, so a stale file cannot strand a resumable multi-GB download.
    options.share_mode(0);
    let path = dir.join(".crystal-official-installer.lock");
    file_length(&path)?;
    options
        .open(path)
        .map_err(|e| tf("Installer folder is already in use or unavailable: {error}", &[("error", &e)]))
}

/// Downloads all five official volumes into one folder. Existing complete
/// volumes are retained; interrupted `.part` files resume only with validators.
/// This function does not start extraction or modify an installed game.
/// The summary line of a completed download.
pub async fn download(dir: PathBuf, operation: Operation) -> Result<String, String> {
    operation.check_cancelled()?;
    let dir = safe_directory(&dir)?;
    let _lock = download_lock(&dir)?;
    let client = client()?;
    let mut volumes = Vec::new();
    for (index, name) in PARTS.iter().enumerate() {
        operation.progress_part(
            Stage::InstallerMetadata,
            index as u64,
            PARTS.len() as u64,
            Part { number: index as u8 + 1, name, state: PartState::Checking },
        );
        let url = format!("{BASE_URL}{name}");
        let head = cancellable(
            client
                .head(&url)
                .header(header::ACCEPT_ENCODING, "identity")
                .send(),
            &operation,
        )
        .await?
        .map_err(|e| tf("Installer metadata: {error}", &[("error", &e)]))?;
        if head.status() != StatusCode::OK || !official_url(head.url()) {
            return Err(tf(
                "Official installer metadata HTTP {status}",
                &[("status", &head.status())],
            ));
        }
        identity_encoding(head.headers())?;
        let length = length_header(head.headers())?;
        if length == 0 {
            return Err(tf("Official {name} is empty", &[("name", name)]));
        }
        volumes.push((
            *name,
            Metadata {
                url,
                length,
                etag: text_header(head.headers(), header::ETAG)?,
                modified: text_header(head.headers(), header::LAST_MODIFIED)?,
                complete: false,
                sha256: None,
                format: 1,
            },
        ));
    }
    let total = volumes
        .iter()
        .try_fold(0u64, |sum, (_, meta)| sum.checked_add(meta.length))
        .ok_or(t("Official installer total size overflow"))?;
    let mut completed = 0;
    let mut transferred = 0;
    for (index, (name, mut metadata)) in volumes.into_iter().enumerate() {
        operation.check_cancelled()?;
        let part = |state| Part { number: index as u8 + 1, name, state };
        let url = metadata.url.clone();
        let expected = metadata.length;
        let target = dir.join(name);
        let partial = dir.join(format!("{name}.part"));
        let sidecar = dir.join(format!("{name}.download.json"));
        let sidecar_exists = file_length(&sidecar)?.is_some();
        let old: Option<Metadata> = if sidecar_exists {
            fs::read(&sidecar)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        } else {
            None
        };
        let unchanged = old
            .as_ref()
            .is_some_and(|m| m.complete && m.can_resume(&metadata));
        let tracked_digest = old.as_ref().and_then(|m| m.sha256.as_deref());
        if file_length(&target)? == Some(expected) && unchanged && tracked_digest.is_some() {
            let hash_path = target.clone();
            let hash_op = operation.clone();
            let actual = tokio::task::spawn_blocking(move || {
                hash_file(&hash_path, expected, Some(&hash_op))
            })
            .await
            .map_err(|e| e.to_string())??;
            if Some(hex_digest(actual)) == tracked_digest.map(str::to_owned) {
                completed += expected;
                operation.progress_part(Stage::InstallerDownload, completed, total, part(PartState::AlreadyComplete));
                continue;
            }
            operation.progress_part(Stage::InstallerDownload, completed, total, part(PartState::Changed));
        }
        let existing = file_length(&partial)?.unwrap_or(0);
        let offset = if existing <= expected
            && old
                .as_ref()
                .is_some_and(|m| !m.complete && m.can_resume(&metadata))
        {
            existing
        } else {
            0
        };
        // Do not publish new metadata until stale bytes have been removed. A
        // crash between these steps must never relabel old bytes as new data.
        if offset == 0 {
            File::create(&partial).map_err(|e| tf("Create installer part: {error}", &[("error", &e)]))?;
        }
        save_metadata(&sidecar, &metadata, false)?;
        if offset == expected {
            let finish_op = operation.clone();
            tokio::task::spawn_blocking(move || {
                finish_partial(&partial, &target, &sidecar, &mut metadata, &finish_op)
            })
            .await
            .map_err(|e| e.to_string())??;
            completed += expected;
            operation.progress_part(Stage::InstallerDownload, completed, total, part(PartState::AlreadyComplete));
            continue;
        }
        let mut request = client.get(&url).header(header::ACCEPT_ENCODING, "identity");
        if offset > 0 {
            request = request
                .header(header::RANGE, format!("bytes={offset}-"))
                .header(
                    header::IF_RANGE,
                    metadata.validator().ok_or(t("Missing resume validator"))?,
                );
        }
        let mut response = cancellable(request.send(), &operation)
            .await?
            .map_err(|e| tf("Download {name}: {error}", &[("name", &name), ("error", &e)]))?;
        if !official_url(response.url()) {
            return Err(t("Installer left the official HTTPS host").into());
        }
        let mut done = response_offset(response.status(), response.headers(), offset, expected)?;
        // Check a returned validator too. Missing GET validators are allowed
        // because If-Range + 206 already expresses the server's match.
        {
            let etag = text_header(response.headers(), header::ETAG)?;
            let modified = text_header(response.headers(), header::LAST_MODIFIED)?;
            if (etag.is_some() && metadata.etag.is_some() && etag != metadata.etag)
                || (modified.is_some()
                    && metadata.modified.is_some()
                    && modified != metadata.modified)
            {
                return Err(t("Installer changed between metadata and resumed response").into());
            }
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(done == 0)
            .append(done > 0)
            .open(&partial)
            .map_err(|e| tf("Open installer part: {error}", &[("error", &e)]))?;
        let hash = if done == 0 {
            Sha256::new()
        } else {
            let hash_path = partial.clone();
            let hash_op = operation.clone();
            tokio::task::spawn_blocking(move || hash_file(&hash_path, done, Some(&hash_op)))
                .await
                .map_err(|e| e.to_string())??
        };
        let mut last_progress = Instant::now() - Duration::from_secs(1);
        operation.progress_part(
            Stage::InstallerDownload,
            completed + done,
            total,
            part(PartState::Downloading { done, expected }),
        );
        // One blocking writer for the whole volume, fed through a bounded channel, so the
        // socket keeps draining while bytes are written and hashed.
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let writer = spawn_writer(file, hash, receiver);
        let streamed = async {
            while let Some(chunk) = cancellable(response.chunk(), &operation)
                .await?
                .map_err(|e| tf("Download {name} interrupted: {error}. Retry to resume.", &[("name", &name), ("error", &e)]))?
            {
                operation.check_cancelled()?;
                let size = chunk.len() as u64;
                if done.checked_add(size).is_none_or(|next| next > expected) {
                    return Err(tf("{name} exceeds its official expected length", &[("name", &name)]));
                }
                if sender.send(chunk).await.is_err() {
                    break; // The writer failed; its error is reported below.
                }
                done += size;
                transferred += size;
                if last_progress.elapsed() >= Duration::from_millis(150) {
                    operation.progress_part(
                        Stage::InstallerDownload,
                        completed + done,
                        total,
                        part(PartState::Downloading { done, expected }),
                    );
                    last_progress = Instant::now();
                }
            }
            Ok(())
        }
        .await;
        // Every exit (end, interruption, cancel) lets the writer finish what it was given,
        // so the partial file holds exactly the bytes counted for a later resume.
        drop(sender);
        let (file, hash) = writer
            .await
            .map_err(|e| tf("Installer file worker: {error}", &[("error", &e)]))??;
        streamed?;
        if done != expected {
            return Err(tf(
                "{name} ended at {done} of {expected} bytes; retry to resume",
                &[("name", &name), ("done", &done), ("expected", &expected)],
            ));
        }
        metadata.sha256 = Some(hex_digest(hash));
        operation.check_cancelled()?;
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| tf("Flush installer part: {error}", &[("error", &e)]))?;
        operation.check_cancelled()?;
        publish(&partial, &target)?;
        save_metadata(&sidecar, &metadata, true)?;
        completed += expected;
        operation.progress_part(Stage::InstallerDownload, completed, total, part(PartState::Complete));
    }
    let verify_dir = dir.clone();
    let verify_op = operation.clone();
    tokio::task::spawn_blocking(move || validate_volumes(&verify_dir, Some(&verify_op)))
        .await
        .map_err(|e| e.to_string())??;
    operation.check_cancelled()?;
    Ok(tf(
        "All five official installer files are complete in {folder}. Downloaded {bytes} bytes this run. Transfer lengths were checked; Square Enix publishes no checksum on the download page. Extract next, then run the official setup and select all applications for a first installation.",
        &[("folder", &dir.display()), ("bytes", &transferred)],
    ))
}

/// Append every chunk `receiver` yields to `file` (buffered) and `hash`, until the sender is
/// dropped; then flush and hand both back. A write error ends it early, which the sender
/// sees as a closed channel.
fn spawn_writer<T: AsRef<[u8]> + Send + 'static>(
    file: File,
    mut hash: Sha256,
    mut receiver: tokio::sync::mpsc::Receiver<T>,
) -> tokio::task::JoinHandle<Result<(File, Sha256), String>> {
    tokio::task::spawn_blocking(move || {
        let write_error = |e: std::io::Error| tf("Write installer part: {error}", &[("error", &e)]);
        let mut out = std::io::BufWriter::with_capacity(4 << 20, file);
        while let Some(chunk) = receiver.blocking_recv() {
            out.write_all(chunk.as_ref()).map_err(write_error)?;
            hash.update(chunk.as_ref());
        }
        let file = out.into_inner().map_err(|e| write_error(e.into_error()))?;
        Ok((file, hash))
    })
}

fn finish_partial(partial: &Path, target: &Path, sidecar: &Path, metadata: &mut Metadata, operation: &Operation) -> Result<(), String> {
    metadata.sha256 = Some(hex_digest(hash_file(partial, metadata.length, Some(operation))?));
    // FlushFileBuffers requires a write handle on Windows, even when no more
    // bytes need downloading. A read-only File::open strands completed parts.
    OpenOptions::new().write(true).open(partial)
        .and_then(|file| file.sync_all())
        .map_err(|e| tf("Flush complete installer part: {error}", &[("error", &e)]))?;
    operation.check_cancelled()?;
    publish(partial, target)?;
    save_metadata(sidecar, metadata, true)
}

fn hash_file(path: &Path, expected: u64, operation: Option<&Operation>) -> Result<Sha256, String> {
    if file_length(path)? != Some(expected) {
        return Err(tf("{path} changed size while hashing", &[("path", &path.display())]));
    }
    let mut file = File::open(path).map_err(|e| tf("Hash installer file: {error}", &[("error", &e)]))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    let mut read = 0u64;
    loop {
        if let Some(op) = operation {
            op.check_cancelled()?;
        }
        let n = file
            .read(&mut buffer)
            .map_err(|e| tf("Read installer hash: {error}", &[("error", &e)]))?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > expected {
            return Err(t("Installer file grew while hashing").into());
        }
        hash.update(&buffer[..n]);
    }
    if read != expected {
        return Err(t("Installer file shrank while hashing").into());
    }
    Ok(hash)
}

fn hex_digest(hash: Sha256) -> String {
    format!("{:x}", hash.finalize())
}

fn save_metadata(path: &Path, metadata: &Metadata, complete: bool) -> Result<(), String> {
    let mut metadata = metadata.clone();
    metadata.complete = complete;
    let json = serde_json::to_vec(&metadata).map_err(|e| e.to_string())?;
    file_length(path)?;
    // The full new JSON is flushed before the old sidecar is replaced, so a crash
    // cannot truncate the only copy of the validators/completion state.
    crate::fsutil::write_atomic(path, &json).map_err(|failed| {
        if failed.replacing {
            tf("Publish installer file: {error}", &[("error", &failed.error)])
        } else {
            tf("Write installer resume metadata: {error}", &[("error", &failed.error)])
        }
    })
}

fn publish(partial: &Path, target: &Path) -> Result<(), String> {
    // Windows rename does not replace an existing file. Keep an incomplete old
    // file until the new volume is fully flushed, then atomically replace it.
    file_length(partial)?.ok_or(t("Completed installer file disappeared before publishing"))?;
    file_length(target)?;
    crate::fsutil::replace(partial, target)
        .map_err(|error| tf("Publish installer file: {error}", &[("error", &error)]))
}

fn validate_volumes(dir: &Path, operation: Option<&Operation>) -> Result<(), String> {
    for name in PARTS {
        let sidecar = dir.join(format!("{name}.download.json"));
        let metadata = match file_length(&sidecar)? {
            Some(_) => {
                let metadata: Metadata =
                    serde_json::from_slice(&fs::read(sidecar).map_err(|e| e.to_string())?)
                        .map_err(|_| {
                            tf("{name} has invalid download metadata; download again", &[("name", &name)])
                        })?;
                if metadata.url != format!("{BASE_URL}{name}")
                    || metadata.length == 0
                    || !metadata.complete
                    || metadata.format != 1
                    || metadata.sha256.as_ref().is_none_or(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    return Err(tf(
                        "{name} has invalid download metadata; download again",
                        &[("name", &name)],
                    ));
                }
                metadata
            }
            None => return Err(tf("{name} has no official-download metadata. Download all five files through the launcher first.", &[("name", &name)])),
        };
        if file_length(&dir.join(name))? != Some(metadata.length) {
            return Err(tf("{name} is missing or incomplete. Finish downloading all five files into this folder.", &[("name", &name)]));
        }
        let actual = hex_digest(hash_file(&dir.join(name), metadata.length, operation)?);
        if metadata.sha256.as_deref() != Some(actual.as_str()) {
            return Err(tf(
                "{name} changed after its official download; extraction was not started",
                &[("name", &name)],
            ));
        }
    }
    Ok(())
}

/// Runs the official self-extractor interactively, waiting for its exit. The
/// extraction destination is selected in that program. Run on a worker thread.
pub fn extract(dir: &Path) -> Result<(), String> {
    let dir = fs::canonicalize(dir).map_err(|e| tf("Installer folder: {error}", &[("error", &e)]))?;
    let _lock = download_lock(&dir)?;
    validate_volumes(&dir, None)?;
    launch(&dir.join(PARTS[0]), &dir)
}

/// Runs and waits for the official client installer interactively, preserving DirectX,
/// PlayOnline, FFXI registration and dependency installation behavior.
/// Accept either the download/extraction parent or the extracted folder itself.
pub fn install(dir: &Path) -> Result<(), String> {
    let executable = setup_executable(dir)?;
    launch(&executable, executable.parent().expect("setup has a parent"))
}

fn setup_executable(dir: &Path) -> Result<PathBuf, String> {
    let root = fs::canonicalize(dir).map_err(|_| t("Installer folder does not exist. Extract the official download first, then browse to the folder containing FFXISetup.exe.").to_owned())?;
    // The extractor allows any destination. Inspect contents rather than the
    // folder's leaf name so directly selected or renamed setup folders work.
    for executable in [root.join("FFXISetup.exe"), root.join("FFXIFullSetup_US/FFXISetup.exe")] {
        if file_length(&executable)?.unwrap_or(0) > 0 {
            return Ok(executable);
        }
    }
    Err(t("FFXISetup.exe was not found or is empty. Extract the official download, then browse to its FFXIFullSetup_US folder. In setup, select PlayOnline Viewer and FINAL FANTASY XI.").into())
}

fn launch(executable: &Path, cwd: &Path) -> Result<(), String> {
    use crate::prereqs::wait_exit_code;
    use crate::win::{shell_execute, ShellError};
    // The verified read handle stays open until the installer exits.
    let _file = {
        let file = lock_executable(executable)?;
        verify_authenticode(executable, &file)?;
        file
    };
    let untracked = || t("Could not track the official installer; wait for it to close before continuing.").to_owned();
    // Let the executable's manifest request UAC when needed. CreateProcess
    // (Command::status) only returns ERROR_ELEVATION_REQUIRED to a normal user.
    let process = shell_execute("open", executable, None, Some(cwd), windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL, true)
        .map_err(|error| match error {
            ShellError::Failed(code) => {
                tf("Run official installer: {error}", &[("error", &std::io::Error::from_raw_os_error(code as i32))])
            }
            ShellError::NoProcess => untracked(),
        })?;
    let status = wait_exit_code(process).ok_or_else(untracked)?;
    if status != 0 {
        return Err(tf("Official installer exited with {status}", &[("status", &status)]));
    }
    Ok(())
}

fn lock_executable(executable: &Path) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    file_length(executable)?.ok_or(t("Official installer executable is missing"))?;
    OpenOptions::new().read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(executable)
        .map_err(|e| tf("Run official installer: {error}", &[("error", &e)]))
}

fn verify_authenticode(executable: &Path, file: &File) -> Result<(), String> {
    let status = crate::prereqs::win_verify_trust(executable, file, |status, _| status);
    // WinRAR self-extractors may legitimately have no signature. HTTPS origin
    // and transfer checks do not imply Authenticode; do not claim they do.
    // Reject signatures that are present but fail Windows trust validation.
    if status != 0 && (status as u32 != 0x800B0100 || has_embedded_signature(executable)?) {
        return Err(tf(
            "Windows could not validate the Authenticode signature on {path} ({code}). The file was not launched. See the official download instructions: {url}",
            &[
                ("path", &executable.display()),
                ("code", &format!("0x{:08X}", status as u32)),
                ("url", &INSTRUCTIONS_URL),
            ],
        ));
    }
    Ok(())
}

fn has_embedded_signature(executable: &Path) -> Result<bool, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(executable).map_err(|e| tf("Read installer PE header: {error}", &[("error", &e)]))?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let mut dos = [0; 64];
    file.read_exact(&mut dos)
        .map_err(|e| tf("Read installer DOS header: {error}", &[("error", &e)]))?;
    if &dos[..2] != b"MZ" {
        return Err(t("Installer is not a Windows executable").into());
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut pe = [0; 24];
    file.read_exact(&mut pe)
        .map_err(|e| tf("Read installer PE header: {error}", &[("error", &e)]))?;
    if &pe[..4] != b"PE\0\0" {
        return Err(t("Invalid installer PE signature").into());
    }
    let size = u16::from_le_bytes(pe[20..22].try_into().unwrap()) as usize;
    let mut optional = vec![0; size];
    file.read_exact(&mut optional)
        .map_err(|e| tf("Read installer optional header: {error}", &[("error", &e)]))?;
    embedded_signature_directory(&optional, length)
}

fn embedded_signature_directory(optional: &[u8], file_length: u64) -> Result<bool, String> {
    let magic = optional.get(..2).ok_or(t("Truncated installer PE header"))?;
    let count_offset = match u16::from_le_bytes(magic.try_into().unwrap()) {
        0x10b => 92,
        0x20b => 108,
        _ => return Err(t("Unsupported installer PE header").into()),
    };
    let read = |offset| {
        optional
            .get(offset..offset + 4)
            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
            .ok_or_else(|| t("Truncated installer signature directory").to_owned())
    };
    if read(count_offset)? <= 4 {
        return Ok(false);
    }
    let security_offset = count_offset + 4 + 4 * 8;
    let offset = read(security_offset)? as u64;
    let size = read(security_offset + 4)? as u64;
    if offset == 0 && size == 0 {
        return Ok(false);
    }
    if offset == 0 || size < 8 || offset.checked_add(size).is_none_or(|end| end > file_length) {
        return Err(t("Invalid installer embedded signature bounds").into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The volumes' lengths, verified with official HTTPS HEAD responses on 2026-10-02.
    const BASELINE_LENGTHS: [u64; 5] = [1_610_626_088, 1_610_612_736, 1_610_612_736, 1_610_612_736, 1_291_746_416];

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-installer-{label}-{}-{}",
            std::process::id(), crate::backend::unique_id().unwrap()));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn a_completed_partial_is_flushed_published_and_tracked_without_downloading() {
        let dir = fixture("complete-part");
        let target = dir.join(PARTS[0]);
        let partial = dir.join(format!("{}.part", PARTS[0]));
        let sidecar = dir.join(format!("{}.download.json", PARTS[0]));
        fs::write(&partial, b"complete archive fixture").unwrap();
        let mut metadata = Metadata { url: format!("{BASE_URL}{}", PARTS[0]),
            length: 24, etag: Some("\"fixture\"".into()), modified: None,
            complete: false, sha256: None, format: 1 };
        save_metadata(&sidecar, &metadata, false).unwrap();
        let cancelled = Operation::new(); cancelled.cancel();
        assert!(finish_partial(&partial, &target, &sidecar, &mut metadata, &cancelled).is_err());
        assert!(partial.is_file()); assert!(!target.exists());
        finish_partial(&partial, &target, &sidecar, &mut metadata, &Operation::new()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"complete archive fixture");
        assert!(!partial.exists());
        let recorded: Metadata = serde_json::from_slice(&fs::read(sidecar).unwrap()).unwrap();
        assert!(recorded.complete);
        assert_eq!(recorded.sha256, Some(hex_digest(hash_file(&target, metadata.length, None).unwrap())));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn extraction_refuses_a_folder_locked_by_another_download() {
        let dir = fixture("download-lock");
        let lock = download_lock(&dir).unwrap();
        assert!(download_lock(&dir).is_err());
        // Refusal occurs before volume validation and before any process launch.
        assert!(extract(&dir).unwrap_err().contains("already in use"));
        drop(lock);
        assert!(download_lock(&dir).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_executable_handle_prevents_replacement_until_it_is_released() {
        let dir = fixture("executable-lock");
        let exe = dir.join("fixture.exe");
        fs::write(&exe, b"fixture, never executed").unwrap();
        let lock = lock_executable(&exe).unwrap();
        assert!(fs::write(&exe, b"changed").is_err());
        assert!(fs::remove_file(&exe).is_err());
        drop(lock);
        assert_eq!(fs::read(&exe).unwrap(), b"fixture, never executed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    #[ignore = "read-only official HTTPS installer metadata and bounded range probe"]
    async fn official_installer_metadata_and_first_bytes_are_reachable() {
        let client = client().unwrap();
        for name in PARTS {
            let url = format!("{BASE_URL}{name}");
            let head = client.head(&url).header(header::ACCEPT_ENCODING, "identity").send().await
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(head.status(), StatusCode::OK);
            assert!(official_url(head.url()));
            identity_encoding(head.headers()).unwrap();
            assert!(length_header(head.headers()).unwrap() > 0);
        }
        let mut response = client.get(format!("{BASE_URL}{}", PARTS[0]))
            .header(header::ACCEPT_ENCODING, "identity").header(header::RANGE, "bytes=0-4095")
            .send().await.unwrap_or_else(|error| panic!("installer range: {error:?}"));
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(official_url(response.url()));
        assert_eq!(length_header(response.headers()).unwrap(), 4096);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            assert!(bytes.len() + chunk.len() <= 4096);
            bytes.extend_from_slice(&chunk);
        }
        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[..2], b"MZ");
        eprintln!("All five official installer metadata requests and a 4096-byte executable transfer succeeded.");
    }

    #[test]
    fn setup_accepts_the_extracted_folder_or_its_parent_without_relying_on_its_name() {
        let root = std::env::temp_dir().join(format!("crystal-setup-{}-{}", std::process::id(), crate::backend::unique_id().unwrap()));
        let extracted = root.join("FFXIFullSetup_US");
        fs::create_dir_all(&extracted).unwrap();
        let setup = extracted.join("FFXISetup.exe");
        fs::write(&setup, b"fixture, never executed").unwrap();
        assert_eq!(setup_executable(&root).unwrap(), fs::canonicalize(&setup).unwrap());
        assert_eq!(setup_executable(&extracted).unwrap(), fs::canonicalize(&setup).unwrap());
        let renamed = root.join("My extracted installer");
        fs::rename(&extracted, &renamed).unwrap();
        assert_eq!(setup_executable(&renamed).unwrap(), fs::canonicalize(renamed.join("FFXISetup.exe")).unwrap());
        fs::write(renamed.join("FFXISetup.exe"), []).unwrap();
        assert!(setup_executable(&renamed).unwrap_err().contains("not found or is empty"));
        assert!(setup_executable(&root.join("missing")).unwrap_err().contains("does not exist"));
        fs::remove_dir_all(root).unwrap();
    }
    fn headers(length: u64, range: Option<&str>) -> header::HeaderMap {
        let mut h = header::HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
        if let Some(range) = range {
            h.insert(header::CONTENT_RANGE, range.parse().unwrap());
        }
        h
    }
    #[test]
    fn resumes_only_exact_remaining_range() {
        assert_eq!(
            response_offset(
                StatusCode::PARTIAL_CONTENT,
                &headers(60, Some("bytes 40-99/100")),
                40,
                100
            ),
            Ok(40)
        );
        for range in [
            "bytes 0-59/100",
            "bytes 40-98/100",
            "bytes 40-99/101",
            "bytes */100",
            "bytes 40-100/100",
        ] {
            assert!(
                response_offset(
                    StatusCode::PARTIAL_CONTENT,
                    &headers(60, Some(range)),
                    40,
                    100
                )
                .is_err(),
                "{range}"
            );
        }
        assert!(response_offset(
            StatusCode::PARTIAL_CONTENT,
            &headers(61, Some("bytes 40-99/100")),
            40,
            100
        )
        .is_err());
    }
    #[test]
    fn ignored_range_restarts_and_errors_never_append() {
        assert_eq!(
            response_offset(StatusCode::OK, &headers(100, None), 40, 100),
            Ok(0)
        );
        assert!(response_offset(StatusCode::OK, &headers(60, None), 40, 100).is_err());
        assert!(response_offset(
            StatusCode::RANGE_NOT_SATISFIABLE,
            &headers(0, None),
            100,
            100
        )
        .is_err());
    }
    #[test]
    fn resume_metadata_requires_identity_validator() {
        let current = Metadata {
            url: format!("{BASE_URL}{}", PARTS[0]),
            length: BASELINE_LENGTHS[0],
            etag: Some("\"one\"".into()),
            modified: None,
            complete: false,
            sha256: None,
            format: 1,
        };
        assert!(current.can_resume(&current));
        let mut other = current.clone();
        other.etag = Some("\"two\"".into());
        assert!(!other.can_resume(&current));
        other = current.clone();
        other.length += 1;
        assert!(!other.can_resume(&current));
        other = current.clone();
        other.url.push_str("other");
        assert!(!other.can_resume(&current));
        other = current.clone();
        other.etag = Some("W/\"one\"".into());
        assert!(!other.can_resume(&other));
        other.modified = Some("Fri, 10 May 2019 05:58:40 GMT".into());
        assert!(other.can_resume(&other));
    }
    #[test]
    fn only_official_https_without_credentials() {
        assert!(official_url(
            &Url::parse(&format!("{BASE_URL}{}", PARTS[0])).unwrap()
        ));
        for url in [
            "http://gdl.square-enix.com/x",
            "https://gdl.square-enix.com.evil.test/x",
            "https://user@gdl.square-enix.com/x",
            "https://gdl.square-enix.com:444/x",
            "https://example.org/x",
        ] {
            assert!(!official_url(&Url::parse(url).unwrap()));
        }
    }
    #[test]
    fn missing_volumes_and_relative_paths_are_rejected() {
        assert!(safe_directory(Path::new("relative-folder")).is_err());
        assert!(validate_volumes(
            &std::env::temp_dir().join("missing-official-installer-test"),
            None
        )
        .is_err());
        assert_eq!(BASELINE_LENGTHS.iter().sum::<u64>(), 7_734_210_712);
    }

    #[test]
    fn downloaded_volume_hash_rejects_same_size_corruption_and_untracked_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("xi-installer-hash-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        for name in PARTS {
            let path = dir.join(name);
            fs::write(&path, b"abc").unwrap();
            let metadata = Metadata {
                url: format!("{BASE_URL}{name}"),
                length: 3,
                etag: Some("\"test\"".into()),
                modified: None,
                complete: false,
                sha256: Some(hex_digest(hash_file(&path, 3, None).unwrap())),
                format: 1,
            };
            save_metadata(&dir.join(format!("{name}.download.json")), &metadata, true).unwrap();
        }
        assert!(validate_volumes(&dir, None).is_ok());
        fs::write(dir.join(PARTS[0]), b"xyz").unwrap();
        assert!(validate_volumes(&dir, None)
            .unwrap_err()
            .contains("changed after"));
        fs::remove_file(dir.join(format!("{}.download.json", PARTS[0]))).unwrap();
        assert!(validate_volumes(&dir, None)
            .unwrap_err()
            .contains("no official-download metadata"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The volume writer keeps every byte it was given, in order, when the sender stops early
    /// (an interrupted or cancelled download), so a resume starts from an exact offset.
    #[tokio::test]
    async fn the_volume_writer_flushes_everything_it_was_sent() {
        let dir = fixture("writer");
        let path = dir.join("volume.part");
        fs::write(&path, b"head-").unwrap();
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        let mut start = Sha256::new();
        start.update(b"head-");
        let (sender, receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let writer = spawn_writer(file, start, receiver);
        for piece in [&b"one-"[..], b"two-", b"three"] {
            sender.send(piece.to_vec()).await.unwrap();
        }
        drop(sender);
        let (_file, hash) = writer.await.unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"head-one-two-three");
        assert_eq!(hex_digest(hash), hex_digest(hash_file(&path, 18, None).unwrap()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn distinguishes_unsigned_from_malformed_embedded_signature() {
        for (magic, count) in [(0x10bu16, 92), (0x20bu16, 108)] {
            let mut header = vec![0u8; 240];
            header[..2].copy_from_slice(&magic.to_le_bytes());
            header[count..count + 4].copy_from_slice(&16u32.to_le_bytes());
            assert_eq!(embedded_signature_directory(&header, 1024), Ok(false));
            let offset = count + 4 + 4 * 8;
            header[offset..offset + 4].copy_from_slice(&900u32.to_le_bytes());
            header[offset + 4..offset + 8].copy_from_slice(&100u32.to_le_bytes());
            assert_eq!(embedded_signature_directory(&header, 1024), Ok(true));
            assert!(embedded_signature_directory(&header, 999).is_err());
            header[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
            assert!(embedded_signature_directory(&header, 1024).is_err());
        }
        assert!(embedded_signature_directory(&[], 0).is_err());
    }
}
