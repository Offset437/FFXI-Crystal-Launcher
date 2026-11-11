//! Opening links and files in the user's default programs: only vetted targets.

use crate::i18n::t;
use crate::{backend, launch_log, win, DISCORD_INVITE};

pub(crate) fn allowed_link(url: &str) -> bool {
    // The one Discord link Crystal opens: its own invite, and no other page of that site.
    if url == DISCORD_INVITE { return true; }
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "www.playonline.com"
                    | "playonline.com"
                    | "sqex.to"
                    | "www.ffxiah.com"
                    | "ffxiah.com"
                    | "www.bg-wiki.com"
                    | "bg-wiki.com"
                    | "wiki.ffo.jp"
                    | "support.na.square-enix.com"
                    | "reshade.me"
            )
        )
}
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    if !allowed_link(url) {
        return Err(t("This link is not an allowed FFXI website.").into());
    }
    shell_open(url).map_err(|_| t("Windows could not open the website.").into())
}
/// Open a client log, or the logs folder itself, in the default app. The target must resolve to
/// somewhere inside `logs_dir` (the account's own logs folder) and a file must be a `.log`.
pub(crate) fn open_log_path(logs_dir: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    let resolved = launch_log::resolve_log_target(logs_dir, target)?;
    // canonicalize yields \\?\C:\..., which ShellExecute handles but Notepad titles show verbatim.
    let plain = backend::plain_windows_path(&resolved);
    shell_open(&plain.to_string_lossy()).map_err(|_| t("Windows could not open the log.").into())
}
/// ShellExecute "open" on a URL or path the caller has already vetted.
pub(crate) fn shell_open(target: &str) -> Result<(), ()> {
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let target = win::wide(target);
        let verb = win::wide("open");
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if result as isize <= 32 {
            return Err(());
        }
    }
    Ok(())
}
