// Crystal-hosted adaptation of the FF11 Limit Break pool change.
// Upstream copyright (c) 2026 finalpantasy (MIT); see LICENSE-TEXTURE-FORGE.
use crate::mem::{read, read_into};
use windows_sys::Win32::System::{
    Diagnostics::Debug::FlushInstructionCache,
    Environment::SetEnvironmentVariableW,
    Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE},
    Threading::GetCurrentProcess,
};

const INITIALIZER: [u8; 253] = [
    0x83, 0xec, 0x20, 0x56, 0x57, 0x8b, 0xf1, 0xb9, 0x08, 0x00, 0x00, 0x00, 0x33, 0xc0, 0x8d, 0x7c,
    0x24, 0x08, 0xf3, 0xab, 0x8d, 0x44, 0x24, 0x08, 0xc7, 0x44, 0x24, 0x08, 0x20, 0x00, 0x00, 0x00,
    0x50, 0xff, 0x15, 0xb4, 0x91, 0x32, 0x10, 0x8b, 0x44, 0x24, 0x10, 0xb9, 0x00, 0x20, 0xa0, 0x05,
    0xc1, 0xe8, 0x14, 0x3d, 0x80, 0x00, 0x00, 0x00, 0x89, 0x46, 0x04, 0xbf, 0x00, 0x00, 0x00, 0x04,
    0x7e, 0x25, 0x3d, 0x00, 0x01, 0x00, 0x00, 0x7e, 0x07, 0xc7, 0x46, 0x04, 0x00, 0x01, 0x00, 0x00,
    0x8b, 0x46, 0x04, 0x2d, 0x80, 0x00, 0x00, 0x00, 0xc1, 0xe0, 0x14, 0x8d, 0xb8, 0x00, 0x00, 0x00,
    0x04, 0x8d, 0x88, 0x00, 0x20, 0xa0, 0x05, 0x51, 0xe8, 0xfe, 0x88, 0x2e, 0x00, 0x83, 0xc4, 0x04,
    0x89, 0x46, 0x0c, 0x85, 0xc0, 0x75, 0x15, 0x68, 0x00, 0x20, 0xa0, 0x05, 0xbf, 0x00, 0x00, 0x00,
    0x04, 0xe8, 0xe5, 0x88, 0x2e, 0x00, 0x83, 0xc4, 0x04, 0x89, 0x46, 0x0c, 0x8b, 0x46, 0x0c, 0x85,
    0xc0, 0x75, 0x08, 0x5f, 0x32, 0xc0, 0x5e, 0x83, 0xc4, 0x20, 0xc3, 0x83, 0xc0, 0x0f, 0x53, 0x24,
    0xf0, 0x55, 0x89, 0x46, 0x08, 0x8d, 0x34, 0x38, 0x83, 0xc7, 0x80, 0x57, 0x50, 0x8d, 0x9e, 0x00,
    0x00, 0xa0, 0x00, 0x8d, 0xab, 0x00, 0x10, 0x00, 0x00, 0xe8, 0x62, 0xf4, 0xff, 0xff, 0x68, 0x80,
    0xff, 0x9f, 0x00, 0x56, 0xe8, 0x77, 0xf4, 0xff, 0xff, 0x68, 0x80, 0x0f, 0x00, 0x00, 0x53, 0xe8,
    0xac, 0xf4, 0xff, 0xff, 0x68, 0x80, 0xff, 0xff, 0x00, 0x55, 0xe8, 0x81, 0xf4, 0xff, 0xff, 0x81,
    0xc5, 0x00, 0x00, 0x00, 0x01, 0x68, 0x80, 0x0f, 0x00, 0x00, 0x55, 0xe8, 0xb0, 0xf4, 0xff, 0xff,
    0x83, 0xc4, 0x28, 0xb0, 0x01, 0x5d, 0x5b, 0x5f, 0x5e, 0x83, 0xc4, 0x20, 0xc3,
];
const POOL_INIT: [u8; 109] = [
    0x8b, 0x54, 0x24, 0x08, 0x8b, 0x4c, 0x24, 0x0c, 0x8b, 0x44, 0x24, 0x04, 0x83, 0xe2, 0xf0, 0x89,
    0x01, 0x53, 0x8d, 0x44, 0x02, 0xf0, 0x8b, 0x54, 0x24, 0x14, 0x56, 0x33, 0xf6, 0x89, 0x02, 0x8b,
    0x01, 0x57, 0x8b, 0x58, 0x18, 0x89, 0x70, 0x0c, 0x8b, 0x3a, 0x89, 0x70, 0x14, 0x89, 0x78, 0x08,
    0x8b, 0x3a, 0x89, 0x78, 0x10, 0xbf, 0xfc, 0xff, 0xff, 0xff, 0x23, 0xdf, 0x89, 0x70, 0x04, 0x89,
    0x58, 0x18, 0x89, 0x70, 0x1c, 0x8b, 0x02, 0x8b, 0x11, 0x89, 0x50, 0x0c, 0x89, 0x70, 0x08, 0x8b,
    0x09, 0x89, 0x70, 0x10, 0x89, 0x48, 0x14, 0x8b, 0x48, 0x18, 0x23, 0xcf, 0x89, 0x70, 0x1c, 0x5f,
    0x5e, 0xc7, 0x40, 0x04, 0x01, 0x00, 0x00, 0x00, 0x89, 0x48, 0x18, 0x5b, 0xc3,
];
const WRAPPER: [u8; 29] = [
    0x8b, 0x44, 0x24, 0x08, 0x8b, 0x4c, 0x24, 0x04, 0x68, 0xb8, 0x70, 0x45, 0x10, 0x68, 0xb4, 0x70,
    0x45, 0x10, 0x50, 0x51, 0xe8, 0x77, 0xff, 0xff, 0xff, 0x83, 0xc4, 0x10, 0xc3,
];
const INITIALIZER_RELOCATIONS: &[usize] = &[35, 36, 37, 38, 105, 106, 107, 108, 130, 131, 132, 133, 186, 187, 188, 189, 197, 198, 199, 200, 208, 209, 210, 211, 219, 220, 221, 222, 236, 237, 238, 239];

const MIB: u32 = 1024 * 1024;
const EXPECTED_BYTES: u32 = 1024 * MIB - 128;

#[derive(Clone, Copy)]
pub(crate) struct Pool {
    first: usize,
}

impl Pool {
    pub(crate) fn measured_bytes(self) -> Option<u32> {
        let start = read::<u32>(self.first)?;
        let end = read::<u32>(self.first + 4)?;
        if start == 0 { return None; }
        end.checked_sub(start)?.checked_add(16)
    }
}

fn relative(base: usize, operand: usize) -> Option<usize> {
    let target = operand.checked_add(4)?.checked_add_signed(read::<i32>(base.checked_add(operand)?)? as isize)?;
    Some(target)
}

pub(crate) fn prepare(base: usize) -> Result<Pool, &'static str> {
    if !crate::pe::this_process_large_address_aware() {
        return Err("the Crystal host is missing Large Address Aware");
    }
    let identity = crate::pe::identity(base).ok_or("invalid GameMain image")?;
    let nt = read::<u32>(base + 0x3c).ok_or("missing PE header")? as usize;
    let optional = read::<u16>(base + nt + 20).ok_or("missing optional header")? as usize;
    let count = read::<u16>(base + nt + 6).ok_or("missing PE sections")? as usize;
    if count == 0 || count > 96 || nt + 24 + optional + count * 40 > identity.size {
        return Err("invalid PE sections");
    }
    let mut sections = Vec::new();
    for index in 0..count {
        let section = base + nt + 24 + optional + index * 40;
        let size = read::<u32>(section + 8).ok_or("invalid PE section size")? as usize;
        let start = read::<u32>(section + 12).ok_or("invalid PE section start")? as usize;
        let flags = read::<u32>(section + 36).ok_or("invalid PE section flags")?;
        let end = start.checked_add(size).filter(|end| *end <= identity.size).ok_or("section outside GameMain image")?;
        sections.push((start..end, flags));
    }
    let inside = |rva: usize, len: usize, flag: u32| sections.iter().any(|(region, flags)|
        flags & flag != 0 && region.contains(&rva) && len <= region.end - rva);
    let executable = 0x2000_0000;
    let mut found = None;
    for (region, flags) in &sections {
        if flags & executable == 0 || region.len() < INITIALIZER.len() { continue; }
        let mut bytes = vec![0u8; region.len()];
        if !read_into(base + region.start, &mut bytes) { return Err("cannot read executable GameMain section"); }
        for (index, candidate) in bytes.windows(INITIALIZER.len()).enumerate() {
            if candidate[..8] != INITIALIZER[..8] { continue; }
            if candidate.iter().zip(INITIALIZER).enumerate().all(|(i, (a, b))|
                *a == b || INITIALIZER_RELOCATIONS.contains(&i)) {
                if found.replace(region.start + index).is_some() { return Err("ambiguous GameMain allocator"); }
            }
        }
    }
    let init = found.ok_or("unrecognised GameMain allocator")?;
    let mut first = 0;
    let mut pool_init = 0;
    for (index, (slot, offset)) in [(0xba, 0), (0xc5, 8), (0xd0, 24), (0xdb, 16), (0xec, 32)].into_iter().enumerate() {
        let wrapper = relative(base, init + slot).filter(|rva| inside(*rva, WRAPPER.len(), executable))
            .ok_or("invalid GameMain pool wrapper")?;
        let bytes = read::<[u8; WRAPPER.len()]>(base + wrapper).ok_or("unreadable GameMain pool wrapper")?;
        if bytes.iter().zip(WRAPPER).enumerate().any(|(i, (a, b))|
            !(9..13).contains(&i) && !(14..18).contains(&i) && !(21..25).contains(&i) && *a != b) {
            return Err("unexpected GameMain pool wrapper");
        }
        let start = u32::from_le_bytes(bytes[14..18].try_into().unwrap()) as usize;
        let end = u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize;
        let target = relative(base, wrapper + 21).ok_or("invalid GameMain pool initializer")?;
        if index == 0 { first = start; pool_init = target; }
        if first.checked_add(offset) != Some(start) || start.checked_add(4) != Some(end) || target != pool_init {
            return Err("GameMain pool references disagree");
        }
    }
    if !inside(pool_init, POOL_INIT.len(), executable) ||
        read::<[u8; POOL_INIT.len()]>(base + pool_init) != Some(POOL_INIT) ||
        !first.checked_sub(base).is_some_and(|rva| inside(rva, 40, 0x8000_0000)) ||
        read::<[u32; 10]>(first) != Some([0; 10]) {
        return Err("GameMain pool is already initialized or its layout differs");
    }

    const PATCH: usize = 0x42;
    const LENGTH: usize = 37;
    let address = base + init + PATCH;
    let before: [u8; LENGTH] = INITIALIZER[PATCH..PATCH + LENGTH].try_into().unwrap();
    if read::<[u8; LENGTH]>(address) != Some(before) { return Err("GameMain allocator changed during inspection"); }
    let mut after = before;
    let main_base = (1024 - 128) * MIB;
    after[27..31].copy_from_slice(&main_base.to_le_bytes());
    after[33..37].copy_from_slice(&(main_base + 26 * MIB + 8192).to_le_bytes());
    let mut protection = 0;
    let mut ignored = 0;
    // The GameMain factory has returned on this game thread and pool state is still empty.
    unsafe {
        if VirtualProtect(address as _, LENGTH, PAGE_EXECUTE_READWRITE, &mut protection) == 0 {
            return Err("cannot set GameMain allocator page protection");
        }
        std::ptr::copy_nonoverlapping(after.as_ptr(), address as *mut u8, LENGTH);
        let verified = read::<[u8; LENGTH]>(address) == Some(after);
        if !verified {
            std::ptr::copy_nonoverlapping(before.as_ptr(), address as *mut u8, LENGTH);
        }
        let flushed = FlushInstructionCache(GetCurrentProcess(), address as _, LENGTH) != 0;
        let restored = VirtualProtect(address as _, LENGTH, protection, &mut ignored) != 0;
        if !verified || !flushed || !restored { return Err("GameMain allocator update could not be verified"); }
    }
    Ok(Pool { first })
}

const STATUS_VARIABLE: &str = "CRYSTAL_FFXI_POOL_VERIFIED_V1";

pub(crate) fn publish_status(verified: bool) -> bool {
    let name: Vec<u16> = STATUS_VARIABLE.encode_utf16().chain(Some(0)).collect();
    let value: Vec<u16> = std::process::id().to_string().encode_utf16().chain(Some(0)).collect();
    // Windows copies these strings into this process's environment.
    unsafe {
        SetEnvironmentVariableW(name.as_ptr(), if verified { value.as_ptr() } else { std::ptr::null() }) != 0
    }
}

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

thread_local! {
    static MEASUREMENT: Cell<Option<Arc<Mutex<Option<Pool>>>>> = const { Cell::new(None) };
}

pub(crate) fn arm(base: usize) -> Result<(), &'static str> {
    MEASUREMENT.with(|slot| {
        let state = slot.take().ok_or("GameMain was created outside the monitored startup")?;
        let result = (|| {
            let mut pool = state.lock().map_err(|_| "asset-pool verifier lock failed")?;
            if pool.is_some() { return Err("GameMain was already prepared"); }
            *pool = Some(prepare(base)?);
            crate::log!("asset pool: 1 GiB change prepared; allocation pending");
            Ok(())
        })();
        slot.set(Some(state));
        result
    })
}

pub(crate) struct Verifier(Option<JoinHandle<()>>);

impl Verifier {
    pub(crate) fn start() -> Result<Self, String> {
        let state = Arc::new(Mutex::new(None::<Pool>));
        let observed = Arc::clone(&state);
        let worker = thread::Builder::new().name("asset-pool-verify".into()).spawn(move || {
            loop {
                thread::park_timeout(Duration::from_millis(10));
                let pool = match observed.lock() { Ok(pool) => *pool, Err(_) => return };
                if let Some(pool) = pool {
                    if pool.measured_bytes() == Some(EXPECTED_BYTES) {
                        crate::log!("asset pool: 1024 MiB verified");
                        if !publish_status(true) {
                            crate::log!("asset pool: could not publish the verified result to Windower");
                        }
                        return;
                    }
                }
                if Arc::strong_count(&observed) == 1 { return; }
            }
        }).map_err(|err| format!("cannot start asset-pool verifier: {err}"))?;
        MEASUREMENT.with(|slot| slot.set(Some(state)));
        Ok(Self(Some(worker)))
    }
}

impl Drop for Verifier {
    fn drop(&mut self) {
        MEASUREMENT.with(|slot| { slot.take(); });
        if let Some(worker) = self.0.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
        publish_status(false);
    }
}

