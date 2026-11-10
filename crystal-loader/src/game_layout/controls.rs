//! Background shutdown and live controller settings: the FFXiMain code these
//! two features call, found by signature like every other group (see the
//! parent module). Locators keep opcodes and struct displacements exact and
//! wildcard every relocated address and every relative call/jump operand, so
//! an update that only moves code or data still resolves; any doubt (a
//! locator that does not match exactly once, operands that disagree, a target
//! outside the module's code) refuses the group with its reason.

use super::{agree, expect, Ctx, Found};

/// The client's text-command entry (cdecl `(text, source)`), the chat-submit
/// caller that proves that ABI, and the globals its in-game test reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shutdown {
    pub entry: usize,
    pub caller: usize,
    /// The command context pointer the entry loads first.
    pub context: usize,
    /// The world-entry "commands enabled" DWORD (a scalar, set to 1).
    pub enabled: usize,
    /// The manager object the entry reads (`+0x59`).
    pub manager: usize,
}

/// The command entry's stock first instruction, `sub esp, 0x404`. A host
/// (Windower, Ashita) may have replaced it with a `jmp` to its own hook.
pub(crate) const SHUTDOWN_PROLOGUE: [u8; 6] = [0x81, 0xEC, 4, 4, 0, 0];
/// Bytes of the entry the shutdown locator and the live check cover.
pub(crate) const SHUTDOWN_BODY: usize = 0x99;
/// Offset of the caller's `call rel32` to the entry.
pub(crate) const SHUTDOWN_CALLER_CALL: usize = 15;
pub(crate) const SHUTDOWN_CALLER_LEN: usize = 24;
/// The entry from its seventh byte (after the prologue a host may hook).
pub(crate) const SHUTDOWN_COMMAND: &str = "8B 8C 24 08 04 00 00 55 56 57 8D 84 24 10 02 00 00 68 00 02 00 00 50 33 ED 51 8B 0D ?? ?? ?? ?? \
    89 6C 24 18 E8 ?? ?? ?? ?? A1 ?? ?? ?? ?? C6 84 24 0F 04 00 00 00 3B C5 89 2D ?? ?? ?? ?? 75 ?? 5F 5E 33 C0 5D 81 C4 04 04 00 00 C3 \
    8A 84 24 10 02 00 00 84 C0 75 ?? 5F 5E 33 C0 5D 81 C4 04 04 00 00 C3 8A 84 24 18 04 00 00 3C 01 74 ?? 3C 02 C6 05 ?? ?? ?? ?? 01 \
    75 ?? C6 05 ?? ?? ?? ?? 00 3C 02 A1 ?? ?? ?? ?? 0F 94 C2 88 15 ?? ?? ?? ?? 8A 48 59";
/// `mov eax,[esp+4]; test; jne; mov eax,[esp+8]; push 1; push eax; call; add esp,8; ret`:
/// the chat-submit callback passes the user source (1), as Crystal does.
pub(crate) const SHUTDOWN_CALLER: &str = "8B 44 24 04 85 C0 75 ?? 8B 44 24 08 6A 01 50 E8 ?? ?? ?? ?? 83 C4 08 C3";

/// The live controller reload: the stock config-tool sequence (suspend
/// background input, reload, resume) and everything it reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Controller {
    /// `reload(this, initial)`, thiscall, `ret 4`.
    pub reload: usize,
    /// `suspend(keyboard, suspended)`, thiscall, `ret 4`.
    pub suspend: usize,
    /// The pad configuration object global and its two vtables (+0, +0x8C).
    pub config: usize,
    pub config_vtable: usize,
    pub member_vtable: usize,
    pub keyboard: usize,
    pub flags: usize,
    pub bindings: usize,
    pub guid: usize,
    /// The config tool object global (`+4` is its launch flag).
    pub config_tool: usize,
    /// Every code locator's site, in [`CONTROLLER_CODE`] order, for the
    /// reader's live re-check.
    pub code: [usize; CONTROLLER_CODE.len()],
}

/// The controller locators the live check re-reads, in `Controller::code` order.
pub(crate) const CONTROLLER_CODE: [&str; 15] = ["controller.reload", "controller.slots", "controller.mode",
    "controller.recreate", "controller.pad_frame", "controller.pad_acquire", "controller.read_int", "controller.read_guid",
    "controller.tool_launch", "controller.tool_finish", "controller.factory", "controller.ready", "controller.config_ctor",
    "controller.tool_frame", "controller.tool_suspend"];
/// The pad-source slot map the binding parser walks (ends with -1).
pub(crate) const SLOT_MAP: [i32; 33] = [8, 9, 14, 13, 10, 3, 1, 0, 2, 16, 25, 26, 15, 23, 24, 22, 21, 19, 20, 18, 17, 6, 7, 5, 4,
    12, 31, 32, 33, 34, 35, 36, -1];
/// The binding parser's strtok delimiters.
pub(crate) const DELIMITERS: &[u8] = b" ,";
/// The config object's size (its factory allocates this much).
pub(crate) const CONFIG_SIZE: usize = 0xB4;
/// Struct offsets the reader uses: the +0x80 member's +0xC sub-object.
pub(crate) const CONFIG_MEMBER: usize = 0x8C;
pub(crate) const CONFIG_READY: usize = 0x94;

/// Anchored shapes (checked where a call lands, never scanned).
const CONFIG_CTOR: &str = "56 8B F1 8D 4E 04 E8 ?? ?? ?? ?? 8D 8E 80 00 00 00 E8 ?? ?? ?? ?? 8D 8E 9C 00 00 00 E8 ?? ?? ?? ?? \
    8B CE C7 06 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B C6 5E C3";
const MEMBER_CTOR: &str = "56 8B F1 8D 4E 0C E8 ?? ?? ?? ?? 8B CE C7 06 ?? ?? ?? ??";
const INNER_CTOR: &str = "56 8B F1 33 C0 89 46 04 C7 06 ?? ?? ?? ?? 89 46 08";
const TOOL_FRAME: &str = "8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ??";
const TOOL_SUSPEND: &str = "8B 0D ?? ?? ?? ?? 85 C9 74 ?? 6A 01 E8 ?? ?? ?? ??";

pub(super) const PATTERNS: &[(&str, &str)] = &[
    ("shutdown.command", SHUTDOWN_COMMAND),
    ("shutdown.caller", SHUTDOWN_CALLER),
    // Controller settings (2840, 5130, 5490, 60eb, 83d0, 8d90, 18fc0, 19060, 1bcc30, 1bce30 in both known builds).
    ("controller.reload", "81 EC 18 01 00 00 55 56 57 8B D1 B9 40 00 00 00 33 C0 8D 7C 24 24 8D B2 8C 00 00 00 F3 AB 8D 44 24 0C 8D 4C 24 24 50 68 ?? ?? ?? ?? 51 8B CE C7 44 24 18 00 01 00 00 E8 ?? ?? ?? ?? 3C 01 75 ?? 8D 54 24 24 52 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B AC 24 30 01 00 00 8B 44 24 14 83 C4 08 8D 4C 24 24 55 50 51 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? EB ?? 8B AC 24 28 01 00 00 8B 0D ?? ?? ?? ?? 55 6A 00 6A 00 E8 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 04 B9 40 00 00 00 33 C0 8D 7C 24 24 8D 54 24 0C F3 AB 52 8D 44 24 28 68 ?? ?? ?? ?? 50 8B CE C7 44 24 18 00 01 00 00 E8 ?? ?? ?? ?? 3C 01 75 ?? A0 ?? ?? ?? ?? 84 C0 74 ?? 8D 4C 24 24 51 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 54 24 14 8B 0D ?? ?? ?? ?? 83 C4 08 8D 44 24 24 52 50 E8 ?? ?? ?? ?? EB ?? 8B 0D ?? ?? ?? ?? 6A 00 6A 00 E8 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 04 B9 40 00 00 00 33 C0 8D 7C 24 24 8D 54 24 24 F3 AB 8D 4C 24 0C C7 44 24 0C 00 01 00 00 51 68 ?? ?? ?? ?? 52 8B CE E8 ?? ?? ?? ?? 3C 01 75 ?? A0 ?? ?? ?? ?? 84 C0 74 ?? 8D 44 24 24 50 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 8B 4C 24 14 83 C4 08 8D 54 24 24 51 8B 0D ?? ?? ?? ?? 52 E8 ?? ?? ?? ?? EB ?? 8B 0D ?? ?? ?? ?? 6A 00 6A 00 E8 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 04 8B 0D ?? ?? ?? ?? A1 ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 89 4C 24 14 8D 4C 24 10 89 44 24 10 A1 ?? ?? ?? ?? 68 ?? ?? ?? ?? 51 8B CE 89 54 24 20 89 44 24 24 E8 ?? ?? ?? ?? 3C 01 0F 85 ?? ?? ?? ?? A0 ?? ?? ?? ?? 84 C0 0F 84 ?? ?? ?? ?? 8B 54 24 1F 8B 44 24 1E 8B 4C 24 1D 81 E2 FF 00 00 00 52 8B 54 24 20 25 FF 00 00 00 81 E1 FF 00 00 00 50 8B 44 24 23 81 E2 FF 00 00 00 51 8B 4C 24 26 52 8B 54 24 29 25 FF 00 00 00 81 E1 FF 00 00 00 50 8B 44 24 2C 81 E2 FF 00 00 00 51 8B 4C 24 2E 52 8B 54 24 30 25 FF 00 00 00 81 E1 FF FF 00 00 50 8B 44 24 30 81 E2 FF FF 00 00 51 52 50 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 30 8B 54 24 10 8B 44 24 14 55 83 EC 10 8B CC 89 11 8B 54 24 2C 89 41 04 8B 44 24 30 89 51 08 89 41 0C 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 5F 5E 5D 81 C4 18 01 00 00 C2 04 00 8B 54 24 10 55 8B 44 24 18 83 EC 10 8B CC 89 11 8B 54 24 2C 89 41 04 8B 44 24 30 89 51 08 89 41 0C 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 68 ?? ?? ?? ?? E8 ?? ?? ?? ?? 83 C4 04 5F 5E 5D 81 C4 18 01 00 00 C2 04 00"),
    ("controller.slots", "8B 54 24 04 81 EC 94 00 00 00 53 33 DB 3B D3 0F 84 ?? ?? ?? ?? 55 56 83 CD FF 57 B9 25 00 00 00 8B C5 8D 7C 24 10 68 ?? ?? ?? ?? F3 AB B9 20 00 00 00 BF ?? ?? ?? ?? F3 AB 52 E8 ?? ?? ?? ?? 83 C4 08 3B C3 74 ?? BF ?? ?? ?? ?? BE ?? ?? ?? ?? 39 2E 74 ?? 50 E8 ?? ?? ?? ?? 8B 0E 88 07 0F BE C0 68 ?? ?? ?? ?? 53 89 44 8C 1C E8 ?? ?? ?? ?? 83 C4 0C 47 83 C6 04 3B C3 75 ?? B9 FE 00 00 00 BE ?? ?? ?? ?? BF ?? ?? ?? ?? 89 2D ?? ?? ?? ?? 89 2D ?? ?? ?? ?? 89 1D ?? ?? ?? ?? F3 A5 B9 16 00 00 00 BE ?? ?? ?? ?? BF ?? ?? ?? ?? 89 1D ?? ?? ?? ?? F3 A5 BF ?? ?? ?? ?? 8B 74 9C 10 83 FE FF 0F 84 ?? ?? ?? ?? 8B C6 99 8B E8 33 EA 2B EA 83 FD 20 7D ?? 85 F6 8B CB 7D ?? 83 C8 FF 83 CA FF E8 ?? ?? ?? ?? 89 04 ED ?? ?? ?? ?? 89 14 ED ?? ?? ?? ?? EB ?? B8 01 00 00 00 33 D2 E8 ?? ?? ?? ?? 89 04 F5 ?? ?? ?? ?? 89 14 F5 ?? ?? ?? ?? EB ?? 81 FF ?? ?? ?? ?? 7F ?? 81 FF ?? ?? ?? ?? 7C ?? 85 F6 7E ?? 8D 46 E1 99 89 87 58 FF FF FF 89 97 5C FF FF FF EB ?? 8D 46 1F 99 89 87 58 FF FF FF 89 97 5C FF FF FF EB ?? 81 FF ?? ?? ?? ?? 7C ?? 81 FF ?? ?? ?? ?? 7F ?? 85 F6 8D 46 E1 7F ?? 8D 46 1F 99 89 07 89 57 04 83 C7 08 43 81 FF ?? ?? ?? ?? 0F 8C ?? ?? ?? ?? 8B 84 24 8C 00 00 00 32 DB 5F 5E 83 F8 FF 88 1D ?? ?? ?? ?? 88 1D ?? ?? ?? ?? 5D 74 ?? 8B 44 24 58 83 F8 FF 74 ?? 99 8B C8 8B 44 24 48 33 CA 2B CA 99 33 C2 2B C2 3B C1 75 ?? B3 01 88 1D ?? ?? ?? ?? 8B 44 24 14 99 33 C2 2B C2 3B C1 75 ?? C6 05 ?? ?? ?? ?? 01 84 DB 75 ?? A0 ?? ?? ?? ?? 84 C0 75 ?? 66 C7 05 ?? ?? ?? ?? 00 00 5B 81 C4 94 00 00 00 C2 08 00 8B 54 24 04 81 EC 94 00 00 00 85 D2 0F 84 ?? ?? ?? ?? 53 56 57 B9 25 00 00 00 83 C8 FF 8D 7C 24 0C F3 AB B9 FE 00 00 00 BE ?? ?? ?? ?? BF ?? ?? ?? ?? C7 05 ?? ?? ?? ?? FF FF FF FF C7 05 ?? ?? ?? ?? FF FF FF FF 83 C8 FF F3 A5 B9 20 00 00 00 BF ?? ?? ?? ?? 68 ?? ?? ?? ?? 52 F3 AB E8 ?? ?? ?? ?? 83 C4 08 85 C0 74 ?? BF ?? ?? ?? ?? BE ?? ?? ?? ?? 83 3E FF 74 ?? 50 E8 ?? ?? ?? ?? 8B 0E 88 07 0F BE C0 68 ?? ?? ?? ?? 6A 00 89 44 8C 18 E8 ?? ?? ?? ?? 83 C4 0C 47 83 C6 04 85 C0 75 ?? 33 DB 8B 7C 9C 0C 83 FF FF 74 ?? 8B C7 99 8B F0 33 F2 2B F2 83 FE 20 7D ?? 85 FF 8B CB 7D ?? 83 C8 FF 83 CA FF E8 ?? ?? ?? ?? 89 04 F5 ?? ?? ?? ?? 89 14 F5 ?? ?? ?? ?? EB ?? B8 01 00 00 00 33 D2 E8 ?? ?? ?? ?? 89 04 FD ?? ?? ?? ?? 89 14 FD ?? ?? ?? ?? 43 83 FB 25 7C ?? 8B 84 24 88 00 00 00 5F 5E 83 F8 FF 5B 74 ?? 8B 44 24 54 83 F8 FF 74 ?? 99 8B C8 8B 44 24 44 33 CA 2B CA 99 33 C2 2B C2 3B C1 75 ?? C6 05 ?? ?? ?? ?? 01 8B 44 24 10 99 33 C2 2B C2 3B C1 75 ?? C6 05 ?? ?? ?? ?? 01 A0 ?? ?? ?? ?? 84 C0 75 ?? A0 ?? ?? ?? ?? 84 C0 75 ?? 66 C7 05 ?? ?? ?? ?? 00 00 81 C4 94 00 00 00 C2 08 00"),
    ("controller.mode", "83 EC 20 A0 ?? ?? ?? ?? 8A 0D ?? ?? ?? ?? 8A 15 ?? ?? ?? ?? 88 44 24 02 B8 ?? ?? ?? ?? 88 4C 24 03 88 54 24 01 89 44 24 04 C7 44 24 08 ?? ?? ?? ?? C7 44 24 0C ?? ?? ?? ?? C7 44 24 10 ?? ?? ?? ?? C7 44 24 14 ?? ?? ?? ?? C7 44 24 18 ?? ?? ?? ?? C7 44 24 1C 00 00 00 00 8D 4C 24 04 83 C1 04 C6 00 00 8B 01 85 C0 75 ?? 8B 44 24 24 85 C0 0F 84 ?? ?? ?? ?? 56 68 ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? 83 C4 08 8D 74 24 08 85 C0 74 ?? 50 E8 ?? ?? ?? ?? 83 C4 04 83 F8 01 75 ?? 8B 06 C6 00 01 68 ?? ?? ?? ?? 6A 00 E8 ?? ?? ?? ?? 8B 4E 04 83 C6 04 83 C4 08 85 C9 75 ?? 8A 44 24 30 5E 84 C0 75 ?? 8A 4C 24 01 A0 ?? ?? ?? ?? 3A C8 75 ?? 8A 54 24 02 A0 ?? ?? ?? ?? 3A D0 75 ?? 8A 44 24 03 8A 0D ?? ?? ?? ?? 3A C1 74 ?? C6 05 ?? ?? ?? ?? 01 8A 0D ?? ?? ?? ?? C6 05 ?? ?? ?? ?? 00 88 0D ?? ?? ?? ?? 83 C4 20 C2 0C 00 90 90 90 90 90 90 90 56 57 B9 04 00 00 00 8D 7C 24 0C BE ?? ?? ?? ?? 33 C0 F3 A7 5F 5E 74 ?? 8B 44 24 0C 8B 4C 24 04 8B 54 24 08 A3 ?? ?? ?? ?? 8A 44 24 14 89 0D ?? ?? ?? ?? 8B 4C 24 10 89 15 ?? ?? ?? ?? 84 C0 89 0D ?? ?? ?? ?? 75 ?? C6 05 ?? ?? ?? ?? 01 C2 14 00 90 90 90 90 90 90 90 90 90 90 90 90 90 90 90 8A 44 24 04 33 D2 3A C2 A0 ?? ?? ?? ?? 74 ?? 3A C2 75 ?? A0 ?? ?? ?? ?? 88 15 ?? ?? ?? ?? A2 ?? ?? ?? ?? C6 05 ?? ?? ?? ?? 01 EB ?? 3A C2 74 ?? A0 ?? ?? ?? ?? 88 15 ?? ?? ?? ?? A2 ?? ?? ?? ?? 39 91 90 00 00 00 74 ?? 3C 01 75 ?? A2 ?? ?? ?? ?? C2 04 00"),
    ("controller.recreate", "A0 ?? ?? ?? ?? 84 C0 8B 83 90 00 00 00 74 ?? 3B C5 74 ?? 8B 08 50 FF 51 20 A1 ?? ?? ?? ?? 3B C5 74 ?? 8B 10 50 FF 52 2C A1 ?? ?? ?? ?? 50 8B 08 FF 51 08 89 2D ?? ?? ?? ?? 8B 83 90 00 00 00 50 8B 10 FF 52 08 89 AB 90 00 00 00 C7 83 9C 0C 00 00 FF FF FF FF C6 83 99 0C 00 00 00 A0 ?? ?? ?? ?? 84 C0 74 ?? 8B CB E8 ?? ?? ?? ?? C6 05 ?? ?? ?? ?? 00 E9 ?? ?? ?? ??"),
    ("controller.pad_frame", "A0 ?? ?? ?? ?? 83 EC 7C 53 33 DB 55 56 3A C3 57 8B F1 74 ?? E8 ?? ?? ?? ?? 3A C3 88 86 9A 0C 00 00 0F 84 ?? ?? ?? ?? 8B CE E8 ?? ?? ?? ?? 89 1D ?? ?? ?? ?? 89 9E 90 00 00 00 8A 0D ?? ?? ?? ?? 33 C0 3A CB C6 44 24 13 01 74 ?? 8B 86 84 00 00 00 68 01 01 00 00 53 68 ?? ?? ?? ?? 8B 08 6A 04 50 FF 51 10 3B C3 7C ?? 39 9E 90 00 00 00 75 ?? 8B 86 84 00 00 00 6A 01 53 68 ?? ?? ?? ?? 8B 10 6A 04 50 88 5C 24 27 FF 52 10 3B C3 7D ?? 89 9E 90 00 00 00 5F 5E 5D 32 C0 5B 83 C4 7C C3 8B 96 90 00 00 00 88 9E 98 0C 00 00 3B D3 0F 84 ?? ?? ?? ?? B9 0B 00 00 00 33 C0 8D 7C 24 54 F3 AB 8D 4C 24 54 C7 44 24 54 2C 00 00 00 8B 02 51 52 FF 50 0C F6 44 24 58 0A 74 ?? C6 86 98 0C 00 00 01 8B 86 90 00 00 00 BD 10 00 00 00 8D 4C 24 28 C7 44 24 28 14 00 00 00 89 6C 24 2C 89 5C 24 30 89 5C 24 34 C7 44 24 38 40 01 00 00 8B 10 51 6A 01 50 FF 52 18 3B C3 7D ?? 50 8B CE E8 ?? ?? ?? ?? 5F 5E 5D 5B 83 C4 7C C3 8B 86 90 00 00 00 68 ?? ?? ?? ?? 50 8B 10 FF 52 2C 3B C3 7D ?? 50 8B CE E8 ?? ?? ?? ?? 5F 5E 5D 5B 83 C4 7C C3 8A 15 ?? ?? ?? ?? 33 C9 3A D3 8B 86 90 00 00 00 0F 95 C1 8B 10 8D 0C 8D 04 00 00 00 83 C9 01 51 8B 0D ?? ?? ?? ?? 8B 49 18 51 50 FF 52 34 3B C3 7D ?? 50 8B CE E8 ?? ?? ?? ?? 5F 5E 5D 5B 83 C4 7C C3 B9 06 00 00 00 33 C0 8D 7C 24 3C F3 AB 8B 86 90 00 00 00 BF 18 00 00 00 8D 4C 24 3C 89 7C 24 3C 89 6C 24 40 C7 44 24 48 01 00 00 00 C7 44 24 4C 81 FF FF FF C7 44 24 50 7F 00 00 00 89 5C 24 44 8B 10 51 6A 04 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C C7 44 24 44 04 00 00 00 51 8B 10 6A 04 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C C7 44 24 44 08 00 00 00 51 8B 10 6A 04 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C C7 44 24 44 0C 00 00 00 51 8B 10 6A 04 50 FF 52 18 8B 86 90 00 00 00 89 6C 24 44 8D 4C 24 3C BD 04 00 00 00 8B 10 51 55 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C C7 44 24 44 14 00 00 00 51 8B 10 55 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C 89 7C 24 44 51 8B 10 55 50 FF 52 18 8B 86 90 00 00 00 8D 4C 24 3C C7 44 24 44 1C 00 00 00 51 8B 10 55 50 FF 52 18 8B 86 90 00 00 00 50 8B 10 FF 52 1C 38 5C 24 13 0F 84 ?? ?? ?? ?? 38 1D ?? ?? ?? ?? 0F 84 ?? ?? ?? ?? 8B 86 90 00 00 00 6A 03 68 ?? ?? ?? ?? 68 ?? ?? ?? ?? 8B 08 50 FF 51 10 85 C0 0F 8C ?? ?? ?? ?? 8B 15 ?? ?? ?? ?? 83 FA 02 76 ?? BA 02 00 00 00 89 15 ?? ?? ?? ?? B9 0E 00 00 00 33 C0 8D 7C 24 54 8B B6 90 00 00 00 F3 AB 83 C8 FF 89 54 24 70 89 44 24 5C 89 44 24 68 8D 44 24 20 8D 54 24 18 89 44 24 78 8D 4C 24 14 53 8D 44 24 58 68 ?? ?? ?? ?? 89 5C 24 20 89 6C 24 24 89 5C 24 28 89 5C 24 2C 89 5C 24 1C C7 44 24 5C 38 00 00 00 C7 44 24 60 12 00 00 00 89 5C 24 68 C7 44 24 6C 10 27 00 00 89 5C 24 74 89 54 24 7C 89 9C 24 84 00 00 00 89 AC 24 88 00 00 00 89 8C 24 8C 00 00 00 89 9C 24 90 00 00 00 8B 16 50 68 ?? ?? ?? ?? 56 FF 52 48 5F 5E 5D B0 01 5B 83 C4 7C C3"),
    ("controller.pad_acquire", "A0 ?? ?? ?? ?? 53 32 DB 84 C0 74 ?? 56 8D B1 A0 0C 00 00 57 8B D6 88 1E A1 ?? ?? ?? ?? 8B 48 10 8A 01 41 88 02 42 84 C0 75 ?? 8B FE 83 C9 FF 33 C0 56 F2 AE 8B 0D ?? ?? ?? ?? 4F 89 0F 8B 15 ?? ?? ?? ?? 89 57 04 A1 ?? ?? ?? ?? 89 47 08 66 8B 0D ?? ?? ?? ?? 66 89 4F 0C 8A 15 ?? ?? ?? ?? 88 57 0E FF 15 ?? ?? ?? ?? 5F 5E 85 C0 74 ?? 50 FF 15 ?? ?? ?? ?? B0 01 5B C3 8A C3 5B C3 90 90 90 A0 ?? ?? ?? ?? 84 C0 74 ?? 81 C1 A0 0C 00 00 51 FF 15 ?? ?? ?? ?? 85 C0 A3 ?? ?? ?? ?? 74 ?? 56 8B 35 ?? ?? ?? ?? 68 ?? ?? ?? ?? 50 FF D6 A3 ?? ?? ?? ?? A1 ?? ?? ?? ?? 68 ?? ?? ?? ?? 50 FF D6 8B 0D ?? ?? ?? ?? 68 ?? ?? ?? ?? 51 A3 ?? ?? ?? ?? FF D6 A3 ?? ?? ?? ?? B0 01 5E C3 32 C0 C3 90 A0 ?? ?? ?? ?? 84 C0 74 ?? A1 ?? ?? ?? ?? 85 C0 74 ?? 50 FF 15 ?? ?? ?? ?? C7 05 ?? ?? ?? ?? 00 00 00 00 C3"),
    ("controller.read_int", "51 8B 44 24 10 8B 54 24 08 50 8D 44 24 04 52 8B 54 24 14 50 8B 41 04 6A 00 52 50 C7 44 24 18 00 00 00 00 FF 15 ?? ?? ?? ?? 85 C0 0F 94 C0 59 C2 0C 00"),
    ("controller.read_guid", "81 EC C8 00 00 00 8D 44 24 04 8D 54 24 08 50 8D 44 24 04 52 8B 94 24 D8 00 00 00 50 8B 41 04 6A 00 52 50 C7 44 24 1C 40 00 00 00 C7 44 24 18 00 00 00 00 FF 15 ?? ?? ?? ?? 85 C0 75 ?? 8D 4C 24 08 6A 40 8D 54 24 4C 51 52 E8 ?? ?? ?? ?? 8B 84 24 D8 00 00 00 83 C4 0C 8D 4C 24 48 50 51 FF 15 ?? ?? ?? ?? B0 01 81 C4 C8 00 00 00 C2 08 00 32 C0 81 C4 C8 00 00 00 C2 08 00"),
    ("controller.tool_launch", "56 68 ?? ?? ?? ?? 6A 00 8B F1 FF 15 ?? ?? ?? ?? 85 C0 74 ?? B8 01 00 00 00 5E C3 8B 46 0C C6 46 04 01 83 F8 FF C7 46 08 00 00 00 00 74 ?? B8 01 00 00 00 5E C3 6A 00 6A 04 56 68 ?? ?? ?? ?? 6A 00 6A 00 E8 ?? ?? ?? ?? 83 C4 18 83 F8 FF 89 46 0C 75 ?? B8 01 00 00 00 5E C3 50 FF 15 ?? ?? ?? ?? 33 C0 5E C3"),
    ("controller.tool_finish", "51 56 8B F1 8B 46 0C 83 F8 FF 74 ?? 8D 4C 24 04 51 50 FF 15 ?? ?? ?? ?? 85 C0 74 ?? 81 7C 24 04 03 01 00 00 74 ?? 8B 56 0C 52 FF 15 ?? ?? ?? ?? C7 46 0C FF FF FF FF 8A 46 04 84 C0 74 ?? 8B 46 08 83 F8 5A 7E ?? 68 ?? ?? ?? ?? 6A 00 FF 15 ?? ?? ?? ?? 85 C0 75 ?? 8B 0D ?? ?? ?? ?? 50 E8 ?? ?? ?? ?? C6 46 04 00 C7 46 08 00 00 00 00 5E 59 C3 40 89 46 08 5E 59 C3"),
    // The config object's factory, its ready-check reload (2b20, 2820) and the window procedure's reload (16d26).
    ("controller.factory", "68 B4 00 00 00 E8 ?? ?? ?? ?? 83 C4 04 85 C0 74 ?? 8B C8 E8 ?? ?? ?? ?? 85 C0 A3 ?? ?? ?? ??"),
    ("controller.ready", "8B 81 94 00 00 00 85 C0 74 ?? 6A 01 E8 ?? ?? ?? ?? C3"),
    ("controller.reload_site", "8B 0D ?? ?? ?? ?? 85 C9 74 ?? 6A 00 E8 ?? ?? ?? ??"),
];

/// Pattern text of a locator in this module's table, or of an anchored shape.
pub(crate) fn pattern(name: &str) -> Option<&'static str> {
    match name {
        "controller.config_ctor" => Some(CONFIG_CTOR),
        "controller.tool_frame" => Some(TOOL_FRAME),
        "controller.tool_suspend" => Some(TOOL_SUSPEND),
        _ => PATTERNS.iter().find(|(n, _)| *n == name).map(|(_, text)| *text),
    }
}

/// The raw target of `call rel32` at `rva`, which may lie outside the image
/// (a host redirected it): `None` only when there is no call there.
fn raw_call(c: &Ctx, rva: usize) -> Option<isize> {
    (c.image.u8(rva)? == 0xE8).then_some(())?;
    Some(rva as isize + 5 + c.image.u32(rva + 1)? as u32 as i32 as isize)
}

fn in_text(c: &Ctx, what: &str, rva: usize) -> Found<usize> {
    let (start, text) = c.image.text();
    (start <= rva && rva < start + text.len()).then_some(rva).ok_or_else(|| format!("{what} ({rva:#x}) is not in the module's code"))
}

/// Every `mov dword [abs], imm32` in `.text` storing `value` at `target`.
fn stores(c: &Ctx, target: usize, value: u32) -> usize {
    let (_, text) = c.image.text();
    let operand = (c.image.base + target) as u32;
    text.windows(10).filter(|w| w[..2] == [0xC7, 0x05] && w[2..6] == operand.to_le_bytes() && w[6..] == value.to_le_bytes())
        .count()
}

pub(super) fn shutdown(c: &Ctx) -> Found<Shutdown> {
    let entry = c.unique("shutdown.command")?.checked_sub(SHUTDOWN_PROLOGUE.len()).ok_or("shutdown entry underflows")?;
    let prologue = c.image.bytes(entry, SHUTDOWN_PROLOGUE.len()).ok_or("shutdown entry is unreadable")?;
    let hooked = prologue[0] == 0xE9 && matches!(prologue[5], 0 | 0x90 | 0xCC);
    if prologue != SHUTDOWN_PROLOGUE && !hooked {
        return Err("shutdown entry starts with neither its stock frame nor a host jmp".into());
    }
    let context = c.abs(entry + 0x22)?;
    let enabled = c.abs(entry + 0x30)?;
    let manager = c.abs(entry + 0x89)?;
    for (what, rva) in [("shutdown context", context), ("commands enabled", enabled), ("shutdown manager", manager)] {
        c.global(what, rva)?;
    }
    // The entry clears the DWORD after ENABLED, and sets one source flag byte twice.
    expect("command dispatch pair", c.abs(entry + 0x40)?, enabled + 8)?;
    let source = agree("command source flag", &[c.abs(entry + 0x78)?, c.abs(entry + 0x81)?])?;
    for (what, rva) in [("command source flag", source), ("command result flag", c.abs(entry + 0x92)?)] {
        c.image.inside(".data", rva, 1).then_some(()).ok_or_else(|| format!("{what} ({rva:#x}) is not in .data"))?;
    }
    in_text(c, "shutdown helper", c.call(entry + 0x2A)?)?;
    // World entry enables commands with the scalar 1 (93e0d in both known builds).
    (stores(c, enabled, 1) >= 1).then_some(()).ok_or("nothing sets the commands-enabled DWORD to 1")?;
    // Exactly one chat-submit caller reaches the entry, directly or through a
    // host's redirect of that one call (outside this module).
    let size = c.image.headers.size as isize;
    let callers: Vec<usize> = c.hits("shutdown.caller").iter().copied()
        .filter(|at| raw_call(c, at + SHUTDOWN_CALLER_CALL).is_some_and(|t| t == entry as isize || t < 0 || t >= size))
        .collect();
    let [caller] = callers[..] else {
        return Err(format!("shutdown.caller: {} candidate callers of the entry", callers.len()));
    };
    Ok(Shutdown { entry, caller, context, enabled, manager })
}

pub(super) fn controller(c: &Ctx) -> Found<Controller> {
    let unique = |name: &str| c.unique(name);
    let reload = unique("controller.reload")?;
    let slots = unique("controller.slots")?;
    let mode = unique("controller.mode")?;
    let recreate = unique("controller.recreate")?;
    let pad_frame = unique("controller.pad_frame")?;
    let pad_acquire = unique("controller.pad_acquire")?;
    let read_int = unique("controller.read_int")?;
    let read_guid = unique("controller.read_guid")?;
    let tool_launch = unique("controller.tool_launch")?;
    let tool_finish = unique("controller.tool_finish")?;
    let factory = unique("controller.factory")?;
    let ready = unique("controller.ready")?;

    // reload's own callees and its four setting names.
    for (at, callee, what) in [(0x37, read_int, "integer reader"), (0xBB, read_int, "integer reader"), (0x138, read_int, "integer reader"),
        (0x1C1, read_guid, "GUID reader"), (0x6A, mode, "mode parser"), (0x83, mode, "mode parser"), (0xEF, slots, "binding parser"),
        (0x100, slots, "binding parser")] {
        expect(&format!("reload {what} call"), c.call(reload + at)?, callee)?;
    }
    for (at, name) in [(0x28, "padmode000"), (0xAC, "padsin000"), (0x131, "padexsin000"), (0x1B2, "padguid000")] {
        c.string(reload + at, name)?;
    }
    expect("pad recreation call", c.call(recreate + 0x67)?, pad_frame)?;
    expect("pad device call", c.call(pad_frame + 0x14)?, pad_acquire)?;

    let keyboard = agree("controller KEYBOARD", &[c.abs(reload + 0x66)?, c.abs(reload + 0x7A)?, c.abs(reload + 0xE2)?,
        c.abs(reload + 0xF8)?, c.abs(reload + 0x167)?, c.abs(reload + 0x175)?, c.abs(reload + 0x27B)?, c.abs(reload + 0x2B3)?,
        c.abs(tool_finish + 0x59)?])?;
    let flags = agree("controller FLAGS", &[c.abs(reload + 0xC5)?, c.abs(reload + 0x142)?, c.abs(reload + 0x1CF)?,
        c.abs(mode + 4)?, c.abs(mode + 0x19)?, c.abs(mode + 0xD3)?])?;
    expect("XInput flag", agree("XInput flag", &[c.abs(mode + 0x10)?, c.abs(pad_frame + 1)?, c.abs(pad_acquire + 1)?])?, flags + 4)?;
    expect("recreation flag", agree("recreation flag", &[c.abs(recreate + 1)?, c.abs(recreate + 0x6E)?, c.abs(mode + 0xEB)?])?, flags + 12)?;
    let bindings = agree("controller BINDINGS", &[c.abs(slots + 0x33)?, c.abs(slots + 0x47)?])?;
    let guid = agree("controller GUID", &[c.abs(mode + 0x11C)?, c.abs(mode + 0x13F)?])?;
    for (at, offset) in [(0x149, 4), (0x135, 8), (0x151, 0xC)] {
        expect("controller GUID part", c.abs(mode + at)?, guid + offset)?;
    }
    let map = agree("binding slot map", &[c.abs(slots + 0x4C)?, c.abs(slots + 0x25F)?])?;
    let map_bytes: Vec<u8> = SLOT_MAP.iter().flat_map(|v| v.to_le_bytes()).collect();
    (c.image.bytes(map, map_bytes.len()) == Some(&map_bytes[..])).then_some(()).ok_or("binding slot map differs")?;
    let delimiters = agree("binding delimiters", &[c.abs(slots + 0x27)?, c.abs(slots + 0x62)?, c.abs(slots + 0x246)?,
        c.abs(slots + 0x276)?, c.abs(mode + 0x77)?, c.abs(mode + 0xA0)?])?;
    c.image.c_string(delimiters, DELIMITERS).then_some(()).ok_or("binding delimiters differ")?;

    // suspend: the third function of the mode locator, called by the config tool.
    let suspend = agree("suspend", &[mode + 0x170, c.call(tool_finish + 0x5E)?])?;
    let suspend_sites: Vec<usize> = c.image.callers(suspend).into_iter()
        .filter(|at| at.checked_sub(12).is_some_and(|site| c.shape(site, "tool suspend", TOOL_SUSPEND).is_ok()
            && c.abs(site + 2).ok() == Some(keyboard)))
        .collect();
    let [tool_suspend_call] = suspend_sites[..] else {
        return Err(format!("controller.tool_suspend: {} launch-time suspend calls", suspend_sites.len()));
    };
    let tool_suspend = tool_suspend_call - 12;

    // The config tool frame runs just before the keyboard frame call.
    let frame_call = c.unique("input.controller_call")?;
    let tool_frame = frame_call.checked_sub(0xB).ok_or("config tool frame underflows")?;
    c.shape(tool_frame, "config tool frame", TOOL_FRAME)?;
    expect("config tool frame call", c.call(tool_frame + 6)?, tool_finish)?;
    expect("config tool frame KEYBOARD", c.abs(frame_call + 2)?, keyboard)?;
    let config_tool = c.abs(tool_frame + 2)?;

    // The config object: its factory's size and global, its constructor's
    // vtable and ready check, and the +0x80 member's +0xC sub-object.
    expect("config object size", c.dword(factory + 1)?, CONFIG_SIZE)?;
    let config = c.abs(factory + 0x1B)?;
    let ctor = c.call(factory + 0x13)?;
    c.shape(ctor, "config constructor", CONFIG_CTOR)?;
    let config_vtable = c.abs(ctor + 0x25)?;
    expect("config constructor ready check", c.call(ctor + 0x29)?, ready)?;
    expect("ready-check reload", c.call(ready + 0xC)?, reload)?;
    expect("config ready field", c.dword(ready + 2)?, CONFIG_READY)?;
    let member = c.call(ctor + 0x11)?;
    c.shape(member, "config member constructor", MEMBER_CTOR)?;
    expect("config member", c.dword(ctor + 0xD)? + c.byte(member + 5)?, CONFIG_MEMBER)?;
    let inner = c.call(member + 6)?;
    c.shape(inner, "config sub-object constructor", INNER_CTOR)?;
    let member_vtable = c.abs(inner + 0xA)?;
    // The window procedure reloads the same object.
    let sites: Vec<usize> = c.hits("controller.reload_site").iter().copied()
        .filter(|at| c.call(at + 0xC).ok() == Some(reload)).collect();
    if sites.is_empty() {
        return Err("controller.reload_site: no window-procedure reload".into());
    }
    let mut configs = vec![config];
    for site in &sites {
        configs.push(c.abs(site + 2)?);
    }
    agree("controller CONFIG", &configs)?;

    for (what, rva) in [("controller CONFIG", config), ("controller KEYBOARD", keyboard), ("config tool", config_tool)] {
        c.global(what, rva)?;
    }
    for (what, rva, len) in [("controller FLAGS", flags, 15), ("controller BINDINGS", bindings, 32), ("controller GUID", guid, 16)] {
        c.image.inside(".data", rva, len).then_some(()).ok_or_else(|| format!("{what} ({rva:#x}) is not in .data"))?;
    }
    for (what, rva) in [("config vtable", config_vtable), ("config member vtable", member_vtable)] {
        c.image.inside(".rdata", rva, 4).then_some(()).ok_or_else(|| format!("{what} ({rva:#x}) is not in .rdata"))?;
    }
    for (what, rva) in [("reload", reload), ("suspend", suspend)] {
        in_text(c, what, rva)?;
    }
    let code = [reload, slots, mode, recreate, pad_frame, pad_acquire, read_int, read_guid, tool_launch, tool_finish, factory,
        ready, ctor, tool_frame, tool_suspend];
    Ok(Controller { reload, suspend, config, config_vtable, member_vtable, keyboard, flags, bindings, guid, config_tool, code })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sigscan::Pattern;

    #[test]
    fn every_shape_and_live_site_has_a_parsable_pattern() {
        for shape in [CONFIG_CTOR, MEMBER_CTOR, INNER_CTOR, TOOL_FRAME, TOOL_SUSPEND, SHUTDOWN_COMMAND, SHUTDOWN_CALLER] {
            assert!(Pattern::parse(shape).is_some(), "{shape}");
        }
        for name in CONTROLLER_CODE {
            assert!(pattern(name).and_then(Pattern::parse).is_some(), "{name}");
        }
        assert_eq!(Pattern::parse(SHUTDOWN_COMMAND).unwrap().len() + SHUTDOWN_PROLOGUE.len(), SHUTDOWN_BODY);
        assert_eq!(Pattern::parse(SHUTDOWN_CALLER).unwrap().len(), SHUTDOWN_CALLER_LEN);
        assert_eq!(SLOT_MAP.last(), Some(&-1));
    }
}