//! Where this process's FFXiMain.dll keeps what Crystal reads or calls, found
//! by signature so an FFXI update that moves code does not switch features off.
//!
//! One read-only pass per process ([`ffximain`]): copy the unpacked module
//! (see [`sigscan`]), find every locator, and resolve each value only when
//! every locator for it matches exactly once and all of them agree. Struct
//! offsets are extracted too and must equal the constants the readers use; a
//! changed layout switches the feature off instead of being guessed at.
//!
//! Known builds keep a table of their proven values. The scan still runs and
//! must reproduce the table, or the group is refused. Unknown builds use the
//! scan alone. Every group, key input included, is then re-checked byte for
//! byte in live memory by its reader before use. Never use an unresolved
//! group: each feature turns itself off with the logged reason.

use crate::{
    log,
    sigscan::{self, CaptureError, Image, Pattern},
};
use std::{
    fmt::Debug,
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) mod controls;
pub(crate) use controls::{Controller, Shutdown};

/// The lobby controller global and its get-error getter (relocation anchor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lobby {
    pub ptr: usize,
    pub get_error: usize,
}

/// Log-only network state: the main game-state manager global.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Diagnostics {
    pub main: usize,
}

/// Code sites of today's fixed byte checks, re-read live before use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MenuSites {
    pub focus: usize,
    pub selected: usize,
    pub help: usize,
    pub window: usize,
    pub confirm13: usize,
    pub help11: usize,
}

/// Login menu globals (all read-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Menus {
    pub focus: usize,
    pub license: usize,
    pub title: usize,
    pub characters: usize,
    pub help: usize,
    pub confirm: usize,
    pub selected: usize,
    pub window: usize,
    pub sites: MenuSites,
}

/// One resource group: its id, its group-table entry and its managed object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub id: usize,
    pub entry: usize,
    pub object: usize,
}

/// The friend display cache, the own-zone object and area-name resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Friends {
    pub lobby: usize,
    pub cache_getter: usize,
    pub name_getter: usize,
    pub display_area: usize,
    pub area_fn: usize,
    pub own_zone: usize,
    pub group_table: usize,
    pub legacy_table: usize,
    pub area: Group,
    pub fallback: Group,
    pub fallback_index: usize,
}

/// Key input: the game's input bank, its three key-down handlers, the keyboard
/// controller global, and the code sites the live checks re-read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Input {
    /// The input bank the three handlers write through (`this`).
    pub input: usize,
    pub pressed: usize,
    pub edge: usize,
    pub repeat: usize,
    pub keyboard: usize,
    pub iat_slot: usize,
    pub frame_call: usize,
    pub controller_call: usize,
    pub buffer_site: usize,
    /// The client's first-key-down path: `mov ecx, INPUT` ... the three calls.
    pub key_down: usize,
}

/// The input handlers' exact bodies (10123fc0, 10123f40, 10123f80): each
/// ORs its bit into `[this + scan]`; the first also sets `[this + 0x100]`.
pub(crate) const KEY_PRESSED: [u8; 28] = [0x8B, 0x44, 0x24, 4, 0x25, 0xFF, 0, 0, 0, 3, 0xC1,
    0x8A, 0x10, 0x80, 0xCA, 4, 0x88, 0x10, 0xC6, 0x81, 0, 1, 0, 0, 1, 0xC2, 4, 0];
pub(crate) const KEY_EDGE: [u8; 17] = [0x8B, 0x44, 0x24, 4, 0x25, 0xFF, 0, 0, 0, 3, 0xC1, 0x80, 8, 2, 0xC2, 4, 0];
pub(crate) const KEY_REPEAT: [u8; 17] = [0x8B, 0x44, 0x24, 4, 0x25, 0xFF, 0, 0, 0, 3, 0xC1, 0x80, 8, 1, 0xC2, 4, 0];
/// Bytes of the input bank the handlers write: 256 scan codes and the flag.
const KEY_BANK: usize = 0x101;
/// The key-down path and the GetKeyboardState call lie this close after the
/// start of the keyboard frame function (0x3ee in both known builds).
const KEYBOARD_FRAME_SPAN: usize = 0x1000;

pub(crate) struct Known {
    pub timestamp: u32,
    pub size: usize,
    pub lobby: Lobby,
    pub diagnostics: Diagnostics,
    pub menus: Menus,
    pub friends: Friends,
    pub input: Input,
    pub shutdown: Shutdown,
    pub controller: Controller,
}

/// The same RVAs in both known builds.
const SHUTDOWN: Shutdown = Shutdown { entry: 0x8_0DC0, caller: 0x1B_7E90, context: 0x66_2DA4, enabled: 0x47_D3F0, manager: 0x57_876C };

const CONTROLLER: Controller = Controller {
    reload: 0x2840,
    suspend: 0x5600,
    config: 0x3D_3CB8,
    config_vtable: 0x32_95FC,
    member_vtable: 0x32_A064,
    keyboard: 0x45_5C20,
    flags: 0x45_5C34,
    bindings: 0x45_5B20,
    guid: 0x45_5B10,
    config_tool: 0x66_2DBC,
    code: [0x2840, 0x5130, 0x5490, 0x60EB, 0x83D0, 0x8D90, 0x1_8FC0, 0x1_9060, 0x1B_CC30, 0x1B_CE30, 0x2B26, 0x2820, 0x2790,
        0x1_286A, 0x1B_CE07],
};

const FRIENDS: Friends = Friends {
    lobby: 0x4D_F770,
    cache_getter: 0x0E_6320,
    name_getter: 0x0E_7950,
    display_area: 0x1E_AF22,
    area_fn: 0x1C_BC10,
    own_zone: 0x4D_FD98,
    group_table: 0x37_1DA8,
    legacy_table: 0x57_8548,
    area: Group { id: 4, entry: 21, object: 0x57_85F8 },
    fallback: Group { id: 9, entry: 3, object: 0x57_8658 },
    fallback_index: 0x40,
};

const MENU_GLOBALS: Menus = Menus {
    focus: 0x62_188C,
    license: 0x66_9000,
    title: 0x66_9050,
    characters: 0x66_9080,
    help: 0x66_9078,
    confirm: 0x66_9004,
    selected: 0x66_908C,
    window: 0x45_69A8,
    sites: MenuSites { focus: 0x15_FDF0, selected: 0x24_1842, help: 0x24_3699, window: 0x01_5A57, confirm13: 0x24_3BBA, help11: 0x24_3C18 },
};

const KEYS: Input = Input {
    input: 0x4E_1C40,
    pressed: 0x12_3FC0,
    edge: 0x12_3F40,
    repeat: 0x12_3F80,
    keyboard: 0x45_5C20,
    iat_slot: 0x32_9388,
    frame_call: 0x5E39,
    controller_call: 0x1_2875,
    buffer_site: 0x5E32,
    key_down: 0x5CE3,
};

/// The build every original constant was read from.
pub(crate) const BUILD_6A995428: Known = Known {
    timestamp: 0x6A99_5428,
    size: 0xC1_5000,
    lobby: Lobby { ptr: 0x4D_F770, get_error: 0x0E_F920 },
    diagnostics: Diagnostics { main: 0x36_01BC },
    menus: MENU_GLOBALS,
    friends: FRIENDS,
    input: KEYS,
    shutdown: SHUTDOWN,
    controller: CONTROLLER,
};

/// The 2026-10-13 update: the same data layout, lobby UI code moved +0x20.
/// Every value here was derived and independently re-derived from its unpacked
/// image. Key input is byte-identical at the same RVAs (two reviews, and the
/// scan reproduces it).
pub(crate) const BUILD_6AC37846: Known = Known {
    timestamp: 0x6AC3_7846,
    size: 0xC1_5000,
    lobby: Lobby { ptr: 0x4D_F770, get_error: 0x0E_F920 },
    diagnostics: Diagnostics { main: 0x36_01BC },
    menus: Menus {
        sites: MenuSites { focus: 0x15_FDF0, selected: 0x24_1862, help: 0x24_36B9, window: 0x01_5A57, confirm13: 0x24_3BDA, help11: 0x24_3C38 },
        ..MENU_GLOBALS
    },
    friends: FRIENDS,
    input: KEYS,
    shutdown: SHUTDOWN,
    controller: CONTROLLER,
};

pub(crate) const KNOWN: [&Known; 2] = [&BUILD_6A995428, &BUILD_6AC37846];

/// Every scanned locator. A value's locators must each match exactly once in
/// `.text`; anchored follow-up shapes are checked in place, never scanned.
const PATTERNS: &[(&str, &str)] = &[
    // Lobby controller and its connection state (100ef920, 100ed760, 100edf27, 100ef8f0, ...).
    // Lobby error-code literals are wildcards here; the two locators that
    // compare lists of them only corroborate (see `lobby`).
    ("lobby.get_error", "A1 ?? ?? ?? ?? 85 C0 74 ?? E8 ?? ?? ?? ?? 85 C0 75 ?? A1 ?? ?? ?? ?? 8B 80 ?? ?? ?? ?? 85 C0 75 ?? B8 ?? 00 00 00 C3 B8 ?? 00 00 00 C3"),
    ("lobby.clear_retryable", "83 3D ?? ?? ?? ?? 02 75 ?? 8B 0D ?? ?? ?? ?? 8B 81 ?? ?? ?? ?? 3D 39 01 00 00 74 ?? 3D 42 01 00 00 74 ?? 3D 44 01 00 00 74 ?? 3D 3C 01 00 00 75 ?? C7 81 ?? ?? ?? ?? 00 00 00 00 33 C0 C3"),
    ("lobby.drain", "A1 ?? ?? ?? ?? 8D 90 ?? ?? ?? ?? 8B 80 ?? ?? ?? ?? 52 50 E8 ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 83 C4 08 8B 8A ?? ?? ?? ?? 8D 82 ?? ?? ?? ?? 85 C9 74 ?? 83 F9 FF 74 ?? 8B 8A ?? ?? ?? ?? 50 51 E8 ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 83 C4 08 8B 8A ?? ?? ?? ?? 8D 82 ?? ?? ?? ?? 85 C9 75 ?? C7 82 ?? ?? ?? ?? 00 00 00 00"),
    ("lobby.tcp_protocol_error", "51 8B 0D ?? ?? ?? ?? 33 C0 89 44 24 00 8B 89 ?? ?? ?? ?? 85 C9 74 ?? 8D 54 24 00 52 51 E8 ?? ?? ?? ?? 8B 44 24 08 83 C4 08 59 C3"),
    ("lobby.reply_check", "8B 90 ?? ?? ?? ?? 8D 88 ?? ?? ?? ?? 51 52 E8 ?? ?? ?? ?? A1 ?? ?? ?? ?? 83 C4 08 39 98 ?? ?? ?? ?? 75 ?? FF 07 8B 0D ?? ?? ?? ?? 83 B9 ?? ?? ?? ?? FF 74 ??"),
    ("lobby.socket_ready", "83 3D ?? ?? ?? ?? 02 75 ?? A1 ?? ?? ?? ?? 8B 88 ?? ?? ?? ?? 85 C9 75 ?? 8B 80 ?? ?? ?? ?? 85 C0 74 ?? 50 E8 ?? ?? ?? ?? 83 C4 04 83 F8 01 75 ?? 33 C0 C3 83 C8 FF C3"),
    ("tcp.process", "83 EC 10 55 56 8B 74 24 1C 33 ED 57 8B 86 ?? ?? ?? ?? 8B BE ?? ?? ?? ?? 3B C5 75 ?? 8B 44 24 24 5F 5E 89 28 B8 01 00 00 00 5D 83 C4 10 C3 53 8D 9E ?? ?? ?? ?? 53 E8 ?? ?? ?? ?? 8B 86 ?? ?? ?? ?? 83 C4 04 48 83 F8 ?? 0F 87 ?? ?? ?? ?? FF 24 85 ?? ?? ?? ?? 8B 86 ?? ?? ?? ?? 83 F8 06 0F 85 ?? ?? ?? ?? C7 87 ?? ?? ?? ?? 64 00 00 00"),
    ("tcp.socket_error_copy", "8B 86 ?? ?? ?? ?? 83 F8 06 0F 85 ?? ?? ?? ?? 8B 86 ?? ?? ?? ?? 3B C5 74 ?? 89 87 ?? ?? ?? ?? 5B 89 AE ?? ?? ?? ?? 5F 5E C7 01 FF FF FF FF B8 01 00 00 00 5D 83 C4 10 C3"),
    ("main.pump_loop", "BF 2D 00 00 00 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 4F 75 ??"),
    ("main.lobby_error_check", "8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 3D 4B 01 00 00 0F 84 ?? ?? ?? ?? 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 3D 39 01 00 00 74 ?? 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 3D 42 01 00 00"),
    ("main.init_call", "8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 84 C0 75 ?? 6A 05 E8 ?? ?? ?? ?? 83 C4 04"),
    ("main.request_state", "56 8B F1 57 33 FF 8B 86 98 00 00 00 3B C7 8B 44 24 0C 0F 84 ?? ?? ?? ?? 3D CB 00 00 00 0F 87 ?? ?? ?? ?? 33 C9 8A 88 ?? ?? ?? ?? FF 24 8D ?? ?? ?? ?? 89 BE ?? ?? ?? ?? 89 7E ?? C7 86 90 00 00 00 64 00 00 00 5F B8 01 00 00 00 5E C2 08 00 89 7E ?? C7 86 ?? ?? ?? ?? 01 00 00 00"),
    ("main.init", "89 5E 08 89 5E ?? 89 9E 80 00 00 00 C7 86 90 00 00 00 65 00 00 00 C7 86 8C 00 00 00 FF FF FF FF C7 86 94 00 00 00 C8 00 00 00 89 5E 14 E8 ?? ?? ?? ?? 89 86 98 00 00 00 E8 ?? ?? ?? ?? 89 86 9C 00 00 00 E8 ?? ?? ?? ?? 0F BF C0 89 86 A4 00 00 00 8B 86 98 00 00 00 B9 03 00 00 00 89 9E A0 00 00 00 3B C1 89 9E B4 00 00 00 75 ?? 89 9E ?? ?? ?? ?? EB ?? 89 8E ?? ?? ?? ?? 88 5E 18"),
    ("op.connect", "56 8B 74 24 08 8B 86 ?? ?? ?? ?? 85 C0 74 ?? 33 C0 5E C3 8B 86 ?? ?? ?? ?? 8D 96 ?? ?? ?? ?? 8B 88 20 01 00 00 83 C0 20 51 50 52 E8 ?? ?? ?? ?? 83 C4 0C 85 C0 7D ?? 33 C0 5E C3 C7 86 ?? ?? ?? ?? ?? 00 00 00 B8 01 00 00 00 5E C3"),
    ("op.login", "83 EC 10 56 57 8B 7C 24 1C 8B 87 ?? ?? ?? ?? 85 C0 74 ?? 5F 33 C0 5E 83 C4 10 C3 8D B7 ?? ?? ?? ?? 56 E8 ?? ?? ?? ?? 83 C4 04 85 C0 7D ?? 5F 33 C0 5E 83 C4 10 C3 8D 44 24 08 50 56 E8 ?? ?? ?? ?? 8B 8F ?? ?? ?? ?? 8B 54 24 10 51 6A 26 52 E8 ?? ?? ?? ?? 8D 44 24 1C 50 56 E8 ?? ?? ?? ?? 83 C4 1C C7 87 ?? ?? ?? ?? ?? 00 00 00 B8 01 00 00 00"),
    ("op.char_list", "83 EC 10 53 56 57 8B 7C 24 20 8B 87 ?? ?? ?? ?? 8B 9F ?? ?? ?? ?? 85 C0 74 ?? 5F 5E 33 C0 5B 83 C4 10 C3 8D B7 ?? ?? ?? ?? 56 E8 ?? ?? ?? ?? 83 C4 04 85 C0 7D ?? 5F 5E 33 C0 5B 83 C4 10 C3 8D 44 24 0C 50 56 E8 ?? ?? ?? ?? 8B 4C 24 14 53 6A 1F 51 E8 ?? ?? ?? ?? 8D 54 24 20 52 56 E8 ?? ?? ?? ?? 83 C4 1C C7 87 ?? ?? ?? ?? ?? 00 00 00 B8 01 00 00 00"),
    ("op.char_select", "83 EC 10 53 56 57 8B 7C 24 20 8B 87 ?? ?? ?? ?? 8B 9F ?? ?? ?? ?? 85 C0 74 ?? 5F 5E 33 C0 5B 83 C4 10 C3 8D B7 ?? ?? ?? ?? 56 E8 ?? ?? ?? ?? 83 C4 04 85 C0 7D ?? 5F 5E 33 C0 5B 83 C4 10 C3 8B 44 24 24 8D 4C 24 0C 51 56 89 83 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 54 24 14 53 6A 07 52 E8 ?? ?? ?? ?? 8D 44 24 20 50 56 E8 ?? ?? ?? ?? 83 C4 1C C7 87 ?? ?? ?? ?? ?? 00 00 00 B8 01 00 00 00"),
    ("op.close", "56 8B 74 24 08 8B 86 ?? ?? ?? ?? 85 C0 74 ?? 33 C0 5E C3 8D 86 ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? 83 C4 04 85 C0 7D ?? 33 C0 5E C3 C7 86 ?? ?? ?? ?? ?? 00 00 00 B8 01 00 00 00 5E C3"),
    // Lobby character table (100efca0, 100efda0, 100ef790, 1023db90, 1023db20).
    ("chars.content_getter", "8B 4C 24 04 8D 04 CD 00 00 00 00 2B C1 8B 0D ?? ?? ?? ?? 8D 04 80 8B 84 81 ?? ?? ?? ?? C3"),
    ("chars.record_reader", "8B 4C 24 04 33 D2 8D 04 CD 00 00 00 00 2B C1 8B 0D ?? ?? ?? ?? 8D 04 80 8A 94 81 ?? ?? ?? ?? 8D 04 81 33 C9 66 8B 88 ?? ?? ?? ?? C1 E2 10 03 D1 52 66 8B 90 ?? ?? ?? ?? 8B 80 ?? ?? ?? ?? 52 50 E8"),
    ("chars.count_cluster", "A1 ?? ?? ?? ?? 8B 80 ?? ?? ?? ?? C3 90 90 90 90 8B 4C 24 04 8D 04 CD 00 00 00 00 2B C1 8B 0D ?? ?? ?? ?? 8D 04 80 8D 84 81 ?? ?? ?? ?? C3"),
    ("chars.menu_builder", "83 EC 0C 8B 0D ?? ?? ?? ?? 53 55 56 57 E8 ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 89 44 24 18 B9 ?? ?? ?? ?? 33 C0"),
    ("chars.collector", "83 EC ?? 53 55 56 57 33 DB 8D 6C 24 10 8B 0D ?? ?? ?? ?? 53 E8 ?? ?? ?? ?? 85 C0 74 ?? 83 F8 01 74 ?? 83 F8 02 75 ?? 8B 0D ?? ?? ?? ?? 53 BF ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B F0 B9 02 00 00 00 33 C0 F3 A6 74 ?? 8B 0D ?? ?? ?? ?? 53 E8 ?? ?? ?? ?? 89 45 00 83 C5 04 43 83 FB ?? 7C ??"),
    ("chars.menu_tail", "43 83 FB ?? 0F 8C ?? ?? ?? ?? A1 ?? ?? ?? ?? 39 70 24 75 ?? 83 7C 24 18 ?? 75 ?? C7 40 24 02 00 00 00"),
    // Login menus (1015fdf0, 1023e200, 102406b0, 10243669, 1023b22d, 1023e350, 10241836, 100158fa, ...).
    ("menu.focus_loop", "8B 48 0C 85 C9 74 ?? 8B 51 08 85 D2 74 ?? 3B 05 ?? ?? ?? ?? 75 ?? 8B 11 6A 01 FF 52 28"),
    ("menu.focus_usable", "8B 41 54 85 C0 74 ?? 83 78 10 0E 74 ?? 8A 48 70 84 C9 75 ?? B0 01 C3"),
    ("menu.set_focus", "56 8B F1 57 8B 7C 24 0C 8B 4E 54 3B CF 74 ?? 85 C9 74 ?? 6A 00 E8 ?? ?? ?? ?? 8B 46 54 C6 80 98 00 00 00 00 8B 4E 54 89 7E 54 85 FF 89 4E 58"),
    ("menu.check_help11", "8B 15 ?? ?? ?? ?? C7 42 2C 11 00 00 00 A1 ?? ?? ?? ?? C6 44 24 13 01"),
    ("menu.login_confirm_call", "39 58 ?? 74 ?? 83 C0 44 53 50 53 6A ?? E8 ?? ?? ?? ?? 83 C4 10 3B C3 0F 8C"),
    ("menu.license_run", "6A 00 6A 01 68 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? A1 ?? ?? ?? ?? 83 C0 1C 50 E8 ?? ?? ?? ?? 83 C4 04 B9 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 0D ?? ?? ?? ?? 8B 41 18 C3"),
    ("menu.license_factory", "6A 20 E8 ?? ?? ?? ?? 8B F0 83 C4 04 3B F3 74 ?? 8B CE E8 ?? ?? ?? ?? C7 06 ?? ?? ?? ?? 89 5E 1C 89 5E 14 89 6E 18 89 35 ?? ?? ?? ?? EB ?? 89 1D ?? ?? ?? ??"),
    ("menu.title_factory", "6A 40 E8 ?? ?? ?? ?? 8B F0 83 C4 04 3B F3 74 ?? 8B CE E8 ?? ?? ?? ?? 8D 7E 20 B9 08 00 00 00 33 C0 C7 06 ?? ?? ?? ?? 89 5E 18 89 5E 1C F3 AB 89 35 ?? ?? ?? ?? EB ?? 89 1D ?? ?? ?? ??"),
    ("menu.title_run", "53 6A 01 68 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 33 F6 53 E8 ?? ?? ?? ?? 83 C4 04 85 C0 0F 8C ?? ?? ?? ?? 46 83 FE 0A 7C ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? 85 C0 0F 8C ?? ?? ?? ?? BE 04 00 00 00 8B 15 ?? ?? ?? ??"),
    ("menu.title_fields", "8B 15 ?? ?? ?? ?? 83 C2 1C 52 E8 ?? ?? ?? ?? 83 C4 04 85 C0 0F 8C ?? ?? ?? ?? A1 ?? ?? ?? ?? 39 70 18 0F 85 ?? ?? ?? ?? 8B 40 ?? C6 40 ?? 01"),
    ("menu.play_row_marked", "A1 ?? ?? ?? ?? 83 F8 FF 0F 84 ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 8D 0C 80 8D 04 48 C1 E0 04 03 C2 39 98 ?? ?? ?? ?? 74 ?? 6A ?? E8"),
    ("menu.help_delete_mode", "68 ?? ?? ?? ?? E8 ?? ?? ?? ?? A1 ?? ?? ?? ?? 89 1D ?? ?? ?? ?? 89 58 1C 8B 0D ?? ?? ?? ?? C7 41 14 02 00 00 00 8B 15 ?? ?? ?? ?? C7 42 ?? ?? 00 00 00"),
    ("menu.help_play_mode", "A1 ?? ?? ?? ?? 89 1D ?? ?? ?? ?? 89 58 1C 53 56 68 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 53 56 68 ?? ?? ?? ?? B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 0D ?? ?? ?? ?? C7 41 14 02 00 00 00 8B 15 ?? ?? ?? ?? C7 42 ?? ?? 00 00 00"),
    ("menu.characters_factory", "68 ?? ?? 00 00 E8 ?? ?? ?? ?? 83 C4 04 8B F0 3B F3 5F 74 ?? 8B CE E8 ?? ?? ?? ?? 8B CE C7 06 ?? ?? ?? ?? E8 ?? ?? ?? ?? 89 35 ?? ?? ?? ?? EB ?? 89 1D ?? ?? ?? ??"),
    ("menu.confirm_fields", "A1 ?? ?? ?? ?? 8B 4C 24 04 53 33 DB 89 58 30 88 98 E1 A5 00 00 89 58 ?? 88 58 20 88 98 ?? ?? ?? ?? C7 40 ?? 01 00 00 00 A1 ?? ?? ?? ?? 56 89 48 ?? 8B 15 ?? ?? ?? ?? 8B 44 24 10 89 42 1C 8B 0D ?? ?? ?? ?? 8B 54 24 18 89 51 ??"),
    ("menu.confirm_plain", "A1 ?? ?? ?? ?? 8B 4C 24 04 53 33 DB 89 58 30 88 98 E1 A5 00 00 89 58 18 88 58 20 88 98 E0 A5 00 00 C7 40 14 01 00 00 00 A1 ?? ?? ?? ?? 56 53 6A 01 89 48 18 8B 15 ?? ?? ?? ?? 8B 44 24 18 68 ?? ?? ?? ?? 89 42 1C 8B 0D ?? ?? ?? ??"),
    ("menu.confirm_factory", "68 ?? ?? 00 00 E8 ?? ?? ?? ?? 8B F0 83 C4 04 3B F3 74 ?? 8B CE E8 ?? ?? ?? ?? 8B CE C7 06 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B CE E8 ?? ?? ?? ?? 89 35 ?? ?? ?? ?? EB ?? 89 1D ?? ?? ?? ??"),
    ("menu.cursor_fields", "8B 46 ?? B9 01 00 00 00 88 4E 32 57 0F BF 40 ?? 48 A3 ?? ?? ?? ?? 8D 3C 80 8D 04 78 C1 E0 04 8B 7C 30 ?? 85 FF 5F 74 ?? 8B 46 ?? 89 4E 1C 88 48 ??"),
    ("menu.find_by_name", "8B 46 ?? 85 C0 74 ?? 83 C0 ?? 6A 10 50 53 E8 ?? ?? ?? ?? 83 C4 0C 85 C0 74"),
    ("menu.window_create", "51 52 68 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 1C A3 ?? ?? ?? ?? 85 C0 0F 84"),
    ("menu.window_destroy", "8B 0D ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 51 52 68 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? C7 05 ?? ?? ?? ?? 00 00 00 00"),
    // Friend display cache (100e6320, 101eaec0, 101cbc10, 10152f20, 102578b0, 101530c0, ...).
    ("friend.cache_getter", "66 8B 44 24 04 66 3D ?? ?? 7C ?? 33 C0 C3 8B 0D ?? ?? ?? ?? 0F BF C0 C1 E0 08 8D 84 08 ?? ?? ?? ?? C3"),
    ("friend.cache_caller", "66 81 FB ?? ?? 7D ?? 53 E8 ?? ?? ?? ?? 8B F0 83 C4 04 85 F6 74 ?? 56 E8"),
    ("friend.name_getter", "8B 44 24 04 F6 80 ?? ?? ?? ?? 01 74 ?? 05 ?? ?? ?? ?? C3 33 C0 C3"),
    ("friend.display_name", "55 E8 ?? ?? ?? ?? 83 C4 04 85 C0 74 ?? 55 E8 ?? ?? ?? ?? 8B 38 8D 4B ??"),
    ("friend.privacy_getter", "8B 44 24 04 F6 80 ?? ?? ?? ?? 40 74 ?? 66 8B 80 ?? ?? ?? ?? C3 83 C8 FF C3"),
    ("friend.display_area", "55 E8 ?? ?? ?? ?? B9 ?? 00 00 00 8D 7B ?? 8D B5 ?? ?? ?? ?? 83 C4 04 F3 A5 8B 74 24 18 8B 0D ?? ?? ?? ?? 84 C0 75 ?? 85 C9 74 ?? 8B 91 ?? ?? ?? ?? 33 C0 66 8B 43 ?? 3B D0 74 ?? 68 80 80 80 80 6A 00 50 E8 ?? ?? ?? ??"),
    ("friend.zone_getter", "8B 44 24 04 66 8B 80 ?? ?? 00 00 C3"),
    ("friend.display_area_visible", "C2 0C 00 85 C9 74 ?? 8B 91 ?? ?? ?? ?? 33 C0 66 8B 43 ?? 3B D0 74 ?? 68 80 80 80 80 6A 00 50 E8 ?? ?? ?? ?? 83 C4 08 8B CE 50 6A 23 6A 05"),
    ("friend.other_area", "8B 0D ?? ?? ?? ?? 85 C9 74 ?? 8B 91 ?? ?? ?? ?? 33 C0 66 8B 45 2C 3B D0 74 ?? 68 80 80 80 80 6A 00 50 E8 ?? ?? ?? ?? 83 C4 08 8B CE 50 6A 23 6A 04"),
    ("friend.display_head", "51 53 55 8B 6C 24 18 56 57 55 89 4C 24 14 E8 ?? ?? ?? ?? 8B 5C 24 20 55 66 89 43 ?? E8 ?? ?? ?? ?? 55 66 89 43 ?? E8 ?? ?? ?? ?? 83 C4 0C 84 C0 75 ?? 8B 0D ?? ?? ?? ?? 33 C0 66 8B 43 ?? 68 80 80 80 80 50 E8 ?? ?? ?? ??"),
    ("friend.area_fn", "53 8A 5C 24 0C 84 DB 74 ?? 8B 44 24 08 50 6A ?? EB ?? 8B 4C 24 08 51 6A ?? E8 ?? ?? ?? ?? 83 C4 08 85 C0 75 ?? 84 DB 75 ?? 5B C7 44 24 08 ?? ?? ?? ?? C7 44 24 04 ?? ?? ?? ?? E9 ?? ?? ?? ??"),
    ("friend.group_lookup", "56 57 8B 7C 24 0C 33 F6 39 3C F5 ?? ?? ?? ?? 74 ?? 46 83 FE ?? 72 ?? 8B 04 BD ?? ?? ?? ?? 85 C0 75 ?? 5F 5E C3 8B 0C F5 ?? ?? ?? ?? 85 C9 74 ?? E8 ?? ?? ?? ?? 84 C0 74 ?? 8B 44 24 10 8B 0C F5 ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? 5F 5E C3 5F 5E 89 44 24 04 E9 ?? ?? ?? ??"),
    ("friend.legacy_free", "56 8B 74 24 08 8B 04 B5 ?? ?? ?? ?? 85 C0 74 ?? 50 E8 ?? ?? ?? ?? 83 C4 04 C7 04 B5 ?? ?? ?? ?? 00 00 00 00 B0 01 5E C3"),
    ("friend.legacy_clear", "57 B9 20 00 00 00 33 C0 BF ?? ?? ?? ?? F3 AB 5F C3"),
    ("friend.own_world_check", "A1 ?? ?? ?? ?? 66 81 B8 ?? ?? ?? ?? FF FF 74 ?? 6A 01 E8 ?? ?? ?? ?? 59 C3"),
    ("friend.same_world", "8B 4C 24 04 32 C0 F6 81 ?? ?? ?? ?? 01 74 ?? 8B 51 08 56 8B 71 0C 0F AC F2 11 83 E2 07 C1 E2 04 C1 EE 11 8A 4C 0A ?? 8B 15 ?? ?? ?? ?? 81 E1 FF 00 00 00 5E 66 3B 8A ?? ?? ?? ?? 75 ?? B0 01 C3"),
    ("friend.row_refresh", "A1 ?? ?? ?? ?? 8D B4 06 ?? ?? ?? ?? 56 55 E8 ?? ?? ?? ?? 83 C4 08 85 C0 0F 8C ?? ?? ?? ?? 8B 86 ?? ?? ?? ?? 33 C9 83 E0 01 0B C1 0F 84 ?? ?? ?? ?? 8B 4E 0C 8B 56 08 8B C1 33 FF D1 E8 25 FF 03 00 00 83 F8 01 0F 85 ?? ?? ?? ?? 85 FF 0F 85 ?? ?? ?? ?? 8B C2 25 00 00 01 00 0B C7 0F 84 ?? ?? ?? ?? 0F AC CA 11 83 E2 07 C1 E2 04 C1 E9 11 8A 44 32 18 8D 74 32 18 A8 01 0F 84 ?? ?? ?? ?? 66 83 7E 02 01 0F 85 ?? ?? ?? ?? 8A 56 06 8B 0D ?? ?? ?? ?? 81 E2 FF 00 00 00 66 3B 91 ?? ?? ?? ?? 0F 85 ?? ?? ?? ?? 8B 46 04 8B D0 25 FF FF 00 00 C1 EA 08 81 E2 00 00 0F 00 0B D0 8B 44 24 ?? 39 94 08 ?? ?? ?? ??"),
    ("friend.row_scan", "8B 0D ?? ?? ?? ?? 8D 9C 0F ?? ?? ?? ?? 53 56 E8 ?? ?? ?? ?? 83 C4 08 85 C0 7C ?? 8B 83 ?? ?? ?? ?? 33 C9 83 E0 01"),
    ("friend.world_name", "56 8B 35 ?? ?? ?? ?? 33 C0 57 8B 96 ?? ?? ?? ?? 85 D2 76 ?? 8B 7C 24 0C 8D 8E ?? ?? ?? ?? 39 39 74 ?? 40 83 C1 ?? 3B C2 72 ?? 5F 5E B8 ?? ?? ?? ?? C3 8D 04 80 5F 8D 84 86 ?? ?? ?? ?? 5E C3"),
    ("friend.world_label", "83 B9 ?? ?? ?? ?? 03 75 ?? 8B 44 24 04 50 E8 ?? ?? ?? ?? 83 C4 04 C2 04 00 33 C0 C2 04 00"),
    ("friend.reply", "83 3F 03 75 ?? 8B 0D ?? ?? ?? ?? 32 C0 8A 91 ?? ?? ?? ?? 84 D2 7E ?? 8B 77 08 8B 76 ?? 0F BE E8 3B B4 A9 ?? ?? ?? ?? 74 ?? FE C0 3A C2 7C ?? 5F 5E 5D C3 0F BE F0 C7 84 B1 ?? ?? ?? ?? 00 00 00 00 8B 15 ?? ?? ?? ?? 8B 4F 08 66 8B 84 72 ?? ?? ?? ?? 51 50 E8"),
    ("friend.mark_complete", "0F BF 44 24 04 8B 0D ?? ?? ?? ?? C1 E0 08 8D 84 08 ?? ?? ?? ?? 83 08 01 C3"),
    ("friend.in_use", "8B 44 24 04 8A 80 ?? ?? ?? ?? 24 01 C3"),
    ("friend.managed_lookup", "8B 41 ?? 8B 54 24 04 56 8B 70 ?? 3B D6 5E 72 ?? 33 C0 C2 08 00 66 83 78 ?? ?? 75 ?? 66 8B 40 ?? 8B 49 ?? 50 8B 44 24 0C 50 52 51 E8 ?? ?? ?? ?? 83 C4 10 C2 08 00 8B 44 24 08 50 52 E8 ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? 83 C4 08 C2 08 00"),
    ("friend.legacy_lookup", "8B 54 24 04 66 83 7A ?? 02 7D ?? 89 54 24 04 E9 ?? ?? ?? ?? 8B 4C 24 08 85 C9 7C ?? 8B 42 ?? 3B C8 7D ?? 8D 0C 49 8D 04 40 8B 4C 8A ?? 8D 04 81 8D 44 10 ?? C3 33 C0 C3"),
    // Key input (10005ce3 first key down, 10005e2e keyboard import call, 10012875 controller frame call).
    ("input.key_down", "B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 84 C0 B9 ?? ?? ?? ?? 56 75 ?? E8 ?? ?? ?? ?? 56 B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? 56 B9 ?? ?? ?? ?? E8 ?? ?? ?? ?? C7 07 00 00 00 00"),
    ("input.keyboard_frame", "8B 74 24 ?? 8D 8E ?? ?? ?? ?? 51 FF 15 ?? ?? ?? ??"),
    ("input.controller_call", "8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 0D ?? ?? ?? ?? E8"),
];

// Anchored shapes, checked only where a locator's call or jump lands.
const SOCKET_PREDICATE: &str = "8B 44 24 04 8B 80 ?? ?? ?? ?? 85 C0 75 ?? C3 33 C9 83 F8 06 0F 95 C1 8B C1 C3";
const PROTOCOL_ERROR_GETTER: &str = "8B 44 24 04 8B 88 ?? ?? ?? ?? 8B 44 24 08 8B 91 ?? ?? ?? ?? 89 10 B8 01 00 00 00 C3";
const MAIN_STEP_FN: &str = "51 53 56 8B F1 8B 46 08 83 F8 04 0F 87 ?? ?? ?? ?? FF 24 85 ?? ?? ?? ?? 8B CE E8 ?? ?? ?? ?? 8A D8";
const MAIN_TICK_FN: &str = "53 56 8B F1 33 DB 39 9E 8C 00 00 00 7C ?? C7 86 8C 00 00 00 FF FF FF FF 39 9E 98 00 00 00 75 ?? 83 BE 90 00 00 00 64 0F 85 ?? ?? ?? ?? 8B 86 ?? ?? ?? ?? C7 86 90 00 00 00 65 00 00 00 89 86 8C 00 00 00 5E B0 01 5B C3 8B 86 ?? ?? ?? ??";
const MAIN_INIT_FN: &str = "56 8B F1 E8 ?? ?? ?? ?? 3C 01 0F 85 ?? ?? ?? ?? 53 33 DB 89 5E 08 89 5E ?? 89 9E 80 00 00 00 C7 86 90 00 00 00 65 00 00 00";
const CONNECT_RESOLVER: &str = "55 8B EC 83 EC 18 56 B8 CC CC CC CC 89 45 E8 89 45 EC 89 45 F0 89 45 F4 89 45 F8 89 45 FC 8B F4 8B 45 0C 50 FF 15 ?? ?? ?? ??";
const COUNT_GETTER: &str = "A1 ?? ?? ?? ?? 8B 80 ?? ?? ?? ?? C3";
const ENTRY_WRAPPER: &str = "8B 44 24 04 50 E8 ?? ?? ?? ?? 83 C4 04 C2 04 00";
const VISIBLE_FN: &str = "8B 44 24 04 F6 80 ?? ?? ?? ?? 40 75 ?? 32 C0 C3 8B 80 ?? ?? ?? ?? C1 E8 ?? F6 D0 24 01 C3";
const RESOURCE_PRESENT: &str = "8B 41 ?? 85 C0 0F 95 C0 C3";
const RESOURCE_WRAP: &str = "8B 44 24 04 6A 00 50 E8 ?? ?? ?? ?? C2 04 00";
const RESOURCE_FIXED: &str = "8B 4C 24 04 85 C9 75 ?? A1 ?? ?? ?? ?? C3 8B 44 24 10 8B 54 24 0C 25 FF FF 00 00 0F AF 44 24 08 03 C2 8B 54 C1 ?? 85 D2 8D 04 C1 75 ?? A1 ?? ?? ?? ?? C3 8B 00 03 C1 C3";
const RESOURCE_ENTRY: &str = "8B 41 ?? 8B 54 24 04 56 3B 50 ?? 72 ?? 33 C0 5E C2 04 00 8B 70 ?? 85 F6 74 ?? 8B 41 ?? 5E 8B 04 D0 8B 51 ?? 03 C2 C2 04 00 8B 40 ?? 5E 0F AF C2 03 41 ?? C2 04 00";
const RESOURCE_TEXT: &str = "8B 4C 24 04 85 C9 75 ?? 33 C0 C3 8B 44 24 08 33 D2 66 8B 11 3B C2 72 ?? 33 C0 C3 8B 54 C1 ?? 85 D2 74 ?? 33 C0 C3 8B 44 C1 ?? 03 C1 66 83 38 ?? 74 ?? 33 C0 C3 83 C0 ?? C3";
const LEGACY_SMALL: &str = "8B 54 24 08 85 D2 7C ?? 8B 44 24 04 8B 48 ?? 3B D1 7D ?? 0F BF 54 90 ?? 03 C2 8D 44 88 ?? C3 33 C0 C3";
const ROW_DETAIL_COPY: &str = "0F BF 44 24 04 8B 0D ?? ?? ?? ?? 56 8B 74 24 0C 57 C1 E0 08 8D BC 08 ?? ?? ?? ?? B9 ?? 00 00 00 F3 A5 5F 5E C3";
/// The keyboard controller's frame method (10005a40): saves `this` on its stack.
const KEYBOARD_FRAME_FN: &str = "81 EC ?? ?? ?? ?? 53 55 56 8B F1 57 89 74 24 ??";

/// A relocated module has hundreds of `call [IAT]` sites; see
/// [`Image::import_calls`].
const MIN_IMPORT_CALLS: usize = 16;

pub(crate) type Found<T> = Result<T, String>;

/// One scan of `.text` for a pattern table, plus checked accessors whose
/// errors name what was missing.
pub(crate) struct Ctx<'a> {
    pub(crate) image: &'a Image,
    names: &'a [(&'a str, &'a str)],
    hits: Vec<Vec<usize>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(image: &'a Image, names: &'a [(&'a str, &'a str)]) -> Self {
        // An unparsable entry becomes a lone wildcard, which never matches.
        let patterns: Vec<Pattern> = names.iter()
            .map(|(_, text)| Pattern::parse(text).unwrap_or_else(|| Pattern::parse("??").unwrap()))
            .collect();
        let (start, text) = image.text();
        let hits = sigscan::scan(text, &patterns).into_iter()
            .map(|found| found.into_iter().map(|at| start + at).collect())
            .collect();
        Self { image, names, hits }
    }

    pub(crate) fn hits(&self, name: &str) -> &[usize] {
        self.names.iter().position(|(n, _)| *n == name).map_or(&[], |index| &self.hits[index])
    }

    /// The single match of a required locator.
    pub(crate) fn unique(&self, name: &str) -> Found<usize> {
        match self.hits(name) {
            [only] => Ok(*only),
            [] => Err(format!("{name}: no match")),
            _ => Err(format!("{name}: more than one match")),
        }
    }

    /// An optional corroborator: used only when it matches exactly once.
    pub(crate) fn optional(&self, name: &str) -> Option<usize> {
        match self.hits(name) {
            [only] => Some(*only),
            _ => None,
        }
    }

    pub(crate) fn abs(&self, rva: usize) -> Found<usize> {
        self.image.abs(rva).ok_or_else(|| format!("operand at {rva:#x} is not inside the image"))
    }

    pub(crate) fn byte(&self, rva: usize) -> Found<usize> {
        self.image.u8(rva).ok_or_else(|| format!("{rva:#x} is unreadable"))
    }

    pub(crate) fn word(&self, rva: usize) -> Found<usize> {
        Ok(self.byte(rva)? | self.byte(rva + 1)? << 8)
    }

    pub(crate) fn dword(&self, rva: usize) -> Found<usize> {
        self.image.u32(rva).ok_or_else(|| format!("{rva:#x} is unreadable"))
    }

    /// A call or jump target must land in the module's code section.
    fn code(&self, what: &str, rva: usize, target: Option<usize>) -> Found<usize> {
        let target = target.ok_or_else(|| format!("no {what} at {rva:#x}"))?;
        let (start, text) = self.image.text();
        (start <= target && target < start + text.len()).then_some(target)
            .ok_or_else(|| format!("the {what} at {rva:#x} lands outside the module's code ({target:#x})"))
    }

    pub(crate) fn call(&self, rva: usize) -> Found<usize> {
        self.code("call", rva, self.image.call(rva))
    }

    pub(crate) fn jump(&self, rva: usize) -> Found<usize> {
        self.code("jmp", rva, self.image.jump(rva))
    }

    pub(crate) fn branch(&self, rva: usize) -> Found<usize> {
        self.image.short_branch(rva).ok_or_else(|| format!("no short branch at {rva:#x}"))
    }

    pub(crate) fn shape(&self, rva: usize, what: &str, pattern: &str) -> Found<()> {
        let pattern = Pattern::parse(pattern).ok_or_else(|| format!("{what}: bad pattern"))?;
        self.image.matches_at(rva, &pattern).then_some(()).ok_or_else(|| format!("{what}: unexpected code at {rva:#x}"))
    }

    /// The code at `rva` is exactly `body` (a whole, fixed function).
    pub(crate) fn body(&self, rva: usize, what: &str, body: &[u8]) -> Found<()> {
        (self.image.bytes(rva, body.len()) == Some(body)).then_some(())
            .ok_or_else(|| format!("{what}: unexpected code at {rva:#x}"))
    }

    /// The absolute operand at `rva` is a slot of the import address table.
    pub(crate) fn import_slot(&self, what: &str, rva: usize) -> Found<usize> {
        let slot = self.abs(rva)?;
        let (iat, len) = self.image.headers.iat;
        (slot >= iat && slot + 4 <= iat + len).then_some(slot).ok_or_else(|| format!("{what} import {slot:#x} is not in the IAT"))
    }

    /// A writable static: inside `.data` and dword-aligned.
    pub(crate) fn global(&self, what: &str, rva: usize) -> Found<usize> {
        (rva.is_multiple_of(4) && self.image.inside(".data", rva, 4)).then_some(rva)
            .ok_or_else(|| format!("{what} ({rva:#x}) is not an aligned .data global"))
    }

    /// The `push imm32` operand at `rva` names this exact C string.
    pub(crate) fn string(&self, rva: usize, text: &str) -> Found<()> {
        let at = self.abs(rva)?;
        self.image.c_string(at, text.as_bytes()).then_some(()).ok_or_else(|| format!("{rva:#x} does not push \"{text}\""))
    }
}


pub(crate) fn agree(what: &str, values: &[usize]) -> Found<usize> {
    let first = values[0];
    values.iter().all(|v| *v == first).then_some(first)
        .ok_or_else(|| format!("{what}: locators disagree {values:x?}"))
}

pub(crate) fn expect(what: &str, got: usize, want: usize) -> Found<()> {
    (got == want).then_some(()).ok_or_else(|| format!("{what} is {got:#x} in this build, expected {want:#x}"))
}

fn lobby(c: &Ctx) -> Found<Lobby> {
    let get = c.unique("lobby.get_error")?;
    let drain = c.unique("lobby.drain")?;
    let helper = c.unique("lobby.tcp_protocol_error")?;
    let mut operands = vec![c.abs(get + 1)?, c.abs(get + 0x13)?, c.abs(drain + 1)?,
        c.abs(drain + 0x1A)?, c.abs(drain + 0x45)?, c.abs(helper + 3)?];
    // These two compare lists of lobby error codes, which an update may
    // extend: they must agree when found once, but are not required.
    if let Some(clear) = c.optional("lobby.clear_retryable") {
        operands.push(c.abs(clear + 0xB)?);
    }
    let ptr = agree("LOBBY_PTR", &operands)?;
    c.global("LOBBY_PTR", ptr)?;
    // The getter calls the tcp protocol-error helper, and it is what main's
    // GetLobbyError thunk jumps to.
    expect("lobby.get_error helper call", c.call(get + 9)?, helper)?;
    if let Some(check) = c.optional("main.lobby_error_check") {
        let thunk = agree("main.lobby_error_check calls", &[c.call(check + 6)?, c.call(check + 0x1C)?, c.call(check + 0x2E)?])?;
        expect("LOBBY_GET_ERROR", c.jump(thunk)?, get)?;
    }
    Ok(Lobby { ptr, get_error: get })
}

fn diagnostics(c: &Ctx, lobby: &Lobby) -> Found<Diagnostics> {
    use crate::lobby as l;
    let get = lobby.get_error;
    let clear = c.optional("lobby.clear_retryable");
    let drain = c.unique("lobby.drain")?;
    let helper = c.unique("lobby.tcp_protocol_error")?;
    let reply_check = c.unique("lobby.reply_check")?;
    let ready = c.unique("lobby.socket_ready")?;
    let process = c.unique("tcp.process")?;
    let copy = c.unique("tcp.socket_error_copy")?;
    agree("LOBBY_PTR (diagnostic sites)", &[lobby.ptr, c.abs(reply_check + 0x14)?, c.abs(reply_check + 0x27)?, c.abs(ready + 0xA)?])?;
    for site in [drain + 0x13, drain + 0x3E, reply_check + 0xE] {
        expect("tcp_process call", c.call(site)?, process)?;
    }
    let predicate = c.call(ready + 0x23)?;
    c.shape(predicate, "socket-ready predicate", SOCKET_PREDICATE)?;
    let getter = c.call(helper + 0x1D)?;
    c.shape(getter, "protocol-error getter", PROTOCOL_ERROR_GETTER)?;

    let tcp = agree("lobby.tcp", &[c.dword(drain + 0xD)?, c.dword(drain + 0x38)?, c.dword(drain + 0x5E)?,
        c.dword(reply_check + 2)?, c.dword(ready + 0x1A)?, c.dword(helper + 0xF)?])?;
    let reply = agree("lobby.reply_state", &[c.dword(drain + 7)?, c.dword(drain + 0x23)?, c.dword(drain + 0x29)?,
        c.dword(drain + 0x4E)?, c.dword(drain + 0x54)?, c.dword(reply_check + 8)?, c.dword(reply_check + 0x1D)?, c.dword(reply_check + 0x2D)?])?;
    let mut errors = vec![c.dword(get + 0x19)?, c.dword(ready + 0x10)?];
    if let Some(clear) = clear {
        errors.extend([c.dword(clear + 0x11)?, c.dword(clear + 0x33)?]);
    }
    let error = agree("lobby.lobby_error", &errors)?;
    expect("lobby.tcp", tcp, l::LOBBY_TCP)?;
    expect("lobby.reply_state", reply, l::LOBBY_REPLY)?;
    expect("lobby.lobby_error", error, l::LOBBY_ERROR)?;

    let operation = agree("tcp.operation", &[c.dword(process + 0xE)?, c.dword(process + 0x3D)?, c.dword(copy + 0x22)?])?;
    let context = agree("tcp.context", &[c.dword(process + 0x14)?, c.dword(getter + 6)?])?;
    let state = agree("tcp.socket_state", &[c.dword(process + 0x57)?, c.dword(copy + 2)?, c.dword(predicate + 6)?])?;
    let socket_error = c.dword(copy + 0x11)?;
    let socket = c.dword(process + 0x31)?;
    expect("tcp socket object + 0x24", socket + 0x24, state)?;
    expect("tcp socket object + 0x28", socket + 0x28, socket_error)?;
    let protocol = agree("context.protocol_error", &[c.dword(getter + 0x10)?, c.dword(process + 0x66)?, c.dword(copy + 0x1B)?])?;
    expect("tcp.operation", operation, l::TCP_OPERATION)?;
    expect("tcp.context", context, l::TCP_CONTEXT)?;
    expect("tcp.socket_state", state, l::TCP_SOCKET_STATE)?;
    expect("tcp.socket_error", socket_error, l::TCP_SOCKET_ERROR)?;
    expect("context.protocol_error", protocol, l::CONTEXT_PROTOCOL_ERROR)?;
    expect("tcp operation limit", c.byte(process + 0x47)? + 1, l::OP_MAX as usize)?;

    // Request functions: each writes its literal operation code.
    let connect = c.unique("op.connect")?;
    let login = c.unique("op.login")?;
    let list = c.unique("op.char_list")?;
    let select = c.unique("op.char_select")?;
    let close = c.unique("op.close")?;
    agree("request operation field", &[operation, c.dword(connect + 7)?, c.dword(connect + 0x3D)?, c.dword(login + 0xB)?,
        c.dword(login + 0x64)?, c.dword(list + 0xC)?, c.dword(list + 0x67)?, c.dword(select + 0xC)?, c.dword(select + 0x71)?,
        c.dword(close + 7)?, c.dword(close + 0x2C)?])?;
    agree("lobby packet builder", &[c.call(login + 0x4F)?, c.call(list + 0x52)?, c.call(select + 0x5C)?])?;
    for (what, got, want) in [("connect", c.byte(connect + 0x41)?, l::OP_CONNECT), ("lobby login", c.byte(login + 0x68)?, l::OP_LOGIN),
        ("character list", c.byte(list + 0x6B)?, l::OP_CHARACTER_LIST), ("character selection", c.byte(select + 0x75)?, l::OP_CHARACTER_SELECT),
        ("close", c.byte(close + 0x30)?, l::OP_CLOSE)] {
        expect(&format!("{what} operation code"), got, want as usize)?;
    }
    // The connect request resolves its host through a relocated import call.
    let resolver = c.call(connect + 0x2B)?;
    c.shape(resolver, "connect host resolver", CONNECT_RESOLVER)?;
    c.import_slot("connect", resolver + 0x26)?;

    // The main game-state manager: its pump loop, init call and the lobby check.
    let pump = c.unique("main.pump_loop")?;
    let init_call = c.unique("main.init_call")?;
    let request = c.unique("main.request_state")?;
    let init = c.unique("main.init")?;
    let mut mains = vec![c.abs(pump + 7)?, c.abs(init_call + 2)?];
    if let Some(check) = c.optional("main.lobby_error_check") {
        mains.extend([c.abs(check + 2)?, c.abs(check + 0x18)?, c.abs(check + 0x2A)?]);
    }
    let main = agree("MAIN_PTR", &mains)?;
    c.global("MAIN_PTR", main)?;
    (main != lobby.ptr).then_some(()).ok_or("MAIN_PTR equals LOBBY_PTR")?;
    let step_fn = c.call(pump + 0xB)?;
    c.shape(step_fn, "main step", MAIN_STEP_FN)?;
    let tick = c.call(step_fn + 0x1A)?;
    c.shape(tick, "main tick", MAIN_TICK_FN)?;
    let init_fn = c.call(init_call + 6)?;
    c.shape(init_fn, "main init", MAIN_INIT_FN)?;
    expect("main.init body", init, init_fn + 0x13)?;
    let main_state = agree("main.state", &[c.dword(tick + 0x2F)?, c.dword(tick + 0x4A)?, c.dword(request + 0x34)?,
        c.dword(request + 0x54)?, c.dword(init + 0x6E)?, c.dword(init + 0x76)?])?;
    let main_step = agree("main.step", &[c.byte(request + 0x3A)?, c.byte(request + 0x51)?, c.byte(init + 5)?, c.byte(init_fn + 0x18)?])?;
    expect("main.state", main_state, l::MAIN_STATE)?;
    expect("main.step", main_step, l::MAIN_STEP)?;
    Ok(Diagnostics { main })
}

/// The lobby root its own accessors load. When the lobby group resolved too,
/// both must agree; when it did not, these accessors stand on their own.
fn own_root(c: &Ctx, what: &str, mut operands: Vec<usize>, lobby: Option<&Lobby>) -> Found<usize> {
    operands.extend(lobby.map(|l| l.ptr));
    let ptr = agree(what, &operands)?;
    c.global(what, ptr)
}

fn characters(c: &Ctx, lobby: Option<&Lobby>) -> Found<()> {
    use crate::lobby as l;
    let getter = c.unique("chars.content_getter")?;
    let record = c.unique("chars.record_reader")?;
    let cluster = c.unique("chars.count_cluster")?;
    let builder = c.unique("chars.menu_builder")?;
    let collector = c.unique("chars.collector")?;
    let tail = c.unique("chars.menu_tail")?;
    // The menu builder's count call goes through a jmp thunk to the count getter.
    let count_getter = c.jump(c.call(builder + 0xD)?)?;
    c.shape(count_getter, "character count getter", COUNT_GETTER)?;
    expect("character count getter", count_getter, cluster)?;
    own_root(c, "LOBBY_PTR (character table)",
        vec![c.abs(getter + 0xF)?, c.abs(record + 0x11)?, c.abs(cluster + 1)?, c.abs(cluster + 0x1F)?], lobby)?;
    let content = |rva: usize| -> Found<usize> { c.dword(rva)?.checked_sub(4).ok_or_else(|| "character entries start below 4".to_string()) };
    let count = agree("lobby.character_count", &[c.dword(cluster + 7)?, content(getter + 0x19)?, content(record + 0x3A)?])?;
    expect("lobby.character_count", count, l::LOBBY_CHARACTER_COUNT)?;
    expect("lobby.characters", count + 4, l::LOBBY_CHARACTERS)?;
    // The record reader's fields relative to the count: what slot 177 decodes.
    expect("character id (low)", c.dword(record + 0x27)?, count + 8)?;
    expect("character world", c.dword(record + 0x34)?, count + 0xA)?;
    expect("character id (high)", c.dword(record + 0x1B)?, count + 0xF)?;
    // The 0x8c stride is the literal lea/sub/lea chain in all three accessors.
    let wrapper = c.call(collector + 0x4C)?;
    c.shape(wrapper, "character entry wrapper", ENTRY_WRAPPER)?;
    expect("character entry wrapper target", c.call(wrapper + 5)?, getter)?;
    let slots = agree("character slots", &[c.byte(collector + 0x5A)?, c.byte(collector + 2)? / 4, c.byte(tail + 3)?, c.byte(tail + 0x18)?])?;
    expect("character slots", slots, l::LOBBY_CHARACTER_SLOTS)?;
    Ok(())
}

/// For each method, the `this` of every `mov ecx, imm32; call method` pair in
/// one pass. A method needs at least one such caller and they must all agree.
fn objects_of(c: &Ctx, methods: &[usize]) -> Found<Vec<usize>> {
    let (start, text) = c.image.text();
    let mut found = vec![None; methods.len()];
    for at in 0..text.len().saturating_sub(10) {
        if text[at] != 0xB9 || text[at + 5] != 0xE8 {
            continue;
        }
        let Some(index) = c.image.call(start + at + 5).and_then(|target| methods.iter().position(|m| *m == target)) else {
            continue;
        };
        let this = c.abs(start + at + 1)?;
        match found[index] {
            Some(known) if known != this => return Err(format!("callers of {:#x} disagree on their object", methods[index])),
            _ => found[index] = Some(this),
        }
    }
    found.into_iter().zip(methods)
        .map(|(this, method)| this.ok_or_else(|| format!("no mov ecx caller of {method:#x}")))
        .collect()
}

fn menus(c: &Ctx) -> Found<Menus> {
    use crate::auto_login as a;
    // Focused menu: the loop's compare, and the UI manager field behind two methods.
    let loop_ = c.unique("menu.focus_loop")?;
    let usable = c.unique("menu.focus_usable")?;
    let set = c.unique("menu.set_focus")?;
    let manager = agree("UI manager", &objects_of(c, &[usable, set])?)?;
    c.global("UI manager", manager)?;
    let focus = agree("FOCUS", &[c.abs(loop_ + 0x10)?, manager + c.byte(usable + 2)?, manager + c.byte(set + 0xA)?])?;

    let license_run = c.unique("menu.license_run")?;
    let license_new = c.unique("menu.license_factory")?;
    c.string(license_run + 5, "menu    ptc8lice")?;
    c.string(license_run + 0x2A, "menu    ptc8lice")?;
    let license = agree("LICENSE", &[c.abs(license_run + 0x14)?, c.abs(license_run + 0x35)?, c.abs(license_new + 0x28)?, c.abs(license_new + 0x30)?])?;

    let title_new = c.unique("menu.title_factory")?;
    let title_run = c.unique("menu.title_run")?;
    let title_fields = c.unique("menu.title_fields")?;
    c.string(title_run + 4, "menu    loby2win")?;
    let title = agree("TITLE", &[c.abs(title_fields + 2)?, c.abs(title_fields + 0x1B)?, c.abs(title_new + 0x31)?, c.abs(title_new + 0x39)?, c.abs(title_run + 0x44)?])?;

    // Each strict literal of these sites is a field checked against
    // auto_login's constants below, so the wildcard locators suffice.
    let marked = c.unique("menu.play_row_marked")?;
    let delete_mode = c.unique("menu.help_delete_mode")?;
    let play_mode = c.unique("menu.help_play_mode")?;
    let characters_new = c.unique("menu.characters_factory")?;
    c.string(delete_mode + 1, "menu    dbnamese")?;
    c.string(play_mode + 0x22, "menu    dbnamese")?;
    let characters = agree("CHARACTERS", &[c.abs(marked + 0x10)?, c.abs(delete_mode + 0xB)?, c.abs(play_mode + 1)?, c.abs(characters_new + 0x2A)?, c.abs(characters_new + 0x32)?])?;
    let rows = 15 * a::ROW_SIZE + a::ROW_MARKED + 4;
    (c.dword(characters_new + 1)? >= rows).then_some(()).ok_or("character controller is smaller than its rows")?;

    // The login and delete flows each set help mode through [HELP] twice.
    let help = agree("HELP", &[c.abs(play_mode + 0x32)?, c.abs(play_mode + 0x3F)?, c.abs(delete_mode + 0x1A)?, c.abs(delete_mode + 0x27)?])?;
    expect("help mode field", agree("help mode field", &[c.byte(play_mode + 0x45)?, c.byte(delete_mode + 0x2D)?])?, a::HELP_MODE)?;
    expect("login help mode", c.byte(play_mode + 0x46)?, a::HELP_LOGIN as usize)?;
    expect("delete help mode", c.byte(delete_mode + 0x2E)?, a::HELP_DELETE as usize)?;

    let plain = c.unique("menu.confirm_plain")?;
    let confirm_new = c.unique("menu.confirm_factory")?;
    let fields = c.unique("menu.confirm_fields")?;
    c.string(plain + 0x3F, "menu    ptc6yesn")?;
    let confirm = agree("CONFIRM", &[c.abs(fields + 1)?, c.abs(fields + 0x29)?, c.abs(fields + 0x33)?, c.abs(fields + 0x40)?,
        c.abs(plain + 1)?, c.abs(plain + 0x29)?, c.abs(plain + 0x36)?, c.abs(plain + 0x48)?, c.abs(confirm_new + 0x30)?, c.abs(confirm_new + 0x38)?])?;
    (c.dword(confirm_new + 1)? > a::CONFIRM_VARIANT).then_some(()).ok_or("confirmation controller is smaller than its variant field")?;

    expect("confirmation kind field", agree("confirmation kind field", &[c.byte(fields + 0x17)?, c.byte(fields + 0x30)?])?, a::CONFIRM_KIND)?;
    expect("confirmation cursor field", agree("confirmation cursor field", &[c.byte(fields + 0x23)?, c.byte(fields + 0x4A)?])?, a::CONFIRM_CURSOR)?;
    expect("confirmation variant field", c.dword(fields + 0x1D)?, a::CONFIRM_VARIANT)?;
    // An existing character's row leads to the named yes/no with kind 13.
    let login_call = c.unique("menu.login_confirm_call")?;
    expect("login confirmation call", c.call(login_call + 0xD)?, fields)?;
    expect("login confirmation kind", c.byte(login_call + 0xC)?, a::CONFIRM_LOGIN as usize)?;

    let cursor_fields = c.unique("menu.cursor_fields")?;
    let selected = agree("SELECTED", &[c.abs(marked + 1)?, c.abs(cursor_fields + 0x12)?, c.abs(delete_mode + 0x11)?])?;
    expect("controller menu field", agree("controller menu field", &[c.byte(cursor_fields + 2)?, c.byte(cursor_fields + 0x2A)?, c.byte(title_fields + 0x2A)?])?, a::CONTROLLER_MENU)?;
    expect("menu cursor field", c.byte(cursor_fields + 0xF)?, a::MENU_CURSOR)?;
    expect("menu disabled field", agree("menu disabled field", &[c.byte(cursor_fields + 0x30)?, c.byte(title_fields + 0x2D)?])?, a::MENU_DISABLED)?;
    expect("row occupied field", agree("row occupied field", &[c.byte(cursor_fields + 0x22)?, c.byte(login_call + 2)?])?, a::ROW_OCCUPIED)?;
    expect("row marked field", c.dword(marked + 0x21)?, a::ROW_MARKED)?;
    let find = c.unique("menu.find_by_name")?;
    expect("menu resource field", c.byte(find + 2)?, a::MENU_RESOURCE)?;
    expect("menu name field", c.byte(find + 9)?, a::MENU_NAME)?;

    let create = c.unique("menu.window_create")?;
    let destroy = c.unique("menu.window_destroy")?;
    c.string(create + 3, "FFXiClass")?;
    c.string(destroy + 0xF, "FFXiClass")?;
    let window = agree("WINDOW", &[c.abs(create + 0x15)?, c.abs(destroy + 2)?, c.abs(destroy + 0x1F)?])?;

    for (what, rva) in [("FOCUS", focus), ("LICENSE", license), ("TITLE", title), ("CHARACTERS", characters), ("HELP", help),
        ("CONFIRM", confirm), ("SELECTED", selected), ("WINDOW", window)] {
        c.global(what, rva)?;
    }
    // The sites auto_login re-reads live: the focus compare, the selected-row
    // store, the login help mode, the window read and the help 0x11 store.
    let check_help11 = c.unique("menu.check_help11")?;
    expect("help 0x11 check operand", c.abs(check_help11 + 2)?, help)?;
    let sites = MenuSites { focus: loop_ + 0xE, selected: cursor_fields + 0xC, help: play_mode + 0x30, window: destroy,
        confirm13: login_call + 5, help11: check_help11 + 6 };
    Ok(Menus { focus, license, title, characters, help, confirm, selected, window, sites })
}

fn friends(c: &Ctx, lobby: Option<&Lobby>) -> Found<Friends> {
    use crate::friend_locations as f;
    // Row cache getter: idx >= capacity ? 0 : [LOBBY] + idx * 0x100 + rows.
    let cache = c.unique("friend.cache_getter")?;
    let cache_caller = c.unique("friend.cache_caller")?;
    expect("friend cache getter call", c.call(cache_caller + 8)?, cache)?;
    let capacity = agree("friend row capacity", &[c.word(cache + 7)?, c.word(cache_caller + 3)?])?;
    (capacity >= crate::proto::FRIEND_SLOTS).then_some(()).ok_or("friend row capacity is below the POL catalog")?;
    // The lobby root, from this feature's own code (agreed below).
    let root = c.abs(cache + 0x10)?;
    expect("friend row stride", 1usize.checked_shl(c.byte(cache + 0x19)? as u32).unwrap_or(0), f::ROW_SIZE)?;

    // The display routine and its pieces, tied together by its own branches.
    let head = c.unique("friend.display_head")?;
    let area = c.unique("friend.display_area")?;
    let visible_area = c.unique("friend.display_area_visible")?;
    let display_name = c.unique("friend.display_name")?;
    expect("display head branch", c.branch(head + 0x30)?, area)?;
    expect("display area branch", c.branch(area + 0x25)?, visible_area + 3)?;
    expect("display hidden-name branch", c.branch(area + 0x29)?, display_name)?;
    expect("display hidden-name branch", c.branch(area + 0x39)?, display_name)?;

    let name_getter = c.unique("friend.name_getter")?;
    expect("display name getter", c.call(display_name + 1)?, name_getter)?;
    expect("display name getter", c.call(display_name + 0xE)?, name_getter)?;
    let visible = c.call(area + 1)?;
    c.shape(visible, "visible predicate", VISIBLE_FN)?;
    let detail = c.dword(area + 0x10)?;
    let copied = c.dword(area + 7)? * 4;
    let item = c.byte(area + 0xD)?;
    let flags = agree("friend flags", &[c.dword(name_getter + 6)?, c.dword(visible + 6)?, detail])?;
    let name = agree("friend name", &[c.dword(name_getter + 0xE)?, (detail + c.byte(display_name + 0x17)?).checked_sub(item).ok_or("name copy underflows")?])?;
    let privacy = c.dword(visible + 0x12)?;
    let zone_at = agree("friend zone item", &[c.byte(area + 0x36)?, c.byte(visible_area + 0x12)?])?;
    let zone = (detail + zone_at).checked_sub(item).ok_or("zone copy underflows")?;
    (detail <= zone && zone + 2 <= detail + copied && zone + 2 <= f::ROW_SIZE).then_some(()).ok_or("zone is outside the copied detail")?;
    if let Some(getter) = c.optional("friend.privacy_getter") {
        expect("privacy getter flags", c.dword(getter + 6)?, flags)?;
        expect("privacy getter word", c.dword(getter + 0x10)?, privacy)?;
    }
    if let Some(getter) = c.optional("friend.zone_getter") {
        expect("zone getter", c.dword(getter + 7)?, zone)?;
    }
    expect("friend detail", detail, f::DETAIL)?;
    expect("friend detail length", copied, f::DETAIL_LEN)?;
    expect("friend name", name, f::NAME)?;
    expect("friend privacy", privacy, f::PRIVACY)?;
    expect("friend hidden bit", c.byte(visible + 0x18)?, f::HIDDEN_BIT)?;
    expect("friend zone", zone, f::ZONE)?;

    // My own zone, compared before an area name is shown.
    let own_zone = c.abs(area + 0x1F)?;
    c.global("OWN_ZONE", own_zone)?;
    let field = agree("own zone field", &[c.dword(area + 0x2D)?, c.dword(visible_area + 9)?])?;
    expect("own zone field", field, f::OWN_ZONE_FIELD)?;
    let area_fn = c.unique("friend.area_fn")?;
    expect("display area call", c.call(area + 0x43)?, area_fn)?;
    expect("display area call", c.call(visible_area + 0x1F)?, area_fn)?;
    if let Some(other) = c.optional("friend.other_area") {
        expect("other list own zone", c.abs(other + 2)?, own_zone)?;
        expect("other list own zone field", c.dword(other + 0xC)?, field)?;
        expect("other list area call", c.call(other + 0x22)?, area_fn)?;
    }

    // Area names: lookup(4, zone), else lookup(9, 0x40), through one thunk.
    let lookup = c.unique("friend.group_lookup")?;
    expect("area lookup call", c.image.through_thunk(c.call(area_fn + 0x19)?), lookup)?;
    expect("fallback lookup jump", c.image.through_thunk(c.jump(area_fn + 0x3A)?), lookup)?;
    let area_group = c.byte(area_fn + 0x18)?;
    let fallback_index = c.dword(area_fn + 0x2E)?;
    let fallback_group = c.dword(area_fn + 0x36)?;
    expect("area group", area_group, f::AREA_GROUP)?;
    expect("fallback group", fallback_group, f::FALLBACK_GROUP)?;
    expect("fallback index", fallback_index, f::FALLBACK_INDEX)?;
    let table = c.abs(lookup + 0xB)?;
    let count = c.byte(lookup + 0x14)?;
    let legacy = c.abs(lookup + 0x1A)?;
    expect("group table objects", c.abs(lookup + 0x28)?, table + 4)?;
    expect("group table objects", c.abs(lookup + 0x40)?, table + 4)?;
    (table % 4 == 0 && c.image.inside(".data", table, count * 8) && c.image.bytes(table, count * 8).is_some())
        .then_some(()).ok_or("group table is not in initialized .data")?;
    // Every id is read, so each used id must appear exactly once. Only the
    // used entries' objects are checked: the game's own lookup skips null
    // objects, and an unrelated group's object never matters here.
    let ids = (0..count).map(|entry| c.dword(table + entry * 8)).collect::<Found<Vec<_>>>()?;
    let group = |id: usize| -> Found<Group> {
        let [entry] = ids.iter().enumerate().filter(|(_, g)| **g == id).map(|(entry, _)| entry).collect::<Vec<_>>()[..] else {
            return Err(format!("group {id} is not in the group table exactly once"));
        };
        let object = c.dword(table + entry * 8 + 4)?.checked_sub(c.image.base)
            .ok_or_else(|| format!("group {id} object is null or outside the image"))?;
        c.global(&format!("group {id} object"), object)?;
        Ok(Group { id, entry, object })
    };
    let (area_res, fallback_res) = (group(area_group)?, group(fallback_group)?);
    c.global("LEGACY_TABLE", legacy)?;
    (area_group.max(fallback_group) < f::LEGACY_SLOTS && fallback_index < 0x1000).then_some(()).ok_or("resource group or index out of range")?;
    if let Some(free) = c.optional("friend.legacy_free") {
        agree("legacy table (free)", &[legacy, c.abs(free + 8)?, c.abs(free + 0x1C)?])?;
    }
    if let Some(clear) = c.optional("friend.legacy_clear") {
        expect("legacy table (clear)", c.abs(clear + 9)?, legacy)?;
        expect("legacy table slots", c.byte(clear + 2)?, f::LEGACY_SLOTS)?;
    }

    // Managed resource objects (102578b0 and its callees).
    let managed = c.unique("friend.managed_lookup")?;
    let present = c.call(lookup + 0x30)?;
    c.shape(present, "resource present check", RESOURCE_PRESENT)?;
    let wrap = c.call(lookup + 0x45)?;
    c.shape(wrap, "resource variant-0 wrapper", RESOURCE_WRAP)?;
    expect("resource wrapper target", c.call(wrap + 7)?, managed)?;
    let fixed = c.call(managed + 0x2B)?;
    c.shape(fixed, "fixed resource entry", RESOURCE_FIXED)?;
    let entry = c.call(managed + 0x3C)?;
    c.shape(entry, "resource entry", RESOURCE_ENTRY)?;
    let text = c.call(managed + 0x42)?;
    c.shape(text, "resource text", RESOURCE_TEXT)?;
    for (what, values, want) in [
        ("resource header", vec![c.byte(present + 2)?, c.byte(managed + 2)?, c.byte(entry + 2)?], f::RES_HEADER),
        ("resource data", vec![c.byte(managed + 0x22)?, c.byte(entry + 0x23)?, c.byte(entry + 0x32)?], f::RES_DATA),
        ("resource count", vec![c.byte(managed + 0xA)?, c.byte(entry + 0xA)?], f::RES_COUNT),
        ("resource kind", vec![c.byte(managed + 0x18)?], f::RES_KIND),
        ("fixed resource kind", vec![c.byte(managed + 0x19)?], f::RES_FIXED_KIND),
        ("resource variants", vec![c.byte(managed + 0x1F)?], f::RES_VARIANTS),
        ("resource indexed flag", vec![c.byte(entry + 0x15)?], f::RES_INDEXED),
        ("resource index", vec![c.byte(entry + 0x1C)?], f::RES_INDEX),
        ("resource stride", vec![c.byte(entry + 0x2B)?], f::RES_STRIDE),
        ("fixed entry presence", vec![c.byte(fixed + 0x25)?], f::FIXED_PRESENT),
        ("text entry reserved", vec![c.byte(text + 0x1E)?], f::TEXT_RESERVED),
        ("text entry offset", vec![c.byte(text + 0x29)?], f::TEXT_OFFSET),
        ("text marker", vec![c.byte(text + 0x2F)?], f::TEXT_MARKER),
        ("text string", vec![c.byte(text + 0x37)?], f::TEXT_STRING),
    ] {
        expect(what, agree(what, &values)?, want)?;
    }
    // Legacy tables (101530c0/10153090).
    let legacy_lookup = c.unique("friend.legacy_lookup")?;
    expect("legacy lookup jump", c.jump(lookup + 0x53)?, legacy_lookup)?;
    let small = c.jump(legacy_lookup + 0xF)?;
    c.shape(small, "small legacy lookup", LEGACY_SMALL)?;
    expect("legacy kind field", c.byte(legacy_lookup + 7)?, f::LEGACY_KIND)?;
    expect("legacy small kinds", c.byte(legacy_lookup + 8)?, f::LEGACY_SMALL_BELOW)?;
    expect("legacy count", agree("legacy count", &[c.byte(legacy_lookup + 0x1E)?, c.byte(small + 0xE)?])?, f::LEGACY_COUNT)?;
    expect("legacy entries", agree("legacy entries", &[c.byte(legacy_lookup + 0x2C)?, c.byte(legacy_lookup + 0x33)?,
        c.byte(small + 0x17)?, c.byte(small + 0x1D)?])?, f::LEGACY_ENTRIES)?;

    // Lobby root: own world, rows and the world table.
    let own_world = c.unique("friend.own_world_check")?;
    let same_world = c.unique("friend.same_world")?;
    let refresh = c.unique("friend.row_refresh")?;
    let world_name = c.unique("friend.world_name")?;
    let world_label = c.unique("friend.world_label")?;
    expect("display same-world call", c.call(head + 0x26)?, same_world)?;
    expect("display world-label call", c.call(head + 0x44)?, world_label)?;
    expect("world label lookup", c.call(world_label + 0xE)?, world_name)?;
    own_root(c, "LOBBY_PTR (friend root)", vec![root, c.abs(own_world + 1)?, c.abs(same_world + 0x29)?, c.abs(refresh + 1)?,
        c.abs(refresh + 0x8F)?, c.abs(world_name + 3)?], lobby)?;
    let rows = agree("friend rows", &[c.dword(cache + 0x1D)?, c.dword(refresh + 8)?])?;
    expect("friend rows", rows, f::ROWS)?;
    expect("own world", agree("own world", &[c.dword(own_world + 8)?, c.dword(same_world + 0x37)?, c.dword(refresh + 0x9C)?])?, f::OWN_WORLD)?;
    let entries = c.dword(world_name + 0x1A)?;
    expect("world count", c.dword(world_name + 0xC)?, f::WORLD_COUNT)?;
    expect("world entries", entries, f::WORLD_ENTRIES)?;
    expect("world stride", c.byte(world_name + 0x25)?, f::WORLD_STRIDE)?;
    expect("world name", c.dword(world_name + 0x39)?.checked_sub(entries).ok_or("world name underflows")?, f::WORLD_NAME)?;
    (f::WORLD_STRIDE == f::WORLD_NAME + 16 && rows + capacity * f::ROW_SIZE <= f::WORLD_COUNT && f::WORLD_COUNT < entries)
        .then_some(()).ok_or("lobby root layout overlaps")?;

    // Row fields: in use, character id (two sources), completed.
    let mut in_use = vec![c.dword(refresh + 0x20)?];
    if let Some(getter) = c.optional("friend.in_use") {
        in_use.push(c.dword(getter + 6)?);
    }
    if let Some(scan) = c.optional("friend.row_scan") {
        expect("row scan LOBBY_PTR", c.abs(scan + 2)?, root)?;
        expect("row scan rows", c.dword(scan + 9)?, rows)?;
        in_use.push(c.dword(scan + 0x1D)?);
    }
    expect("row in-use flag", agree("row in-use flag", &in_use)?, f::IN_USE)?;
    let reply = c.unique("friend.reply")?;
    let copy = c.call(reply + 0x54)?;
    c.shape(copy, "row detail copy", ROW_DETAIL_COPY)?;
    expect("row detail copy LOBBY_PTR", c.abs(copy + 7)?, root)?;
    let reply_detail = c.dword(copy + 0x17)?.checked_sub(rows).ok_or("row detail underflows")?;
    expect("row detail", reply_detail, detail)?;
    expect("row detail length", c.byte(copy + 0x1C)? * 4, copied)?;
    let char_id = agree("row character id", &[c.dword(refresh + 0xC2)?.checked_sub(rows).ok_or("character id underflows")?,
        detail + c.byte(reply + 0x1C)?])?;
    expect("row character id", char_id, f::CHAR_ID)?;
    let mark = c.unique("friend.mark_complete")?;
    expect("mark complete LOBBY_PTR", c.abs(mark + 7)?, root)?;
    let completed = agree("row completed", &[c.dword(mark + 0x11)?.checked_sub(rows).ok_or("completed underflows")?, c.dword(same_world + 8)?])?;
    expect("row completed", completed, f::COMPLETED)?;
    expect("selected character world", c.byte(same_world + 0x26)?, f::CHARACTERS + f::CHARACTER_WORLD)?;
    (detail <= flags && flags < name && name + 16 <= char_id && char_id < zone && zone < privacy && privacy < completed && completed < f::ROW_SIZE)
        .then_some(()).ok_or("friend row fields overlap")?;
    expect("POL record length", detail, crate::proto::FRIEND_RECORD_LEN)?;

    Ok(Friends {
        lobby: root,
        cache_getter: cache,
        name_getter,
        display_area: area,
        area_fn,
        own_zone,
        group_table: table,
        legacy_table: legacy,
        area: area_res,
        fallback: fallback_res,
        fallback_index,
    })
}

/// Key input: the client's own first-key-down path names the input bank and
/// its three handlers; the keyboard controller's frame names the controller
/// global and the GetKeyboardState call whose return the hook acts on.
fn input(c: &Ctx) -> Found<Input> {
    use crate::auto_login::KEYBOARD_BUFFER;
    // 10005ce3: mov ecx,INPUT; call is_down; test al,al; mov ecx,INPUT; push esi;
    // jne; call pressed; push esi; mov ecx,INPUT; call edge; push esi; mov ecx,INPUT; call repeat.
    let key_down = c.unique("input.key_down")?;
    let pressed = c.call(key_down + 0x14)?;
    let edge = c.call(key_down + 0x1F)?;
    let repeat = c.call(key_down + 0x2A)?;
    c.body(pressed, "pressed key handler", &KEY_PRESSED)?;
    c.body(edge, "edge key handler", &KEY_EDGE)?;
    c.body(repeat, "repeat key handler", &KEY_REPEAT)?;
    // Every `mov ecx, imm32; call edge/repeat` in the image passes this object.
    let mut operands = vec![c.abs(key_down + 1)?, c.abs(key_down + 0xD)?, c.abs(key_down + 0x1B)?, c.abs(key_down + 0x26)?];
    operands.extend(objects_of(c, &[edge, repeat])?);
    let input = agree("INPUT", &operands)?;
    c.global("INPUT", input)?;
    c.image.inside(".data", input, KEY_BANK).then_some(()).ok_or("INPUT bank runs past .data")?;

    // 10012875: mov ecx,[KEYBOARD]; call frame (10005a40); call ...; mov ecx,[KEYBOARD]; call ...
    let controller_call = c.unique("input.controller_call")?;
    let keyboard = agree("KEYBOARD", &[c.abs(controller_call + 2)?, c.abs(controller_call + 0x12)?])?;
    c.global("KEYBOARD", keyboard)?;
    let frame_fn = c.call(controller_call + 6)?;
    c.shape(frame_fn, "keyboard frame", KEYBOARD_FRAME_FN)?;
    // 10005e2e: mov esi,[this]; lea ecx,[esi+0x298]; push ecx; call [GetKeyboardState].
    let frame = c.unique("input.keyboard_frame")?;
    expect("keyboard frame this", c.byte(frame + 3)?, c.byte(frame_fn + 0xF)?)?;
    expect("keyboard buffer", c.dword(frame + 6)?, KEYBOARD_BUFFER)?;
    let iat_slot = c.import_slot("keyboard state", frame + 0xD)?;
    // Both the key-down path and the import call are in that frame method.
    (frame_fn < key_down && key_down < frame && frame < frame_fn + KEYBOARD_FRAME_SPAN).then_some(())
        .ok_or("the key-down path and keyboard import call are not in the keyboard frame")?;
    Ok(Input { input, pressed, edge, repeat, keyboard, iat_slot, frame_call: frame + 0xB, controller_call,
        buffer_site: frame + 4, key_down })
}

/// The scan's own results, before any comparison with a known build.
#[derive(Debug)]
pub(crate) struct Scanned {
    pub lobby: Found<Lobby>,
    pub diagnostics: Found<Diagnostics>,
    pub characters: Found<()>,
    pub menus: Found<Menus>,
    pub friends: Found<Friends>,
    pub input: Found<Input>,
    pub shutdown: Found<Shutdown>,
    pub controller: Found<Controller>,
}

fn failure<T>(reason: &str) -> Found<T> {
    Err(reason.to_string())
}

impl Scanned {
    /// Every group refused for one reason.
    fn failed(reason: &str) -> Self {
        Self { lobby: failure(reason), diagnostics: failure(reason), characters: failure(reason), menus: failure(reason),
            friends: failure(reason), input: failure(reason), shutdown: failure(reason), controller: failure(reason) }
    }
}

fn needs<T, U>(group: &Found<T>, what: &str, then: impl FnOnce(&T) -> Found<U>) -> Found<U> {
    match group {
        Ok(value) => then(value),
        Err(reason) => Err(format!("needs {what}: {reason}")),
    }
}

pub(crate) fn scan(image: &Image) -> Scanned {
    let (relocated, stale) = image.import_calls();
    if relocated < MIN_IMPORT_CALLS || stale > 0 {
        const REASON: &str = "the module's code is not fully relocated for its load address";
        return Scanned::failed(REASON);
    }
    let patterns: Vec<(&str, &str)> = PATTERNS.iter().chain(controls::PATTERNS).copied().collect();
    let c = Ctx::new(image, &patterns);
    let lobby = lobby(&c);
    let input = input(&c);
    let controller = controls::controller(&c).and_then(|found| match &input {
        // Both groups name the keyboard controller global: they must agree.
        Ok(keys) => expect("controller KEYBOARD (key input)", found.keyboard, keys.keyboard).map(|_| found),
        Err(_) => Ok(found),
    });
    Scanned {
        diagnostics: needs(&lobby, "LOBBY_PTR", |l| diagnostics(&c, l)),
        // Each finds the lobby root in its own code, so a change that only
        // breaks the lobby group's locators does not switch these off.
        characters: characters(&c, lobby.as_ref().ok()),
        menus: menus(&c),
        friends: friends(&c, lobby.as_ref().ok()),
        input,
        shutdown: controls::shutdown(&c),
        controller,
        lobby,
    }
}


#[derive(Debug)]
pub(crate) struct Resolution {
    pub base: usize,
    pub timestamp: u32,
    pub size: usize,
    pub known: bool,
    pub lobby: Found<Lobby>,
    pub diagnostics: Found<Diagnostics>,
    pub characters: Found<()>,
    pub menus: Found<Menus>,
    pub friends: Found<Friends>,
    pub input: Found<Input>,
    pub shutdown: Found<Shutdown>,
    pub controller: Found<Controller>,
}

fn checked<T: Copy + PartialEq + Debug>(what: &str, scanned: Found<T>, table: Option<&T>) -> Found<T> {
    match (scanned?, table) {
        (value, Some(known)) if value != *known => {
            Err(format!("{what}: scan {value:x?} disagrees with this build's known values {known:x?}"))
        }
        (value, _) => Ok(value),
    }
}

impl Resolution {
    fn new(base: usize, timestamp: u32, size: usize, known: bool, s: Scanned) -> Self {
        let Scanned { lobby, diagnostics, characters, menus, friends, input, shutdown, controller } = s;
        Self { base, timestamp, size, known, lobby, diagnostics, characters, menus, friends, input, shutdown, controller }
    }

    pub(crate) fn unsupported(base: usize, reason: &str) -> Self {
        Self::new(base, 0, 0, false, Scanned::failed(&format!("unsupported module: {reason}")))
    }
}

/// Resolves an image: the scan, then this build's table when it is known.
pub(crate) fn resolve(image: &Image) -> Resolution {
    let known = KNOWN.iter().copied().find(|k| k.timestamp == image.headers.timestamp && k.size == image.headers.size);
    let s = scan(image);
    Resolution::new(image.base, image.headers.timestamp, image.headers.size, known.is_some(), Scanned {
        lobby: checked("lobby", s.lobby, known.map(|k| &k.lobby)),
        diagnostics: checked("diagnostics", s.diagnostics, known.map(|k| &k.diagnostics)),
        characters: s.characters,
        menus: checked("menus", s.menus, known.map(|k| &k.menus)),
        friends: checked("friends", s.friends, known.map(|k| &k.friends)),
        input: checked("key input", s.input, known.map(|k| &k.input)),
        shutdown: checked("background shutdown", s.shutdown, known.map(|k| &k.shutdown)),
        controller: checked("controller settings", s.controller, known.map(|k| &k.controller)),
    })
}

fn report(r: &Resolution, codeview: Option<u32>, elapsed: Duration) {
    log!("game layout: FFXiMain {:08X} size {:#x} at {:#x}, {} build, resolved in {:.1} ms{}",
        r.timestamp, r.size, r.base, if r.known { "known" } else { "unknown" }, elapsed.as_secs_f64() * 1000.0,
        match codeview { Some(stamp) if stamp != r.timestamp => format!(" (CodeView stamp {stamp:08X} differs)"), _ => String::new() });
    // `table`: the scan reproduced this known build's table.
    let line = |name: &str, error: Option<&String>, table: bool| match error {
        None if table => log!("game layout: {name} resolved (signature scan, matches the known-build table)"),
        None => log!("game layout: {name} resolved (signature scan)"),
        Some(reason) => log!("game layout: {name} unavailable: {reason}"),
    };
    line("lobby", r.lobby.as_ref().err(), r.known);
    line("lobby diagnostics", r.diagnostics.as_ref().err(), r.known);
    // Only offsets, all checked against the readers' constants: no table entry.
    line("lobby characters", r.characters.as_ref().err(), false);
    line("login menus", r.menus.as_ref().err(), r.known);
    line("friend locations", r.friends.as_ref().err(), r.known);
    line("key input", r.input.as_ref().err(), r.known);
    line("background shutdown", r.shutdown.as_ref().err(), r.known);
    line("controller settings", r.controller.as_ref().err(), r.known);
}

/// FFXiMain.dll's base in this process, or 0.
fn module_base() -> usize {
    crate::win::module_base("FFXiMain.dll")
}

/// Resolves the module loaded at `base` (no cache). `NotReady`: try later.
pub(crate) fn resolve_module(base: usize) -> Result<Resolution, CaptureError> {
    let started = Instant::now();
    let resolution = match Image::capture(base) {
        Ok(image) => {
            let resolution = resolve(&image);
            report(&resolution, image.codeview_timestamp(), started.elapsed());
            resolution
        }
        Err(CaptureError::NotReady) => return Err(CaptureError::NotReady),
        Err(CaptureError::Invalid(reason)) => {
            let resolution = Resolution::unsupported(base, reason);
            log!("game layout: FFXiMain at {base:#x} is unsupported: {reason}");
            resolution
        }
    };
    Ok(resolution)
}

/// The current FFXiMain mapping's layout. Each game maps FFXiMain afresh
/// (game_modules), so this is per mapping: [`forget`] drops it once that
/// mapping is gone. A superseded layout stays allocated (a few hundred bytes
/// per game) because a worker of the ended game may still hold its reference.
static RESOLVED: Mutex<Option<&'static Resolution>> = Mutex::new(None);

/// This mapping's FFXiMain layout, resolved once and then cached. `None` while
/// FFXiMain.dll is not loaded or is still initializing: poll again later.
pub(crate) fn ffximain() -> Option<&'static Resolution> {
    let base = module_base();
    if base == 0 {
        return None;
    }
    let mut cached = RESOLVED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = *cached {
        // Another image at a new base without a release is never re-resolved here.
        return (r.base == base).then_some(r);
    }
    let resolution = resolve_module(base).ok()?;
    let resolved: &'static Resolution = Box::leak(Box::new(resolution));
    *cached = Some(resolved);
    drop(cached);
    // The startup self-check also resolves Hook.dll, which may wait on a
    // busy loader: never on the caller's (possibly the game's) thread.
    let _ = std::thread::Builder::new().name("crystal-feature-check".into())
        .spawn(move || crate::feature_check::report(resolved));
    Some(resolved)
}

/// FFXiMain.dll was unloaded after its game: the next mapping resolves anew.
pub(crate) fn forget() {
    *RESOLVED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_and_shape_parses_and_names_are_unique() {
        for (name, text) in PATTERNS.iter().chain(controls::PATTERNS) {
            assert!(Pattern::parse(text).is_some(), "{name}");
            assert_eq!(PATTERNS.iter().chain(controls::PATTERNS).filter(|(n, _)| n == name).count(), 1, "{name}");
        }
        for shape in [SOCKET_PREDICATE, PROTOCOL_ERROR_GETTER, MAIN_STEP_FN, MAIN_TICK_FN, MAIN_INIT_FN, CONNECT_RESOLVER,
            COUNT_GETTER, ENTRY_WRAPPER, VISIBLE_FN, RESOURCE_PRESENT, RESOURCE_WRAP, RESOURCE_FIXED, RESOURCE_ENTRY,
            RESOURCE_TEXT, LEGACY_SMALL, ROW_DETAIL_COPY, KEYBOARD_FRAME_FN] {
            assert!(Pattern::parse(shape).is_some(), "{shape}");
        }
    }

    #[test]
    fn agreement_and_expectations_name_the_value() {
        assert_eq!(agree("X", &[4, 4, 4]), Ok(4));
        assert!(agree("X", &[4, 4, 8]).unwrap_err().starts_with("X: locators disagree"));
        assert_eq!(expect("Y", 1, 1), Ok(()));
        assert!(expect("Y", 1, 2).unwrap_err().contains("Y is 0x1"));
    }

    #[test]
    fn a_known_build_refuses_a_scan_that_disagrees_with_its_table() {
        let table = BUILD_6A995428.lobby;
        assert_eq!(checked("lobby", Ok(table), Some(&table)), Ok(table));
        let moved = Lobby { ptr: table.ptr + 4, ..table };
        assert!(checked("lobby", Ok(moved), Some(&table)).unwrap_err().contains("disagrees"));
        assert_eq!(checked("lobby", Ok(moved), None), Ok(moved), "an unknown build uses the scan alone");
        assert_eq!(checked::<Lobby>("lobby", Err("no match".into()), Some(&table)), Err("no match".into()));
    }

    /// Writes the lobby core's five locators at fixed offsets of a synthetic
    /// `.text`, with operands relocated to `base`.
    fn lobby_text(base: usize, lobby_ptr: usize) -> Vec<u8> {
        let mut text = vec![0xCCu8; 0x400];
        let mut put = |at: usize, pattern: &str| {
            let p = Pattern::parse(pattern).unwrap();
            let fill: Vec<u8> = pattern.split_ascii_whitespace().map(|t| if t == "??" { 0 } else { u8::from_str_radix(t, 16).unwrap() }).collect();
            text[at..at + p.len()].copy_from_slice(&fill);
        };
        let names: std::collections::BTreeMap<_, _> = PATTERNS.iter().copied().collect();
        let (get, clear, drain, helper, check, thunk) = (0x000, 0x080, 0x100, 0x200, 0x280, 0x300);
        put(get, names["lobby.get_error"]);
        put(clear, names["lobby.clear_retryable"]);
        put(drain, names["lobby.drain"]);
        put(helper, names["lobby.tcp_protocol_error"]);
        put(check, names["main.lobby_error_check"]);
        let va = (base + lobby_ptr) as u32;
        for at in [get + 1, get + 0x13, clear + 0xB, drain + 1, drain + 0x1A, drain + 0x45, helper + 3] {
            text[at..at + 4].copy_from_slice(&va.to_le_bytes());
        }
        let rel = |from: usize, to: usize| ((to as isize - (from as isize + 5)) as i32).to_le_bytes();
        // .text starts at RVA 0x1000 in the synthetic image.
        text[get + 10..get + 14].copy_from_slice(&rel(get + 9, helper));
        for call in [check + 6, check + 0x1C, check + 0x2E] {
            text[call + 1..call + 5].copy_from_slice(&rel(call, thunk));
        }
        text[thunk] = 0xE9;
        text[thunk + 1..thunk + 5].copy_from_slice(&rel(thunk, get));
        text
    }

    fn image(text: &[u8], base: usize) -> Image {
        let file = crate::sigscan::tests::tiny_image(text, &[]);
        crate::sigscan::tests::load(&file, base)
    }

    #[test]
    fn the_lobby_group_resolves_only_from_unique_agreeing_locators() {
        for base in [0x1000_0000usize, 0x1800_0000] {
            let text = lobby_text(base, 0x2010);
            let found = lobby(&Ctx::new(&image(&text, base), PATTERNS)).unwrap();
            assert_eq!(found, Lobby { ptr: 0x2010, get_error: 0x1000 }, "relocation-aware at {base:#x}");
        }
        let base = 0x1000_0000;
        let text = lobby_text(base, 0x2010);
        // One operand points elsewhere: the locators disagree.
        let mut wrong = text.clone();
        wrong[0x100 + 0x1A..0x100 + 0x1E].copy_from_slice(&((base + 0x2014) as u32).to_le_bytes());
        assert!(lobby(&Ctx::new(&image(&wrong, base), PATTERNS)).unwrap_err().contains("LOBBY_PTR: locators disagree"));
        // A second getter: ambiguous, never guessed.
        let mut twice = text.clone();
        twice.copy_within(0..0x2D, 0x380);
        assert_eq!(lobby(&Ctx::new(&image(&twice, base), PATTERNS)).unwrap_err(), "lobby.get_error: more than one match");
        // A missing required locator.
        let mut missing = text.clone();
        missing[0x100] = 0xCC;
        assert_eq!(lobby(&Ctx::new(&image(&missing, base), PATTERNS)).unwrap_err(), "lobby.drain: no match");
        // The getter's own error codes may change.
        let mut codes = text.clone();
        codes[0x22] = 0x72;
        assert_eq!(lobby(&Ctx::new(&image(&codes, base), PATTERNS)), Ok(Lobby { ptr: 0x2010, get_error: 0x1000 }));
        // The error-code lists only corroborate: one more retryable code, or a
        // reordered check in main, still resolves...
        for (at, code) in [(0x80 + 0x16, 0x3A), (0x280 + 0xC, 0x4C)] {
            let mut edited = text.clone();
            edited[at] = code;
            assert_eq!(lobby(&Ctx::new(&image(&edited, base), PATTERNS)), Ok(Lobby { ptr: 0x2010, get_error: 0x1000 }), "{at:#x}");
        }
        // ...but when found, they must still agree.
        let mut clear = text.clone();
        clear[0x80 + 0xB..0x80 + 0xF].copy_from_slice(&((base + 0x2014) as u32).to_le_bytes());
        assert!(lobby(&Ctx::new(&image(&clear, base), PATTERNS)).unwrap_err().contains("LOBBY_PTR: locators disagree"));
        let mut thunk = text.clone();
        thunk[0x301..0x305].copy_from_slice(&0i32.to_le_bytes());
        assert!(lobby(&Ctx::new(&image(&thunk, base), PATTERNS)).unwrap_err().starts_with("LOBBY_GET_ERROR"));
        // The getter's helper call must reach the protocol-error helper.
        let mut call = text.clone();
        call[10..14].copy_from_slice(&0i32.to_le_bytes());
        assert!(lobby(&Ctx::new(&image(&call, base), PATTERNS)).unwrap_err().starts_with("lobby.get_error helper call"));
        // A call that lands outside the module's code is refused.
        let mut into_data = text.clone();
        into_data[10..14].copy_from_slice(&(0x2010i32 - (0x1000 + 9 + 5)).to_le_bytes());
        assert!(lobby(&Ctx::new(&image(&into_data, base), PATTERNS)).unwrap_err().contains("lands outside the module's code"));
        // A pointer outside .data is not a global.
        let outside = lobby_text(base, 0x1010);
        assert!(lobby(&Ctx::new(&image(&outside, base), PATTERNS)).unwrap_err().contains("not an aligned .data global"));
    }

    #[test]
    fn an_unrelocated_image_resolves_nothing() {
        let text = lobby_text(0x1000_0000, 0x2010);
        let scanned = scan(&image(&text, 0x1000_0000));
        assert!(scanned.lobby.unwrap_err().contains("not fully relocated"));
        assert!(scanned.menus.is_err() && scanned.friends.is_err() && scanned.input.is_err());
    }

    #[test]
    fn an_unsupported_module_logs_every_group_as_unavailable_once() {
        let resolution = Resolution::unsupported(0x1000_0000, "no executable .text section");
        let lines = crate::log::capture(|| report(&resolution, None, Duration::ZERO));
        assert_eq!(lines.len(), 9);
        assert!(lines[0].contains("unknown build"));
        assert!(lines[1..].iter().all(|l| l.ends_with("unavailable: unsupported module: no executable .text section")), "{lines:?}");
    }

    #[test]
    fn known_builds_share_their_data_layout_and_key_input() {
        assert_eq!(BUILD_6A995428.lobby, BUILD_6AC37846.lobby);
        assert_eq!(BUILD_6A995428.friends, BUILD_6AC37846.friends);
        assert_eq!(BUILD_6A995428.menus.characters, BUILD_6AC37846.menus.characters);
        assert_eq!(BUILD_6AC37846.menus.sites.selected - BUILD_6A995428.menus.sites.selected, 0x20);
        // Byte-identical at the same RVAs in both builds (unpacked dumps).
        assert_eq!(BUILD_6A995428.input, BUILD_6AC37846.input);
        let k = BUILD_6AC37846.input;
        assert_eq!((k.frame_call - k.buffer_site, k.buffer_site - k.key_down), (7, 0x14F));
    }

    /// The key-input code of the old build, laid out in a synthetic `.text`
    /// (0x1000) and `.data` (0x2000, IAT at its first 8 bytes).
    struct KeyText { text: Vec<u8>, frame_fn: usize, key_down: usize, frame: usize, controller: usize, handlers: [usize; 3] }

    const INPUT_RVA: usize = 0x2100;
    const KEYBOARD_RVA: usize = 0x2010;
    const IAT_SLOT: usize = 0x2004;

    impl KeyText {
        fn new(base: usize) -> Self {
            let mut k = Self { text: vec![0xCC; 0x1000], frame_fn: 0x000, key_down: 0x100, frame: 0x200, controller: 0x400,
                handlers: [0x300, 0x340, 0x380] };
            let names: std::collections::BTreeMap<_, _> = PATTERNS.iter().copied().collect();
            k.put(k.frame_fn, KEYBOARD_FRAME_FN);
            k.text[k.frame_fn + 0xF] = 0x34;
            k.put(k.key_down, names["input.key_down"]);
            k.put(k.frame, names["input.keyboard_frame"]);
            k.text[k.frame + 3] = 0x34;
            k.text[k.frame + 6..k.frame + 10].copy_from_slice(&0x298u32.to_le_bytes());
            k.put(k.controller, names["input.controller_call"]);
            for (at, body) in k.handlers.into_iter().zip([KEY_PRESSED.as_slice(), KEY_EDGE.as_slice(), KEY_REPEAT.as_slice()]) {
                k.text[at..at + body.len()].copy_from_slice(body);
            }
            for at in [1, 0xD, 0x1B, 0x26] {
                k.abs(k.key_down + at, base + INPUT_RVA);
            }
            let [pressed, edge, repeat] = k.handlers;
            for (at, to) in [(0x5, 0x3C0), (0x14, pressed), (0x1F, edge), (0x2A, repeat)] {
                k.call(k.key_down + at, to);
            }
            k.abs(k.frame + 0xD, base + IAT_SLOT);
            k.abs(k.controller + 2, base + KEYBOARD_RVA);
            k.abs(k.controller + 0x12, base + KEYBOARD_RVA);
            k.call(k.controller + 6, k.frame_fn);
            k
        }

        fn put(&mut self, at: usize, pattern: &str) {
            let fill: Vec<u8> = pattern.split_ascii_whitespace().map(|t| if t == "??" { 0 } else { u8::from_str_radix(t, 16).unwrap() }).collect();
            self.text[at..at + fill.len()].copy_from_slice(&fill);
        }

        fn abs(&mut self, at: usize, va: usize) {
            self.text[at..at + 4].copy_from_slice(&(va as u32).to_le_bytes());
        }

        /// `call` at `at` to `to` (both offsets in this `.text`).
        fn call(&mut self, at: usize, to: usize) {
            self.text[at] = 0xE8;
            self.text[at + 1..at + 5].copy_from_slice(&((to as isize - (at as isize + 5)) as i32).to_le_bytes());
        }

        fn input(&self, base: usize) -> Found<Input> {
            input(&Ctx::new(&image(&self.text, base), PATTERNS))
        }
    }

    #[test]
    fn the_input_group_resolves_only_from_the_clients_own_key_path() {
        let rva = |at: usize| 0x1000 + at;
        for base in [0x1000_0000usize, 0x1800_0000] {
            let k = KeyText::new(base);
            let [pressed, edge, repeat] = k.handlers.map(rva);
            assert_eq!(k.input(base), Ok(Input { input: INPUT_RVA, pressed, edge, repeat, keyboard: KEYBOARD_RVA, iat_slot: IAT_SLOT,
                frame_call: rva(k.frame + 0xB), controller_call: rva(k.controller), buffer_site: rva(k.frame + 4), key_down: rva(k.key_down) }),
                "relocation-aware at {base:#x}");
        }
        let base = 0x1000_0000;
        fn fails(base: usize, why: &str, edit: impl FnOnce(&mut KeyText)) {
            let mut k = KeyText::new(base);
            edit(&mut k);
            let error = k.input(base).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        // INPUT is what every one of the game's own callers passes.
        fails(base, "INPUT: locators disagree", |k| k.abs(k.key_down + 0x26, base + INPUT_RVA + 0x10));
        fails(base, "INPUT: locators disagree", |k| k.abs(k.key_down + 1, base + 0x1F00));
        fails(base, "callers of 0x1380 disagree", |k| {
            k.put(0x500, "B9 00 00 00 00");
            k.abs(0x501, base + 0x2200);
            k.call(0x505, 0x380);
        });
        fails(base, "INPUT bank runs past .data", |k| for at in [1, 0xD, 0x1B, 0x26] { k.abs(k.key_down + at, base + 0x2FF8) });
        // Each call must land on that handler's exact body.
        fails(base, "edge key handler: unexpected code", |k| k.text[0x340 + 0xD] = 4);
        fails(base, "repeat key handler: unexpected code", |k| k.call(k.key_down + 0x2A, 0x340));
        // The keyboard frame: its buffer, its import slot, and its own method.
        fails(base, "keyboard buffer is 0x29c", |k| k.text[k.frame + 6] = 0x9C);
        fails(base, "keyboard state import 0x2010 is not in the IAT", |k| k.abs(k.frame + 0xD, base + 0x2010));
        fails(base, "keyboard frame this", |k| k.text[k.frame + 3] = 0x30);
        fails(base, "KEYBOARD: locators disagree", |k| k.abs(k.controller + 0x12, base + 0x2014));
        fails(base, "keyboard frame: unexpected code", |k| k.call(k.controller + 6, 0x380));
        fails(base, "not in the keyboard frame", |k| {
            k.text.copy_within(0x200..0x211, 0x0C0);
            k.text[0x200..0x211].fill(0xCC);
        });
        // Ambiguity is never resolved by guessing.
        fails(base, "input.key_down: more than one match", |k| k.text.copy_within(0x100..0x135, 0x600));
    }

    fn dump(timestamp: &str) -> Option<Vec<u8>> {
        let path = std::env::var_os("CRYSTAL_FFXIMAIN_DUMPS")
            .map(std::path::PathBuf::from)?
            .join(format!("FFXiMain-{timestamp}.img"));
        std::fs::read(&path).ok()
    }

    /// Applies both of the dump's relocation tables (the packer's and the
    /// loader's, contiguous from `.reloc`) as if it were loaded at `base`.
    fn rebase(file: &mut [u8], base: usize) {
        let delta = (base as u32).wrapping_sub(0x1000_0000);
        let nt = u32::from_le_bytes(file[0x3C..0x40].try_into().unwrap()) as usize;
        let opt = nt + 24;
        let dir = |file: &[u8], at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        let end = dir(file, opt + 96 + 5 * 8) + dir(file, opt + 100 + 5 * 8);
        let headers = crate::sigscan::parse_headers(&file[..0x1000]).unwrap();
        let mut at = headers.sections.iter().find(|s| s.named(".reloc")).unwrap().rva;
        while at < end {
            let (page, size) = (dir(file, at), dir(file, at + 4));
            for index in 0..(size - 8) / 2 {
                let entry = u16::from_le_bytes(file[at + 8 + index * 2..at + 10 + index * 2].try_into().unwrap()) as usize;
                if entry >> 12 == 3 {
                    let site = page + (entry & 0xFFF);
                    let value = u32::from_le_bytes(file[site..site + 4].try_into().unwrap()).wrapping_add(delta);
                    file[site..site + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
            at += size;
        }
    }

    fn dump_image(file: &[u8], base: usize) -> Image {
        Image::from_reader(base, |rva, out| file.get(rva..rva + out.len()).map(|b| out.copy_from_slice(b)).is_some()).unwrap()
    }

    #[test]
    #[ignore = "reads unpacked FFXiMain images from CRYSTAL_FFXIMAIN_DUMPS; skips when absent"]
    fn both_unpacked_builds_resolve_to_their_proven_values() {
        for known in KNOWN {
            let name = format!("{:08X}", known.timestamp);
            let Some(file) = dump(&name) else {
                eprintln!("skipping {name}: dump not found");
                continue;
            };
            let started = Instant::now();
            let image = dump_image(&file, 0x1000_0000);
            let scanned = scan(&image);
            eprintln!("{name}: scanned in {:?}", started.elapsed());
            assert_eq!(scanned.lobby, Ok(known.lobby), "{name}");
            assert_eq!(scanned.diagnostics, Ok(known.diagnostics), "{name}");
            assert_eq!(scanned.characters, Ok(()), "{name}");
            assert_eq!(scanned.menus, Ok(known.menus), "{name}");
            assert_eq!(scanned.friends, Ok(known.friends), "{name}");
            assert_eq!(scanned.input, Ok(known.input), "{name}");
            assert_eq!(scanned.shutdown, Ok(known.shutdown), "{name}");
            assert_eq!(scanned.controller, Ok(known.controller), "{name}");
            assert_eq!(image.codeview_timestamp(), Some(known.timestamp), "{name}");
            let resolved = resolve(&image);
            assert!(resolved.known, "{name}");
            let lines = crate::log::capture(|| report(&resolved, image.codeview_timestamp(), Duration::from_millis(8)));
            eprintln!("{}", lines.join("\n"));
            assert!(lines[0].starts_with(&format!("game layout: FFXiMain {name} size 0xc15000 at 0x10000000, known build")), "{name}");
            for group in ["lobby", "lobby diagnostics", "login menus", "friend locations", "key input", "background shutdown", "controller settings"] {
                assert!(lines.contains(&format!("game layout: {group} resolved (signature scan, matches the known-build table)")), "{name} {group}");
            }
            assert!(lines.contains(&"game layout: lobby characters resolved (signature scan)".to_string()));
            assert_eq!(resolved.input, Ok(known.input), "{name}");

            // An unknown stamp (the next update): the scan alone gives the same values, key input included.
            let mut renamed = file.clone();
            let nt = u32::from_le_bytes(renamed[0x3C..0x40].try_into().unwrap()) as usize;
            renamed[nt + 8..nt + 12].copy_from_slice(&0x7000_0000u32.to_le_bytes());
            let unknown = resolve(&dump_image(&renamed, 0x1000_0000));
            assert!(!unknown.known);
            assert_eq!(unknown.lobby, Ok(known.lobby), "{name}");
            assert_eq!(unknown.diagnostics, Ok(known.diagnostics), "{name}");
            assert_eq!(unknown.characters, Ok(()), "{name}");
            assert_eq!(unknown.menus, Ok(known.menus), "{name}");
            assert_eq!(unknown.friends, Ok(known.friends), "{name}");
            assert_eq!(unknown.input, Ok(known.input), "{name}");
            assert_eq!((unknown.shutdown, unknown.controller), (Ok(known.shutdown), Ok(known.controller)), "{name}");

            // An update that only extends a lobby error-code list still resolves everything.
            let c = Ctx::new(&image, PATTERNS);
            for (locator, at, was) in [("lobby.clear_retryable", 0x16, 0x39), ("main.lobby_error_check", 0xC, 0x4B)] {
                let site = c.unique(locator).unwrap() + at;
                assert_eq!(file[site], was, "{name} {locator}");
                let mut edited = file.clone();
                edited[site] += 1;
                let edited = scan(&dump_image(&edited, 0x1000_0000));
                assert_eq!((edited.lobby, edited.diagnostics, edited.characters), (Ok(known.lobby), Ok(known.diagnostics), Ok(())), "{name} {locator}");
                assert_eq!((edited.friends, edited.input), (Ok(known.friends), Ok(known.input)), "{name} {locator}");
            }
            // A lobby getter that no longer matches switches off only what needs it.
            let mut getter = file.clone();
            getter[known.lobby.get_error + 7] = 0x75;
            let getter = scan(&dump_image(&getter, 0x1000_0000));
            assert_eq!(getter.lobby, Err("lobby.get_error: no match".to_string()), "{name}");
            assert!(getter.diagnostics.unwrap_err().starts_with("needs LOBBY_PTR"), "{name}");
            assert_eq!((getter.characters, getter.menus, getter.friends, getter.input),
                (Ok(()), Ok(known.menus), Ok(known.friends), Ok(known.input)), "{name}");

            // Loaded elsewhere: every RVA is the same.
            let mut moved = file.clone();
            rebase(&mut moved, 0x1800_0000);
            let rebased = scan(&dump_image(&moved, 0x1800_0000));
            assert_eq!(rebased.lobby, Ok(known.lobby), "{name} rebased");
            assert_eq!(rebased.diagnostics, Ok(known.diagnostics), "{name} rebased");
            assert_eq!(rebased.characters, Ok(()), "{name} rebased");
            assert_eq!(rebased.menus, Ok(known.menus), "{name} rebased");
            assert_eq!(rebased.friends, Ok(known.friends), "{name} rebased");
            assert_eq!(rebased.input, Ok(known.input), "{name} rebased");
            assert_eq!((rebased.shutdown, rebased.controller), (Ok(known.shutdown), Ok(known.controller)), "{name} rebased");
            // Read before the stub relocated .text: refused, not shifted.
            let stale = scan(&dump_image(&unrelocated(&file, &moved, 0), 0x1800_0000));
            assert!(stale.lobby.is_err() && stale.menus.is_err() && stale.friends.is_err() && stale.input.is_err(), "{name} unrelocated");
            // Read while relocation was running (its first half done): refused.
            let half = scan(&dump_image(&unrelocated(&file, &moved, 2), 0x1800_0000));
            assert!(half.lobby.unwrap_err().contains("not fully relocated"), "{name} half relocated");
        }
    }

    /// The rebased image with `.text` (from its `1/parts` point onward, or all
    /// of it for 0) still at its preferred-base bytes, as it is between the
    /// unpacking stub's decompression and the end of its relocation.
    fn unrelocated(file: &[u8], moved: &[u8], parts: usize) -> Vec<u8> {
        let mut back = moved.to_vec();
        let headers = crate::sigscan::parse_headers(&file[..0x1000]).unwrap();
        let text = &headers.sections[headers.text];
        let from = text.rva + text.size.checked_div(parts).unwrap_or(0);
        back[from..text.rva + text.size].copy_from_slice(&file[from..text.rva + text.size]);
        back
    }
    /// Loads copies of both builds into this test process: the second one
    /// cannot get 0x10000000, so its own unpacking stub relocates it. Runs only
    /// the DLLs' attach code (CRT and ATL init), as an image dumper does.
    #[test]
    #[ignore = "loads copies of FFXiMain.dll from CRYSTAL_FFXIMAIN_BUILDS; never the installed game"]
    fn loaded_copies_resolve_through_the_live_capture() {
        use windows_sys::Win32::{Foundation::FreeLibrary, System::LibraryLoader::LoadLibraryExW};
        let Some(dir) = std::env::var_os("CRYSTAL_FFXIMAIN_BUILDS").map(std::path::PathBuf::from) else { return; };
        let mut loaded = Vec::new();
        for known in KNOWN {
            let path = dir.join(format!("FFXiMain-{:08X}.dll", known.timestamp));
            let text = path.to_string_lossy().to_ascii_lowercase();
            assert!(!text.contains("squareenix") && !text.contains("playonline"), "never load the installed game");
            if !path.exists() {
                eprintln!("skipping {}: not found", path.display());
                continue;
            }
            let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0) };
            assert!(!module.is_null(), "{}", std::io::Error::last_os_error());
            let base = module as usize;
            loaded.push(module);
            let resolution = resolve_module(base).expect("an initialized module is never deferred");
            eprintln!("{:08X} at {base:#x}", known.timestamp);
            assert!(resolution.known);
            assert_eq!(resolution.lobby, Ok(known.lobby));
            assert_eq!(resolution.diagnostics, Ok(known.diagnostics));
            assert_eq!(resolution.characters, Ok(()));
            assert_eq!(resolution.menus, Ok(known.menus));
            assert_eq!(resolution.friends, Ok(known.friends));
            assert_eq!(resolution.input, Ok(known.input));
            assert_eq!((&resolution.shutdown, &resolution.controller), (&Ok(known.shutdown), &Ok(known.controller)));
            assert_eq!(crate::lobby::verified_lobby(&resolution), Ok(known.lobby), "live getter re-check");
        }
        if loaded.len() == 2 {
            assert_ne!(loaded[1] as usize, 0x1000_0000, "the second copy is relocated");
        }
        for module in loaded.into_iter().rev() {
            unsafe { FreeLibrary(module) };
        }
    }
}
