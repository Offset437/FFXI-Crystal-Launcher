//! Fixing a missing component: download (with progress and cancel), verify
//! the Microsoft signature, then run the installer and map its exit code.
//! Only ever started by an explicit click on the System tab.
use super::detect::Component;
use crate::i18n::{t, tf};
use futures_util::{SinkExt, Stream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DIRECTX_URL: &str =
    "https://download.microsoft.com/download/8/4/A/84A35BF1-DAFE-4AE8-82AF-AD2AE20B6B14/directx_Jun2010_redist.exe";
const VC_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x86.exe";
/// Refuse anything implausibly large for these two installers.
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Downloading { done: u64, total: Option<u64> },
    Verifying,
    Extracting,
    Installing,
}

impl Phase {
    pub(crate) fn cancellable(&self) -> bool {
        matches!(self, Phase::Downloading { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    RestartRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Phase(Phase),
    Finished(Result<Outcome, String>),
}

/// What a fix does, shown before the user confirms it.
pub(crate) struct Plan {
    pub(crate) steps: Vec<&'static str>,
    pub(crate) command: &'static str,
    pub(crate) download: Option<(&'static str, &'static str)>,
}

pub(crate) fn plan(component: Component) -> Plan {
    match component {
        Component::DirectPlay => Plan {
            steps: vec![
                t("Turns on the DirectPlay feature with Windows' own servicing tool (DISM). Nothing is downloaded by the launcher; Windows may fetch the files from Windows Update."),
            ],
            command: "dism.exe /online /enable-feature /featurename:DirectPlay /all /norestart",
            download: None,
        },
        Component::DirectX9 => Plan {
            steps: vec![
                t("Downloads Microsoft's DirectX End-User Runtime (June 2010) from download.microsoft.com."),
                t("Checks the file is validly signed by Microsoft, unpacks it to a temporary folder and runs its setup silently."),
            ],
            command: "directx_Jun2010_redist.exe /Q /T:<temp>  then  DXSETUP.exe /silent",
            download: Some(("directx_Jun2010_redist.exe", t("about 100 MB"))),
        },
        Component::VcRedist => Plan {
            steps: vec![
                t("Downloads Microsoft's Visual C++ 2015–2022 Redistributable (x86) from aka.ms/vs/17/release."),
                t("Checks the file is validly signed by Microsoft, then runs it silently."),
            ],
            command: "vc_redist.x86.exe /install /quiet /norestart",
            download: Some(("vc_redist.x86.exe", t("about 14 MB"))),
        },
    }
}

// ---- exit codes (pure) --------------------------------------------------------

const ERROR_CANCELLED: u32 = 1223;
const ERROR_SUCCESS_REBOOT_INITIATED: u32 = 1641;
const ERROR_SUCCESS_REBOOT_REQUIRED: u32 = 3010;
const ERROR_ELEVATION_REQUIRED: u32 = 740;

fn declined() -> String {
    t("Windows' permission prompt was declined, so nothing was changed.").into()
}

pub(crate) fn dism_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED | ERROR_SUCCESS_REBOOT_INITIATED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED => Err(declined()),
        ERROR_ELEVATION_REQUIRED => Err(t("Enabling DirectPlay needs administrator rights.").into()),
        0x800F081F | 0x800F0950 | 0x800F0906 | 0x800F0907 => Err(tf(
            "Windows couldn't find the DirectPlay files ({code}). Connect to the internet so Windows Update can supply them, or enable it manually.",
            &[("code", &format!("0x{code:08X}"))],
        )),
        other => Err(tf(
            "DISM couldn't enable DirectPlay (exit code {code}).",
            &[("code", &format!("0x{other:08X}"))],
        )),
    }
}

pub(crate) fn vc_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        // A same-or-newer version is already installed.
        1638 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED | ERROR_SUCCESS_REBOOT_INITIATED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED | 1602 => Err(declined()),
        1618 => Err(t("Another installation is in progress. Wait for it to finish and try again.").into()),
        other => Err(tf("The Visual C++ installer failed (exit code {code}).", &[("code", &(other as i32))])),
    }
}

pub(crate) fn dxsetup_outcome(code: u32) -> Result<Outcome, String> {
    match code {
        0 => Ok(Outcome::Done),
        ERROR_SUCCESS_REBOOT_REQUIRED => Ok(Outcome::RestartRequired),
        ERROR_CANCELLED => Err(declined()),
        other => Err(tf("DirectX setup failed (exit code {code}).", &[("code", &(other as i32))])),
    }
}

pub(crate) fn extract_outcome(code: u32) -> Result<(), String> {
    match code {
        0 => Ok(()),
        ERROR_CANCELLED => Err(declined()),
        other => Err(tf("Couldn't unpack the DirectX runtime (exit code {code}).", &[("code", &(other as i32))])),
    }
}

/// Whether a download's final (post-redirect) host is Microsoft's.
pub(crate) fn trusted_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "microsoft.com" || host.ends_with(".microsoft.com")
}

fn trusted_download_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| host == "aka.ms" || trusted_host(host))
}

// ---- the job ----------------------------------------------------------------

/// A temporary folder removed when the job ends (or is cancelled).
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("CrystalLauncher-components-{}-{nanos}", std::process::id());
        let failed = |e: std::io::Error| tf("Couldn't create a temporary folder: {error}", &[("error", &e)]);
        if crate::win::process::is_elevated() {
            // The installers then run elevated from here, and DXSETUP loads DLLs beside itself:
            // a folder in the user's %TEMP% could be changed by any program the user runs.
            // The root comes from the system, not %ProgramData%: an elevated process inherits
            // the user's environment, which that same user-level code could point anywhere.
            return admin_scratch(&name).map(Self).map_err(failed);
        }
        // Known gap, left as is: a launcher built without `elevated-ui` (release builds always
        // have it) runs the installers through "runas" from this user-writable %TEMP% folder,
        // where user-level code could plant DLLs beside DXSETUP before it starts elevated.
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir(&dir).map_err(failed)?;
        Ok(Self(dir))
    }
}

/// A new folder `name` in ProgramData that only Administrators and SYSTEM can change, for files
/// an elevated process runs.
pub(crate) fn admin_scratch(name: &str) -> std::io::Result<PathBuf> {
    let root = program_data()?;
    if !owned_by_admins(&root) {
        return Err(std::io::Error::other(format!("{} isn't owned by Administrators or SYSTEM", root.display())));
    }
    let dir = root.join(name);
    create_admin_only_dir(&dir)?;
    Ok(dir)
}

/// The machine's ProgramData folder as the system records it (FOLDERID_ProgramData, read from
/// HKLM), never from the environment.
pub(crate) fn program_data() -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};
    let mut raw: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `raw` is an out-pointer; on return it is a NUL-terminated CoTaskMem string (or
    // null), read before being freed exactly once.
    unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut raw);
        let path = (hr >= 0 && !raw.is_null()).then(|| {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, len)))
        });
        CoTaskMemFree(raw as _);
        path.filter(|p| p.is_absolute()).ok_or_else(|| std::io::Error::other(format!("no ProgramData folder (0x{hr:08X})")))
    }
}

/// Whether `dir` is owned by Administrators or SYSTEM, so no standard user can swap what is
/// created inside it.
pub(crate) fn owned_by_admins(dir: &Path) -> bool {
    use crate::win::wide_os;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION, PSID,
    };
    let path = wide_os(dir.as_os_str());
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: NUL-terminated path; `owner` points into `descriptor`, which is LocalAlloc'd by
    // the call and freed after the last use of `owner`.
    unsafe {
        let status = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        let ok = status == ERROR_SUCCESS
            && !owner.is_null()
            && (IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0 || IsWellKnownSid(owner, WinLocalSystemSid) != 0);
        if !descriptor.is_null() {
            LocalFree(descriptor as _);
        }
        ok
    }
}

/// Whether `dir`'s DACL grants no write, append, delete, delete-child, write-DAC or take-ownership
/// right to Users, Authenticated Users, Everyone, INTERACTIVE or the current user, so none of them
/// can swap a file inside. An unreadable or empty (null) DACL counts as not safe. Deny entries are
/// ignored: any such allow entry fails the check.
pub(crate) fn no_standard_user_writes(dir: &Path) -> bool {
    use crate::win::wide_os;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetTokenInformation, IsWellKnownSid, TokenUser,
        WinAuthenticatedUserSid, WinBuiltinUsersSid, WinInteractiveSid, WinWorldSid, ACCESS_ALLOWED_ACE, ACL,
        ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER
    // | GENERIC_WRITE | GENERIC_ALL
    const WRITES: u32 = 0x2 | 0x4 | 0x40 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x4000_0000 | 0x1000_0000;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const GROUPS: [WELL_KNOWN_SID_TYPE; 4] =
        [WinBuiltinUsersSid, WinAuthenticatedUserSid, WinWorldSid, WinInteractiveSid];

    let path = wide_os(dir.as_os_str());
    // SAFETY: NUL-terminated path; `dacl` and every SID read below point into `descriptor` (freed
    // after the last use) or into `user_buf`, both alive for the whole walk.
    unsafe {
        // The current user's SID, in a buffer that outlives the walk.
        let mut user_buf: Vec<u64> = Vec::new();
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 {
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            if len > 0 {
                user_buf = vec![0u64; (len as usize).div_ceil(8)];
                if GetTokenInformation(token, TokenUser, user_buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                    user_buf.clear();
                }
            }
            CloseHandle(token);
        }
        if user_buf.is_empty() {
            return false;
        }
        let user_sid: PSID = (*user_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;

        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        let mut safe = status == ERROR_SUCCESS && !dacl.is_null();
        if safe {
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            safe = GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ) != 0;
            if safe {
                for index in 0..info.AceCount {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, index, &mut ace) == 0 {
                        safe = false;
                        break;
                    }
                    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                    if (*ace).Header.AceType != ACCESS_ALLOWED_ACE_TYPE || (*ace).Mask & WRITES == 0 {
                        continue;
                    }
                    let sid: PSID = std::ptr::addr_of_mut!((*ace).SidStart).cast();
                    if EqualSid(sid, user_sid) != 0 || GROUPS.iter().any(|&group| IsWellKnownSid(sid, group) != 0) {
                        safe = false;
                        break;
                    }
                }
            }
        }
        if !descriptor.is_null() {
            LocalFree(descriptor as _);
        }
        safe
    }
}

/// A new folder (never an existing one) that only Administrators and SYSTEM can change; what
/// is created inside inherits that. The DACL is protected, so nothing from the parent applies.
fn create_admin_only_dir(dir: &Path) -> std::io::Result<()> {
    create_dir_with_dacl(dir, ADMIN_ONLY)
}

/// Full control for Administrators and SYSTEM, inherited by everything inside; nothing else.
const ADMIN_ONLY: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";

fn create_dir_with_dacl(dir: &Path, sddl: &str) -> std::io::Result<()> {
    use crate::win::{wide, wide_os};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let sddl = wide(sddl);
    let path = wide_os(dir.as_os_str());
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: NUL-terminated strings and out-pointers live through the calls; the descriptor
    // LocalAlloc'd by the conversion is freed once CreateDirectoryW has copied it.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let created = CreateDirectoryW(path.as_ptr(), &attributes);
        let error = std::io::Error::last_os_error();
        LocalFree(descriptor as _);
        if created == 0 { Err(error) } else { Ok(()) }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Out = futures_channel::mpsc::Sender<Event>;

/// The event stream for one fix. Dropping it (Cancel) stops a download and
/// removes the partial file; an installer that has started is left to finish.
pub(crate) fn job(component: Component) -> impl Stream<Item = Event> {
    crate::runtime::channel(8, async move |mut out: Out| {
        let result = run(component, &mut out).await;
        let _ = out.send(Event::Finished(result)).await;
    })
}

async fn run(component: Component, out: &mut Out) -> Result<Outcome, String> {
    match component {
        Component::DirectPlay => {
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(|| {
                let dism = crate::win::system32().join("dism.exe");
                run_hidden(&dism, "/online /enable-feature /featurename:DirectPlay /all /norestart", None)
                    .and_then(dism_outcome)
            })
            .await
        }
        Component::VcRedist => {
            let (scratch, exe, lock) = fetch_verified(VC_URL, "vc_redist.x86.exe", out).await?;
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(move || {
                // The folder and the write-deny handle stay until the installer has run.
                let (_scratch, _lock) = (scratch, lock);
                run_hidden(&exe, "/install /quiet /norestart", None).and_then(vc_outcome)
            })
            .await
        }
        Component::DirectX9 => {
            let (scratch, exe, lock) = fetch_verified(DIRECTX_URL, "directx_Jun2010_redist.exe", out).await?;
            let _ = out.send(Event::Phase(Phase::Extracting)).await;
            let (scratch, setup) = blocking(move || {
                let _lock = lock;
                let target = scratch.0.join("dx");
                std::fs::create_dir(&target).map_err(|e| tf("Couldn't create a temporary folder: {error}", &[("error", &e)]))?;
                run_hidden(&exe, &format!("/Q /T:\"{}\"", target.display()), None).and_then(extract_outcome)?;
                let setup = target.join("DXSETUP.exe");
                let setup_lock = verified(&setup)?;
                Ok((scratch, (setup, setup_lock, target)))
            })
            .await?;
            let _ = out.send(Event::Phase(Phase::Installing)).await;
            blocking(move || {
                let _scratch = scratch;
                let (setup, _lock, target) = setup;
                run_hidden(&setup, "/silent", Some(&target)).and_then(dxsetup_outcome)
            })
            .await
        }
    }
}

/// Download `url` into a new scratch folder as `name`, then open it write-denied and check
/// its Microsoft signature. Keep all three alive until the file has run.
async fn fetch_verified(url: &str, name: &str, out: &mut Out) -> Result<(Scratch, PathBuf, std::fs::File), String> {
    let scratch = Scratch::new()?;
    let exe = scratch.0.join(name);
    download(url, &exe, out).await?;
    let _ = out.send(Event::Phase(Phase::Verifying)).await;
    blocking(move || {
        let lock = verified(&exe)?;
        Ok((scratch, exe, lock))
    })
    .await
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Err(t("The installer worker stopped unexpectedly.").into()))
}

/// Opens `path` denying writes and deletes, then checks its signature. The
/// returned handle must stay open until the file has run.
fn verified(path: &Path) -> Result<std::fs::File, String> {
    let file = open_locked(path)?;
    super::trust::verify_microsoft(path, &file)?;
    Ok(file)
}

fn open_locked(path: &Path) -> Result<std::fs::File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
        .open(path)
        .map_err(|e| tf("Couldn't open {path}: {error}", &[("path", &path.display()), ("error", &e)]))
}

async fn download(url: &str, dest: &Path, out: &mut Out) -> Result<(), String> {
    use std::io::Write;
    let _ = out.send(Event::Phase(Phase::Downloading { done: 0, total: None })).await;
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        // Includes the response headers: connect_timeout alone lets a server
        // accept the connection and then leave the job waiting indefinitely.
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error(t("Too many prerequisite download redirects."))
            } else if trusted_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error(t("The download left Microsoft's HTTPS servers, so it was stopped."))
            }
        }))
        .user_agent("CrystalLauncher/0.1")
        .build()
        .map_err(|e| tf("Couldn't start the download: {error}", &[("error", &e)]))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| t("Couldn't reach Microsoft's download server. Check your internet connection.").to_owned())?;
    if !response.status().is_success() {
        return Err(tf("Microsoft's download server answered {status}.", &[("status", &response.status())]));
    }
    if !trusted_download_url(response.url()) || !response.url().host_str().is_some_and(trusted_host) {
        return Err(t("The download was redirected away from microsoft.com, so it was stopped.").into());
    }
    let total = response.content_length();
    if total.is_some_and(|size| size > MAX_DOWNLOAD) {
        return Err(t("The download is larger than expected, so it was stopped.").into());
    }
    let mut file = std::fs::File::create(dest).map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), response.chunk())
            .await
            .map_err(|_| t("The download stalled. Check your internet connection and try again.").to_owned())?
            .map_err(|_| t("The download was interrupted. Check your internet connection and try again.").to_owned())?;
        let Some(chunk) = chunk else { break };
        done += chunk.len() as u64;
        if done > MAX_DOWNLOAD {
            return Err(t("The download is larger than expected, so it was stopped.").into());
        }
        file.write_all(&chunk).map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
        if last.elapsed() >= Duration::from_millis(120) {
            last = Instant::now();
            let _ = out.send(Event::Phase(Phase::Downloading { done, total })).await;
        }
    }
    if total.is_some_and(|size| size != done) {
        return Err(t("The download ended early. Try again.").into());
    }
    file.sync_all().map_err(|e| tf("Couldn't save the download: {error}", &[("error", &e)]))?;
    let _ = out.send(Event::Phase(Phase::Downloading { done, total: Some(done) })).await;
    Ok(())
}

/// Wait for `process` to end and return its exit code; `None` when it could not be tracked.
pub(crate) fn wait_exit_code(process: std::os::windows::io::OwnedHandle) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    let handle = process.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut code = 0u32;
    // SAFETY: a live process handle owned by `process` for both calls.
    let (waited, ok) = unsafe { (WaitForSingleObject(handle, INFINITE), GetExitCodeProcess(handle, &mut code)) };
    (waited == WAIT_OBJECT_0 && ok != 0).then_some(code)
}

/// Runs `exe args` without a console window and returns its exit code. An
/// elevated launcher starts it directly; otherwise Windows asks for
/// permission (the "runas" verb) first.
pub(crate) fn run_hidden(exe: &Path, args: &str, dir: Option<&Path>) -> Result<u32, String> {
    use crate::win::{shell_execute, ShellError};
    if crate::win::process::is_elevated() {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(exe);
        command.raw_arg(args).creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let status = command
            .status()
            .map_err(|e| tf("Couldn't start {path}: {error}", &[("path", &exe.display()), ("error", &e)]))?;
        return Ok(status.code().unwrap_or(-1) as u32);
    }
    let process = match shell_execute("runas", exe, Some(args), dir, windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE, true) {
        Ok(process) => process,
        Err(ShellError::Failed(ERROR_CANCELLED)) => return Ok(ERROR_CANCELLED),
        Err(ShellError::Failed(error)) => {
            return Err(tf("Couldn't start {path} (error {error}).", &[("path", &exe.display()), ("error", &error)]))
        }
        Err(ShellError::NoProcess) => return Err(tf("Couldn't track {path}.", &[("path", &exe.display())])),
    };
    wait_exit_code(process).ok_or_else(|| tf("Lost track of {path}.", &[("path", &exe.display())]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(dism_outcome(0), Ok(Outcome::Done));
        assert_eq!(dism_outcome(3010), Ok(Outcome::RestartRequired));
        assert!(dism_outcome(1223).unwrap_err().contains("declined"));
        assert!(dism_outcome(0x800F081F).unwrap_err().contains("0x800F081F"));
        assert!(dism_outcome(87).is_err());

        assert_eq!(vc_outcome(0), Ok(Outcome::Done));
        assert_eq!(vc_outcome(1638), Ok(Outcome::Done));
        assert_eq!(vc_outcome(3010), Ok(Outcome::RestartRequired));
        assert_eq!(vc_outcome(1641), Ok(Outcome::RestartRequired));
        assert!(vc_outcome(1618).unwrap_err().contains("in progress"));
        assert!(vc_outcome(1602).unwrap_err().contains("declined"));

        assert_eq!(dxsetup_outcome(0), Ok(Outcome::Done));
        assert!(dxsetup_outcome((-9i32) as u32).unwrap_err().contains("-9"));
        assert_eq!(extract_outcome(0), Ok(()));
        assert!(extract_outcome(5).is_err());
    }

    #[test]
    fn folder_write_rights() {
        // A fresh temp folder is writable by the current user; the Windows folder is not.
        let dir = std::env::temp_dir().join(format!("crystal-acl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let user_writable = no_standard_user_writes(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!user_writable);
        if let Some(windows) = std::env::var_os("SystemRoot") {
            assert!(no_standard_user_writes(Path::new(&windows)));
        }
        assert!(!no_standard_user_writes(Path::new("C:\\no\\such\\folder")));
    }

    #[test]
    fn download_hosts() {
        assert!(trusted_host("download.microsoft.com"));
        assert!(trusted_host("download.visualstudio.microsoft.com"));
        assert!(trusted_host("Microsoft.com"));
        assert!(!trusted_host("microsoft.com.evil.example"));
        assert!(!trusted_host("notmicrosoft.com"));
        assert!(!trusted_host("aka.ms"));
        for url in [DIRECTX_URL, VC_URL, "https://download.visualstudio.microsoft.com/file"] {
            assert!(trusted_download_url(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
        for url in ["http://download.microsoft.com/file", "https://microsoft.com.evil.example/file",
            "https://user@download.microsoft.com/file", "https://download.microsoft.com:444/file",
            "https://aka.ms.evil.example/file"] {
            assert!(!trusted_download_url(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn plans_describe_commands() {
        assert!(plan(Component::DirectPlay).command.contains("/featurename:DirectPlay"));
        assert!(plan(Component::DirectPlay).download.is_none());
        assert!(plan(Component::VcRedist).command.contains("/quiet"));
        assert!(plan(Component::DirectX9).command.contains("/silent"));
    }

    #[test]
    fn scratch_is_removed() {
        let scratch = Scratch::new().unwrap();
        let path = scratch.0.clone();
        std::fs::write(path.join("x"), b"1").unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    /// Elevated installers run from this folder: an unelevated process (the test runner here)
    /// must not be able to add or replace anything in it.
    #[test]
    fn admin_only_folder_refuses_unelevated_writes() {
        let dir = std::env::temp_dir().join(format!("crystal-admin-only-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        // The production DACL plus, on the folder itself only, what this test needs to remove it
        // again (DELETE, SYNCHRONIZE, FILE_READ_ATTRIBUTES): nothing that adds a file.
        create_dir_with_dacl(&dir, &format!("{ADMIN_ONLY}(A;;0x110080;;;WD)")).unwrap();
        assert!(create_admin_only_dir(&dir).is_err(), "an existing folder is never reused");
        let written = std::fs::write(dir.join("DSETUP.dll"), b"planted");
        if !crate::win::process::is_elevated() {
            assert_eq!(written.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
        let _ = std::fs::remove_file(dir.join("DSETUP.dll"));
        std::fs::remove_dir(&dir).unwrap();
    }

    /// Downloads both real installers to a temp folder and verifies their
    /// signatures. Never runs them. `cargo test prereqs -- --ignored`.
    #[test]
    #[ignore]
    fn live_download_and_verify() {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            for (url, name) in [(VC_URL, "vc_redist.x86.exe"), (DIRECTX_URL, "directx_Jun2010_redist.exe")] {
                let scratch = Scratch::new().unwrap();
                let exe = scratch.0.join(name);
                let (mut tx, _rx) = futures_channel::mpsc::channel(1024);
                download(url, &exe, &mut tx).await.unwrap();
                let size = std::fs::metadata(&exe).unwrap().len();
                let lock = verified(&exe);
                println!("{name}: {size} bytes, {:?}", lock.as_ref().map(|_| "signed by Microsoft"));
                assert!(lock.is_ok(), "{lock:?}");
            }
        });
    }
}
