//! FINAL FANTASY XI game options: what Square Enix's "FINAL FANTASY XI Config"
//! tool edits, built into the Settings page.
//!
//! Values are read when the launcher starts, edited in memory, and written
//! only on Apply - after the values being replaced are backed up. See
//! `store` for where each launch target reads them.
mod display;
/// The tabs as plain data for the Slint front end.
pub(crate) mod front;
mod model;
mod store;

use crate::i18n::{t, tf};
use crate::effect::Effect;
use model::{Data, Values};
use std::path::PathBuf;
use store::{BackupEntry, Paths, Region};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Graphics,
    Sound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

pub(crate) struct State {
    preview: bool,
    loaded: bool,
    paths: Paths,
    region: Region,
    /// Regions whose registry key holds game settings.
    regions: Vec<Region>,
    writable: bool,
    /// As stored, when last read.
    original: Values,
    /// As edited; equal to `original` when nothing is pending.
    edits: Values,
    menu_selected: bool,
    menu_retained: bool,
    /// What an entry missing from storage shows as.
    fallback: Values,
    modes: Vec<(u32, u32)>,
    desktop: (u32, u32),
    backups: Vec<BackupEntry>,
    /// Regions backed up this session (a backup precedes the first write).
    backed_up: Vec<Region>,
    notice: Option<(Tone, String)>,
    /// Bumped by every update: a front end rebuilds the page only when it moved.
    revision: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            preview: crate::preview::flags().is_some(),
            loaded: false,
            paths: Paths::from_config(&crate::backend::Config::default()),
            region: Region::Us,
            regions: Vec::new(),
            writable: true,
            original: Values::new(),
            edits: Values::new(),
            menu_selected: false,
            menu_retained: false,
            fallback: model::defaults((1920, 1080)),
            modes: Vec::new(),
            desktop: (1920, 1080),
            backups: Vec::new(),
            backed_up: Vec::new(),
            notice: None,
            revision: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Load,
    Region(Region),
    Set(&'static str, u32),
    Window(u32, u32),
    Menu(u32, u32),
    Background(u32, u32),
    Gamma(f32),
    ScreenshotDir(String),
    Apply,
    Discard,
    Defaults,
    LoadBackup(BackupEntry),
    DismissNotice,
}

impl State {
    /// Moves with every update (`front` reads nothing else that changes).
    pub(crate) fn revision(&self) -> u64 { self.revision }

    fn pending(&self) -> Vec<&'static str> {
        let mut keys = model::changed_keys(&self.original, &self.edits);
        if self.menu_selected {
            for key in [model::MENU_W, model::MENU_H] {
                if !keys.contains(&key) { keys.push(key); }
            }
        }
        keys
    }
    fn get(&self, key: &str) -> Option<&Data> {
        self.edits.get(key).or_else(|| self.fallback.get(key))
    }
    fn dword(&self, key: &str) -> u32 {
        match self.get(key) {
            Some(Data::Dword(v)) => *v,
            _ => 0,
        }
    }
    /// The stored-as-automatic (0, 0) menu pair, as edited.
    fn automatic_menu(&self) -> bool {
        (self.dword(model::MENU_W), self.dword(model::MENU_H)) == (0, 0)
    }
    fn menu_size(&self) -> (u32, u32) {
        let size = (self.dword(model::MENU_W), self.dword(model::MENU_H));
        if size == (0, 0) {
            (self.dword(model::WINDOW_W), self.dword(model::WINDOW_H))
        } else {
            size
        }
    }
    fn text(&self, key: &str) -> &str {
        match self.get(key) {
            Some(Data::Text(v)) => v,
            _ => "",
        }
    }
    fn changed(&self, keys: &[&str]) -> bool {
        keys.iter().any(|k| self.original.get(*k) != self.edits.get(*k)
            || self.menu_selected && matches!(*k, model::MENU_W | model::MENU_H))
    }
    /// Sets an entry; one missing from storage stays missing while it shows
    /// its fallback, so merely looking never makes a change pending.
    fn set(&mut self, key: &str, data: Data) {
        if !self.original.contains_key(key) &&self.fallback.get(key) == Some(&data) {
            self.edits.remove(key);
        } else {
            self.edits.insert(key.into(), data);
        }
    }
    fn set_dword(&mut self, key: &str, value: u32) {
        self.set(key, Data::Dword(value));
    }

    fn load(&mut self) {
        if self.preview {
            self.load_preview();
            return;
        }
        let (modes, current) = display::modes();
        self.desktop = current.or_else(|| modes.first().copied()).unwrap_or((1920, 1080));
        self.modes = modes;
        self.fallback = model::defaults(self.desktop);
        self.regions = store::retail_regions();
        if !self.regions.contains(&self.region) {
            self.region = self.regions.first().copied().unwrap_or(Region::Us);
        }
        self.read_target();
    }

    fn read_target(&mut self) {
        self.loaded = true;
        self.menu_selected = false;
        self.menu_retained = store::menu_override(&self.paths, self.region).ok().flatten().is_some();
        self.writable = store::writable(self.region);
        self.backups = store::list_backups(&self.paths.backups(), self.region);
        match store::read(&self.paths, self.region) {
            Ok(values) => {
                self.original = values;
                self.edits = self.original.clone();
            }
            Err(e) => {
                self.original = Values::new();
                self.edits = Values::new();
                self.notice = Some((Tone::Error, e));
            }
        }
    }

    /// Realistic synthetic values; nothing on the machine is read.
    fn load_preview(&mut self) {
        let (modes, current) = display::synthetic();
        self.modes = modes;
        self.desktop = current.unwrap_or((3840, 2160));
        self.fallback = model::defaults(self.desktop);
        self.regions = vec![Region::Us];
        self.loaded = true;
        self.writable = true;
        let mut v = self.fallback.clone();
        for (k, x) in [
            (model::WINDOW_W, 2560),
            (model::WINDOW_H, 1440),
            (model::MENU_W, 1708),
            (model::MENU_H, 960),
            (model::WINDOW_MODE, 3),
            (model::TEXTURE_COMPRESSION, 2),
            (model::ENV_ANIMATION, 1),
            (model::MAX_SOUNDS, 16),
            (model::FONT_TYPE, 0),
            (model::OPENING_MOVIE, 0),
            (model::SCREENSHOT_SCREEN_RES, 1),
            (model::GAMMA, model::gamma_bits(0.1)),
        ] {
            v.insert(k.into(), Data::Dword(x));
        }
        v.insert(model::SCREENSHOT_DIR.into(), Data::Text("D:\\Pictures\\Vana'diel".into()));
        if crate::preview::has("--preview-menu-auto") {
            v.insert(model::MENU_W.into(), Data::Dword(0));
            v.insert(model::MENU_H.into(), Data::Dword(0));
        }
        v.insert("0031".into(), Data::Dword(0x3bc4_9ba6));
        self.original = v;
        self.edits = self.original.clone();
        // Two pending edits, so the preview shows the unsaved-changes state.
        self.set_dword(model::ENV_ANIMATION, 2);
        self.set_dword(model::MAX_SOUNDS, 20);
        self.backups = vec![
            BackupEntry {
                path: PathBuf::from("preview-1"),
                label: "Oct 2, 2026  21:14".into(),
            },
            BackupEntry {
                path: PathBuf::from("preview-2"),
                label: "Sep 28, 2026  09:40".into(),
            },
        ];
    }

    fn apply(&mut self) {
        let pending = self.pending();
        if pending.is_empty() {
            return;
        }
        if self.preview {
            self.notice = Some((Tone::Info, t("Design preview: nothing was written.").into()));
            return;
        }
        if let Err(e) = model::validate(&self.edits) {
            self.notice = Some((Tone::Error, e));
            return;
        }
        // Re-read: something else (the game, the official Config tool) may
        // have changed the values since they were loaded.
        let current = match store::read(&self.paths, self.region) {
            Ok(v) => v,
            Err(e) => {
                self.notice = Some((Tone::Error, e));
                return;
            }
        };
        if !model::changed_keys(&self.original, &current).is_empty() {
            self.rebase(current);
            self.menu_retained = store::menu_override(&self.paths, self.region).ok().flatten().is_some();
            self.backups = store::list_backups(&self.paths.backups(), self.region);
            self.notice = Some((
                Tone::Warning,
                t("These settings were changed outside the launcher since they were loaded. The page now compares against the new values; review your changes and apply again.").into(),
            ));
            return;
        }
        if !store::writable(self.region) {
            self.writable = false;
            self.notice = Some((
                Tone::Error,
                t("Windows refused the change: saving FINAL FANTASY XI's settings needs Crystal Launcher to run as administrator. Nothing was changed.").into(),
            ));
            return;
        }
        if !self.backed_up.contains(&self.region) {
            let location = self.region.location();
            match store::save_backup(&self.paths.backups(), self.region, &location, &current, chrono::Local::now().naive_local()) {
                Ok(_) => self.backed_up.push(self.region),
                Err(e) => {
                    self.notice = Some((Tone::Error, tf("{e} Nothing was changed.", &[("e", &e)])));
                    return;
                }
            }
        }
        match store::write(&self.paths, self.region, &self.original, &self.edits, self.menu_selected) {
            Ok(()) => {
                let count = pending.len();
                self.read_target();
                self.notice = Some((
                    Tone::Success,
                    if count == 1 {
                        t("Saved 1 change. FINAL FANTASY XI uses it the next time it starts.").into()
                    } else {
                        tf("Saved {count} changes. FINAL FANTASY XI uses them the next time it starts.", &[("count", &count)])
                    },
                ));
            }
            Err(e) => {
                self.notice = Some((Tone::Error, e));
                // Show what is actually stored now; the edits stay pending.
                if let Ok(v) = store::read(&self.paths, self.region) {
                    self.original = v;
                }
            }
        }
    }

    fn load_values(&mut self, values: &Values) {
        for key in model::MANAGED {
            if let Some(d) = values.get(key) {
                self.set(key, d.clone());
            }
        }
    }

    /// Takes stored values that changed since they were read as the new base.
    /// Keys the user did not edit follow the stored value, so the next Apply
    /// does not write the old ones back over the outside change.
    fn rebase(&mut self, current: Values) {
        for key in model::MANAGED {
            if self.edits.get(key) == self.original.get(key) {
                match current.get(key) {
                    Some(d) => self.edits.insert(key.into(), d.clone()),
                    None => self.edits.remove(key),
                };
            }
        }
        self.original = current;
    }
}

pub(crate) fn update(state: &mut State, msg: Msg) -> Effect<Msg> {
    state.revision = state.revision.wrapping_add(1);
    match msg {
        Msg::Load => state.load(),
        Msg::Region(region) => {
            if region != state.region && state.pending().is_empty() {
                state.region = region;
                state.notice = None;
                if state.preview {
                    state.writable = true;
                } else {
                    state.read_target();
                }
            }
        }
        Msg::Set(key, value) => state.set_dword(key, value),
        Msg::Window(w, h) => {
            // Keep the interface's magnification as the window changes. An
            // automatic (0, 0) menu already follows the window: leave it so.
            let automatic = state.automatic_menu();
            let scale = model::scale_of(state.dword(model::WINDOW_H), state.menu_size().1).max(1.0);
            state.set_dword(model::WINDOW_W, w);
            state.set_dword(model::WINDOW_H, h);
            if !automatic {
                let (mw, mh) = model::menu_for(w, h, scale);
                state.set_dword(model::MENU_W, mw.max(model::MIN_MENU.0));
                state.set_dword(model::MENU_H, mh.max(model::MIN_MENU.1));
            }
        }
        Msg::Menu(w, h) => {
            // An explicit choice must be stored even when a missing registry
            // value happens to display this fallback. Otherwise Apply would
            // treat the missing pair as automatic and remove the preference.
            state.edits.insert(model::MENU_W.into(), Data::Dword(w));
            state.edits.insert(model::MENU_H.into(), Data::Dword(h));
            // Selecting the currently displayed registry size still establishes
            // a preference when no Crystal menu size has been saved yet.
            state.menu_selected = !state.menu_retained && (w, h) != (0, 0);
        }
        Msg::Background(w, h) => {
            state.set_dword(model::BACKGROUND_W, w);
            state.set_dword(model::BACKGROUND_H, h);
        }
        Msg::Gamma(g) => state.set_dword(model::GAMMA, model::gamma_bits(g)),
        Msg::ScreenshotDir(dir) => state.set(model::SCREENSHOT_DIR, Data::Text(dir)),
        Msg::Apply => state.apply(),
        Msg::Discard => {
            state.edits = state.original.clone();
            state.menu_selected = false;
            state.notice = None;
        }
        Msg::Defaults => {
            let automatic = state.automatic_menu();
            let mut defaults = state.fallback.clone();
            if automatic {
                // Keep "Automatic · use launch profile"; defaults never pin a menu size.
                defaults.remove(model::MENU_W);
                defaults.remove(model::MENU_H);
            }
            state.load_values(&defaults);
            state.notice = Some((
                Tone::Info,
                t("Default settings loaded. Review them, then Apply to save.").into(),
            ));
        }
        Msg::LoadBackup(entry) => {
            if state.preview {
                state.notice = Some((Tone::Info, t("Design preview: no backups were read.").into()));
            } else {
                match store::load_backup(&entry.path) {
                    Ok(backup) => {
                        state.load_values(&backup.values);
                        state.notice = Some((
                            Tone::Info,
                            tf("Loaded the backup from {taken}. Review it, then Apply to save.", &[("taken", &backup.taken)]),
                        ));
                    }
                    Err(e) => state.notice = Some((Tone::Error, e)),
                }
            }
        }
        Msg::DismissNotice => state.notice = None,
    }
    Effect::none()
}

pub(crate) fn retail_menu(config: &crate::backend::Config) -> Result<Option<(u32, u32)>, String> {
    store::menu_override(&Paths::from_config(config), Region::Us)
}

pub(crate) use model::{Data as RegData, Values as RegValues};

/// The vendor key ("PlayOnlineUS", ...) controller settings are read from and
/// written to; one rule for the whole app (see `store::controller_region`).
pub(crate) fn controller_region() -> Option<&'static str> {
    store::controller_region().map(Region::vendor)
}

pub(crate) fn read_retail(vendor: &str) -> Result<Option<RegValues>, String> {
    store::read_key(vendor)
}

pub(crate) fn write_retail(vendor: &str, changes: &[(&str, &RegData)]) -> Result<(), String> {
    store::write_key(vendor, changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> State {
        let mut s = State {
            preview: true,
            ..State::default()
        };
        s.load();
        s.edits = s.original.clone();
        s
    }

    #[test]
    fn automatic_retail_menu_uses_the_window_without_pending_edits() {
        let mut s = loaded();
        s.original.remove(model::SCREENSHOT_DIR);
        for key in [model::MENU_W, model::MENU_H] {
            s.original.insert(key.into(), Data::Dword(0));
        }
        s.edits = s.original.clone();
        assert_eq!(s.menu_size(), (2560, 1440));
        assert!(s.pending().is_empty());
        assert!(model::validate(&s.edits).is_ok());
        let _ = update(&mut s, Msg::Set(model::MAX_SOUNDS, 20));
        assert_eq!(s.pending(), vec![model::MAX_SOUNDS]);
        let _ = update(&mut s, Msg::Window(1920, 1080));
        assert_eq!(s.menu_size(), (1920, 1080));
    }

    /// A window change must not turn the automatic menu into a fixed size
    /// (which Apply would also save as Crystal's menu override).
    #[test]
    fn window_change_keeps_an_automatic_menu_automatic() {
        let mut s = loaded();
        for key in [model::MENU_W, model::MENU_H] {
            s.original.insert(key.into(), Data::Dword(0));
        }
        s.edits = s.original.clone();
        let _ = update(&mut s, Msg::Window(2560, 1440));
        let _ = update(&mut s, Msg::Window(3840, 2160));
        assert_eq!(model::dword(&s.edits, model::MENU_W), Some(0));
        assert_eq!(model::dword(&s.edits, model::MENU_H), Some(0));
        assert_eq!(s.pending(), vec![model::WINDOW_W, model::WINDOW_H]);
        // Defaults keep it automatic too.
        let _ = update(&mut s, Msg::Defaults);
        assert_eq!(model::dword(&s.edits, model::MENU_W), Some(0));
        assert_eq!(model::dword(&s.edits, model::MENU_H), Some(0));
        assert!(!s.pending().contains(&model::MENU_W));
    }

    #[test]
    fn choosing_the_current_menu_can_retain_it_across_profile_starts() {
        let mut s = loaded();
        let size = s.menu_size();
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert_eq!(s.pending(), vec![model::MENU_W, model::MENU_H]);
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
        s.menu_retained = true;
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert!(s.pending().is_empty());
    }

    #[test]
    fn choosing_a_fallback_menu_stores_both_dimensions() {
        let mut s = loaded();
        for key in [model::MENU_W, model::MENU_H] {
            s.original.remove(key);
            s.edits.remove(key);
        }
        let size = s.menu_size();
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert_eq!(model::dword(&s.edits, model::MENU_W), Some(size.0));
        assert_eq!(model::dword(&s.edits, model::MENU_H), Some(size.1));
        assert_eq!(s.pending(), vec![model::MENU_W, model::MENU_H]);
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
        assert!(!s.edits.contains_key(model::MENU_W));
    }

    #[test]
    fn game_config_edits_stay_in_memory_until_apply() {
        let mut s = loaded();
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::MIP_MAPPING, 2));
        assert_eq!(s.pending(), vec![model::MIP_MAPPING]);
        // Setting the stored value again clears the change.
        let _ = update(&mut s, Msg::Set(model::MIP_MAPPING, 6));
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::BUMP_MAPPING, 0));
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
    }

    #[test]
    fn game_config_preview_never_writes() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Set(model::SOUND, 0));
        let _ = update(&mut s, Msg::Apply);
        assert_eq!(s.notice.as_ref().map(|n| n.0), Some(Tone::Info));
        assert_eq!(s.pending(), vec![model::SOUND]);
        assert!(s.backed_up.is_empty());
    }

    #[test]
    fn game_config_window_change_keeps_interface_scale() {
        let mut s = loaded(); // 2560 x 1440 with a 1708 x 960 menu: 1.5x
        let _ = update(&mut s, Msg::Window(3840, 2160));
        assert_eq!((s.dword(model::MENU_W), s.dword(model::MENU_H)), (2560, 1440));
        let _ = update(&mut s, Msg::Window(1280, 720));
        assert_eq!((s.dword(model::MENU_W), s.dword(model::MENU_H)), (854, 480));
    }

    #[test]
    fn game_config_missing_entries_show_fallback_without_pending() {
        let mut s = loaded();
        s.original.remove(model::KEEP_ASPECT);
        s.edits.remove(model::KEEP_ASPECT);
        assert_eq!(s.dword(model::KEEP_ASPECT), 1);
        let _ = update(&mut s, Msg::Set(model::KEEP_ASPECT, 1));
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::KEEP_ASPECT, 0));
        assert_eq!(s.pending(), vec![model::KEEP_ASPECT]);
    }

    #[test]
    fn game_config_defaults_load_as_pending_edits() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Defaults);
        assert!(!s.pending().is_empty());
        assert_eq!(s.dword(model::WINDOW_W), 3840);
        // Unmanaged values are untouched.
        assert_eq!(s.edits.get("0031"), Some(&Data::Dword(0x3bc4_9ba6)));
    }


    /// Settings changed outside the launcher become the new base; they never
    /// turn into pending edits that a second Apply would revert.
    #[test]
    fn outside_changes_rebase_untouched_keys_and_keep_the_users_edits() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Set(model::SOUND, 0));
        let mut current = s.original.clone();
        current.insert(model::WINDOW_MODE.into(), Data::Dword(2)); // the game saved Borderless
        current.remove(model::OPENING_MOVIE);
        s.rebase(current);
        assert_eq!(s.pending(), vec![model::SOUND]);
        assert_eq!(s.dword(model::WINDOW_MODE), 2);
        assert!(!s.edits.contains_key(model::OPENING_MOVIE));
    }

    #[test]
    fn game_config_region_switch_waits_for_pending_changes() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Set(model::SOUND, 0));
        let _ = update(&mut s, Msg::Region(Region::Eu));
        assert_eq!(s.region, Region::Us);
    }
}
