//! Record Rust panics even when the Windows GUI process has no stderr handle.

use std::{
    backtrace::Backtrace,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    panic,
    path::Path,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

const LOG_NAME: &str = "crystal-launcher-panic.log";

pub(crate) fn install(data_root: &Path) {
    let path = data_root.join("iced-launcher").join(LOG_NAME);
    let fallback = std::env::temp_dir().join(LOG_NAME);
    // A null module name identifies this executable. The base lets us resolve raw PCs
    // against the matching symbols even if the runtime cannot load the PDB.
    let image_base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Ok(mut file) = open_log(&path).or_else(|_| open_log(&fallback)) {
            let current_thread = std::thread::current();
            let thread = current_thread.name().unwrap_or("unnamed");
            let pid = std::process::id();
            let message = info.payload().downcast_ref::<&str>().copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            let message: String = message.chars().take(2048).collect();
            let mut entry = format!(
                "\n{} | Crystal Launcher {} | PID {pid} | thread {thread:?} | image base {image_base:#x}\n",
                chrono::Utc::now().to_rfc3339(),
                env!("CARGO_PKG_VERSION"),
            );
            if let Some(location) = info.location() {
                let _ = writeln!(entry, "Location: {location}");
            }
            let _ = writeln!(entry, "Panic: {message:?}");
            // Write the panic itself before calling into the platform's stack unwinder.
            let _ = file.write_all(entry.as_bytes());
            let trace = Backtrace::force_capture();
            let _ = file.write_all(format!("Backtrace for PID {pid}, thread {thread:?}:\n{trace}\n").as_bytes());
        }
        default_hook(info);
    }));
}

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}
