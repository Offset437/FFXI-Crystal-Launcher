//! The startup self-check: once FFXiMain is resolved, one log line names
//! every Crystal feature this client (and Windower Hook) build switches off,
//! and why, and the same result is saved beside the log as
//! `crystal-features.json` for the launcher's Settings page. The file is
//! rewritten only when the build pair or a result changes, so it is written
//! once per build. Nothing here reads or writes the game; it only reports
//! the resolutions game_layout and hook_layout already made.

use crate::{game_layout, log, proto::json_string};

pub(crate) const FILE: &str = "crystal-features.json";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Feature {
    pub name: &'static str,
    /// `None`: available. `Some(reason)`: off, and why.
    pub off: Option<String>,
}

fn feature<T>(name: &'static str, group: &Result<T, String>) -> Feature {
    Feature { name, off: group.as_ref().err().cloned() }
}

/// The Hook.dll build this check covers: `(timestamp, label)`.
pub(crate) type HookBuild = Option<(u32, Option<&'static str>)>;

/// The first group that failed, labelled the way its worker labels it.
fn all(groups: &[(&str, Option<&String>)]) -> Result<(), String> {
    match groups.iter().find_map(|(label, error)| error.map(|e| (label, e))) {
        Some((label, error)) => Err(format!("{label}: {error}")),
        None => Ok(()),
    }
}

/// Every feature that depends on a scanned group, in the launcher's order.
/// A feature is available only when every group its worker needs resolved.
pub(crate) fn game_features(r: &game_layout::Resolution) -> Vec<Feature> {
    let menus = ("login menus", r.menus.as_ref().err());
    let window = ("login menus (game window)", r.menus.as_ref().err());
    let input = ("key input", r.input.as_ref().err());
    // auto_login::prepare's order: lobby, characters, menus, input, world flag.
    let auto_login = all(&[("lobby", r.lobby.as_ref().err()), ("lobby characters", r.characters.as_ref().err()),
        menus, input, ("world state", r.shutdown.as_ref().err())]);
    let shutdown = all(&[window, input, ("shutdown command", r.shutdown.as_ref().err())]);
    let controller = all(&[("controller code", r.controller.as_ref().err()), window, input]);
    vec![
        feature("Auto-login", &auto_login),
        feature("Lobby error reporting", &r.lobby),
        feature("Lobby state diagnostics", &r.diagnostics),
        feature("Character list", &r.characters),
        feature("Friend locations", &r.friends),
        feature("Background shutdown", &shutdown),
        feature("Live controller settings", &controller),
    ]
}

#[cfg(target_arch = "x86")]
pub(crate) fn hook_features(r: &crate::hook_layout::Resolution) -> Vec<Feature> {
    vec![
        feature("Windower renderer shutdown wake-up", &r.exit),
        feature("FFXIDB shared-settings protection (Hook side)", &r.plugins),
    ]
}

/// The one log line.
pub(crate) fn summary(game: u32, hook: HookBuild, features: &[Feature]) -> String {
    let hook = match hook {
        Some((stamp, Some(label))) => format!("Hook {stamp:08X} ({label})"),
        Some((stamp, None)) => format!("Hook {stamp:08X} (unknown build)"),
        None => "no Windower Hook".into(),
    };
    let off: Vec<String> = features.iter().filter_map(|f| f.off.as_ref().map(|why| format!("{} ({why})", f.name))).collect();
    if off.is_empty() {
        format!("feature check: FFXiMain {game:08X}, {hook}: all {} features available", features.len())
    } else {
        format!("feature check: FFXiMain {game:08X}, {hook}: {} of {} features OFF: {}", off.len(), features.len(), off.join("; "))
    }
}

/// The saved result. Keys are stable for the launcher.
pub(crate) fn json(game: u32, hook: HookBuild, features: &[Feature]) -> String {
    let items: Vec<String> = features.iter().map(|f| match &f.off {
        None => format!("{{\"name\":{},\"ok\":true}}", json_string(f.name)),
        Some(why) => format!("{{\"name\":{},\"ok\":false,\"reason\":{}}}", json_string(f.name), json_string(why)),
    }).collect();
    let hook = match hook {
        Some((stamp, label)) => format!("{{\"timestamp\":\"{stamp:08X}\",\"known\":{}}}",
            label.map_or("null".to_string(), json_string)),
        None => "null".into(),
    };
    format!("{{\"v\":1,\"ffximain\":\"{game:08X}\",\"hook\":{hook},\"features\":[{}]}}\n", items.join(","))
}

/// Writes `contents` to `path` unless it already holds exactly that.
fn save(path: &std::path::Path, contents: &str) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|old| old == contents.as_bytes()) {
        return Ok(false);
    }
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(true)
}

/// This process's Hook.dll resolution, when Windower loaded one.
#[cfg(target_arch = "x86")]
fn hook() -> Option<&'static crate::hook_layout::Resolution> {
    crate::hook_layout::hook(crate::hook_layout::hook_base()?)
}

// --- PlayOnline core and host --------------------------------------------

/// Saved beside the log when the PlayOnline core is checked, before sign-in,
/// so a core that fails its check still shows in Settings with the reason.
pub(crate) const POL_FILE: &str = "crystal-pol.json";

/// The features the core check covers, in the launcher's order, plus the
/// host's 4 GB memory. A failed core check stops the host before sign-in, so
/// every core feature is off with that reason.
pub(crate) fn pol_features(result: Result<&crate::pol_core::Table, &str>, large_address_aware: bool) -> Vec<Feature> {
    use crate::pol_core::Group;
    let mut features: Vec<Feature> = [Group::SignIn, Group::Friends, Group::Mail, Group::Handles, Group::Game].iter()
        .map(|group| Feature { name: group.feature(), off: result.err().map(str::to_string) })
        .collect();
    features.push(Feature { name: "4 GB memory", off: (!large_address_aware).then(|| "the game host was built without /LARGEADDRESSAWARE".to_string()) });
    features
}

/// The core build this check covers: `(timestamp, verified label)`.
pub(crate) type PolBuild = Option<(u32, Option<&'static str>)>;

pub(crate) fn pol_summary(build: PolBuild, features: &[Feature]) -> String {
    let build = match build {
        Some((stamp, Some(label))) => format!("polcore {stamp:08X} ({label})"),
        Some((stamp, None)) => format!("polcore {stamp:08X} (unknown build)"),
        None => "polcore unreadable".into(),
    };
    let off: Vec<String> = features.iter().filter_map(|f| f.off.as_ref().map(|why| format!("{} ({why})", f.name))).collect();
    if off.is_empty() {
        format!("POL feature check: {build}: all {} features available", features.len())
    } else {
        format!("POL feature check: {build}: {} of {} features OFF: {}", off.len(), features.len(), off.join("; "))
    }
}

pub(crate) fn pol_json(build: PolBuild, features: &[Feature]) -> String {
    let items: Vec<String> = features.iter().map(|f| match &f.off {
        None => format!("{{\"name\":{},\"ok\":true}}", json_string(f.name)),
        Some(why) => format!("{{\"name\":{},\"ok\":false,\"reason\":{}}}", json_string(f.name), json_string(why)),
    }).collect();
    let (core, known) = match build {
        Some((stamp, label)) => (format!("\"{stamp:08X}\""), label.map_or("null".to_string(), json_string)),
        None => ("null".into(), "null".into()),
    };
    format!("{{\"v\":1,\"polcore\":{core},\"known\":{known},\"features\":[{}]}}\n", items.join(","))
}

/// Logs and saves the core check for this host (the game thread, once).
pub(crate) fn report_pol(layout: Option<&crate::pol_core::Layout>, result: Result<&crate::pol_core::Table, &str>) {
    let build = layout.map(|l| (l.timestamp, crate::pol_core::known(l.timestamp)));
    let features = pol_features(result, crate::pe::this_process_large_address_aware());
    log!("{}", pol_summary(build, &features));
    let Some(dir) = log::path().and_then(std::path::Path::parent) else { return };
    if let Err(error) = save(&dir.join(POL_FILE), &pol_json(build, &features)) {
        log!("note: POL feature check could not be saved: {error}");
    }
}

/// Logs and saves the check for this process (called once, after FFXiMain
/// resolved; off the caller's thread).
pub(crate) fn report(game: &'static game_layout::Resolution) {
    let mut features = game_features(game);
    #[cfg(target_arch = "x86")]
    let hook_build: HookBuild = match hook() {
        Some(r) => {
            features.extend(hook_features(r));
            Some((r.timestamp, r.known))
        }
        None => None,
    };
    #[cfg(not(target_arch = "x86"))]
    let hook_build: HookBuild = None;
    log!("{}", summary(game.timestamp, hook_build, &features));
    let Some(dir) = log::path().and_then(std::path::Path::parent) else { return };
    match save(&dir.join(FILE), &json(game.timestamp, hook_build, &features)) {
        Ok(true) => log!("feature check: saved for this build pair ({FILE})"),
        Ok(false) => {}
        Err(error) => log!("note: feature check could not be saved: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features() -> Vec<Feature> {
        vec![Feature { name: "Auto-login", off: None }, Feature { name: "Background shutdown", off: Some("shutdown.command: no match".into()) }]
    }

    #[test]
    fn the_line_names_every_feature_that_is_off_and_why() {
        let line = summary(0x6AC3_7846, Some((0x6A5B_1A74, Some("4.7.9.3"))), &features());
        assert_eq!(line, "feature check: FFXiMain 6AC37846, Hook 6A5B1A74 (4.7.9.3): 1 of 2 features OFF: Background shutdown (shutdown.command: no match)");
        let all = summary(0x7000_0000, None, &features()[..1]);
        assert_eq!(all, "feature check: FFXiMain 70000000, no Windower Hook: all 1 features available");
    }

    #[test]
    fn an_unsupported_module_switches_every_game_feature_off_with_the_reason() {
        let r = game_layout::Resolution::unsupported(0x1000_0000, "no executable .text section");
        let features = game_features(&r);
        assert!(features.len() >= 7);
        assert!(features.iter().all(|f| f.off.as_deref().is_some_and(|why| why.contains("no executable .text section"))), "{features:?}");
    }

    fn resolved() -> game_layout::Resolution {
        let k = game_layout::KNOWN[1];
        game_layout::Resolution { base: 0x1000_0000, timestamp: k.timestamp, size: k.size, known: true,
            lobby: Ok(k.lobby), diagnostics: Ok(k.diagnostics), characters: Ok(()), menus: Ok(k.menus),
            friends: Ok(k.friends), input: Ok(k.input), shutdown: Ok(k.shutdown), controller: Ok(k.controller) }
    }

    fn off<'a>(features: &'a [Feature], name: &str) -> Option<&'a str> {
        features.iter().find(|f| f.name == name).unwrap().off.as_deref()
    }

    #[test]
    fn a_missing_world_flag_switches_auto_login_off() {
        assert!(game_features(&resolved()).iter().all(|f| f.off.is_none()));
        let r = game_layout::Resolution { shutdown: Err("shutdown.caller: no match".into()), ..resolved() };
        let features = game_features(&r);
        assert_eq!(off(&features, "Auto-login"), Some("world state: shutdown.caller: no match"));
        let r = game_layout::Resolution { characters: Err("x".into()), ..resolved() };
        assert_eq!(off(&game_features(&r), "Auto-login"), Some("lobby characters: x"));
    }

    #[test]
    fn missing_menus_switch_off_every_feature_that_needs_the_game_window() {
        let r = game_layout::Resolution { menus: Err("menu.window_create: no match".into()), ..resolved() };
        let features = game_features(&r);
        assert_eq!(off(&features, "Auto-login"), Some("login menus: menu.window_create: no match"));
        for name in ["Background shutdown", "Live controller settings"] {
            assert_eq!(off(&features, name), Some("login menus (game window): menu.window_create: no match"), "{name}");
        }
        let r = game_layout::Resolution { input: Err("y".into()), ..resolved() };
        let features = game_features(&r);
        for name in ["Auto-login", "Background shutdown", "Live controller settings"] {
            assert_eq!(off(&features, name), Some("key input: y"), "{name}");
        }
    }

    #[test]
    fn the_saved_result_is_json_and_written_only_when_it_changes() {
        let text = json(0x6AC3_7846, Some((0x6A5B_1A74, None)), &features());
        let value = crate::json::parse(text.trim_end().as_bytes()).expect("valid JSON");
        assert!(value.get("ffximain").and_then(|v| v.as_jstr()).is_some_and(|v| v.eq_str("6AC37846")));
        assert!(text.contains("\"ffximain\":\"6AC37846\"") && text.contains("\"known\":null") && text.contains("\"ok\":false"));
        let dir = std::env::temp_dir().join(format!("crystal-feature-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let _ = std::fs::remove_file(&path);
        assert!(save(&path, &text).unwrap());
        assert!(!save(&path, &text).unwrap(), "same build, same result: not rewritten");
        assert!(save(&path, &json(0x6AC3_7846, None, &features())).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pol_check_names_every_feature_a_failed_core_switches_off() {
        let table = crate::pol_core::Table { rva: 0xc4180, missing: vec![], added: 0, services: 120 };
        let ok = pol_features(Ok(&table), true);
        assert!(ok.iter().all(|f| f.off.is_none()));
        assert!(ok.iter().any(|f| f.name == "4 GB memory"));
        assert_eq!(pol_summary(Some((0x6ABA_1641, Some("1.18.15f"))), &ok), format!("POL feature check: polcore 6ABA1641 (1.18.15f): all {} features available", ok.len()));
        let why = "PlayOnline core check failed: service 815 (PlayOnline sign-in) is missing from the function table.";
        let failed = pol_features(Err(why), false);
        assert!(failed.iter().filter(|f| f.name != "4 GB memory").all(|f| f.off.as_deref() == Some(why)));
        assert!(failed.iter().find(|f| f.name == "4 GB memory").unwrap().off.as_deref().unwrap().contains("LARGEADDRESSAWARE"));
        let text = pol_json(Some((0x7000_0000, None)), &failed);
        let value = crate::json::parse(text.trim_end().as_bytes()).expect("valid JSON");
        assert!(value.get("polcore").and_then(|v| v.as_jstr()).is_some_and(|v| v.eq_str("70000000")));
        assert!(text.contains("\"known\":null") && text.contains("\"ok\":false"));
        assert!(pol_json(None, &ok).contains("\"polcore\":null"));
    }

    // --- release guard -----------------------------------------------------

    fn files(dir: &std::path::Path, prefix: &str, suffix: &str) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let lower = name.to_ascii_lowercase();
                (lower.starts_with(prefix) && lower.ends_with(suffix)).then(|| (name, std::fs::read(entry.path()).unwrap()))
            })
            .collect();
        out.sort();
        out
    }

    fn row(build: &str, features: &[Feature]) -> bool {
        for f in features {
            match &f.off {
                None => eprintln!("  {build:<28} {:<48} PASS", f.name),
                Some(why) => eprintln!("  {build:<28} {:<48} FAIL  {why}", f.name),
            }
        }
        features.iter().all(|f| f.off.is_none())
    }

    /// Runs every resolver over each unpacked FFXiMain image in
    /// CRYSTAL_FFXIMAIN_DUMPS (`FFXiMain-<stamp>.img`, from an image dumper)
    /// and each Hook.dll in CRYSTAL_HOOK_BUILDS (`Hook-<stamp>.dll`), as data;
    /// with CRYSTAL_SURVEY_FFXIMAIN=<path to an FFXiMain.dll>, also a copy of
    /// that file loaded into this test process (its attach code unpacks it, as
    /// the dumper does; the game never starts). Prints a pass/fail table and
    /// fails when the newest build of either module switches anything off:
    /// run it on a new game or Windower build before a release.
    #[test]
    #[ignore = "release guard: reads CRYSTAL_FFXIMAIN_DUMPS / CRYSTAL_HOOK_BUILDS (and optionally loads a copy of CRYSTAL_SURVEY_FFXIMAIN)"]
    fn release_survey_of_every_resolver_over_the_dumps() {
        let dumps = std::env::var_os("CRYSTAL_FFXIMAIN_DUMPS").map(std::path::PathBuf::from)
            .unwrap_or_else(|| r"dumps".into());
        let mut games = Vec::new();
        eprintln!("FFXiMain (unpacked images in {}):", dumps.display());
        for (name, file) in files(&dumps, "ffximain-", ".img") {
            let image = crate::sigscan::Image::from_reader(0x1000_0000, |rva, out| {
                file.get(rva..rva + out.len()).map(|b| out.copy_from_slice(b)).is_some()
            }).unwrap();
            let r = game_layout::resolve(&image);
            games.push((r.timestamp, row(&format!("{name} {:08X}", r.timestamp), &game_features(&r))));
        }
        if let Some(path) = std::env::var_os("CRYSTAL_SURVEY_FFXIMAIN").map(std::path::PathBuf::from) {
            use windows_sys::Win32::System::LibraryLoader::LoadLibraryExW;
            let copy = std::env::temp_dir().join(format!("crystal-survey-{}", std::process::id())).join("FFXiMain.dll");
            std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::copy(&path, &copy).unwrap();
            let wide: Vec<u16> = copy.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
            assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
            let r = game_layout::resolve_module(module as usize).expect("an initialized module is never deferred");
            eprintln!("FFXiMain (loaded copy of {}):", path.display());
            games.push((r.timestamp, row(&format!("loaded {:08X}", r.timestamp), &game_features(&r))));
        }
        let builds = std::env::var_os("CRYSTAL_HOOK_BUILDS").map(std::path::PathBuf::from)
            .unwrap_or_else(|| r"builds".into());
        let mut hooks = Vec::new();
        eprintln!("Windower Hook.dll (files in {}):", builds.display());
        for (name, file) in files(&builds, "hook-", ".dll") {
            let Ok(pe) = crate::windower_bootstrap_contract::map_pe(&file) else { continue };
            let r = crate::hook_layout::tests::resolve_mapped(&pe.image, pe.image_base);
            let mut features = hook_features(&r);
            features.push(feature("Private Windower startup", &crate::windower_bootstrap_contract::hook_startup(&file).map_err(str::to_string)));
            hooks.push((r.timestamp, row(&format!("{name} {}", r.known.unwrap_or("unknown")), &features)));
        }
        // FFXIDB.dll needs no row: its protection uses only Hook's plugin
        // interface (the Hook side above) and the plugin's file imports.
        eprintln!("Ashita: no Ashita-specific resolvers (background shutdown accepts an Ashita.dll hook on the command entry).");
        // PlayOnline: each core build (CRYSTAL_POL_LEGACY_CORE / CRYSTAL_POL_UPDATED_CORE)
        // through the production structural checks, as data.
        let mut cores = Vec::new();
        eprintln!("PlayOnline core (files):");
        for build in crate::pol_core::tests::builds() {
            let Ok(file) = std::fs::read(&build.core) else { eprintln!("  {} missing: {}", build.name, build.core.display()); continue };
            let checked = crate::pol_core::tests::check_file(&file, &build.vtables);
            let stamp = crate::windower_bootstrap_contract::map_pe(&file).map_or(0, |pe| pe.timestamp);
            let features = pol_features(checked.as_ref().map(|(_, t)| t).map_err(|e| e.as_str()), true);
            cores.push((stamp, row(&format!("polcore {} {stamp:08X}", build.name), &features)));
        }
        for (what, results) in [("FFXiMain", &games), ("Hook.dll", &hooks), ("polcore", &cores)] {
            let newest = results.iter().max_by_key(|(stamp, _)| *stamp);
            assert!(newest.is_some(), "no {what} builds to survey");
            assert!(newest.unwrap().1, "the newest {what} build switches a feature off (see the table)");
        }
    }
}