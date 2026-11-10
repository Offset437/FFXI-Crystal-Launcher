//! PlayOnline portraits (handle pictures), read at runtime from the user's
//! own PlayOnline Viewer install. Crystal bundles no portrait image.
//!
//! The Viewer keeps them scrambled in `<POL>\data\icon\download\hnf000.png`
//! .. `hnf379.png`: 256×192 or 256×256 sheets of eight 64×96 portraits, four
//! across and two down. Portrait `id` is sheet `id >> 3`, cell `id & 7`; 0 is
//! the default silhouette. Sheets are decoded once and kept, cut into cells,
//! in a small in-memory cache.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

pub(crate) const WIDTH: u32 = 64;
pub(crate) const HEIGHT: u32 = 96;
/// The default portrait, shown for everyone in privacy mode.
pub(crate) const SILHOUETTE: u16 = 0;
const SHEETS: u16 = 380;
const CELLS: usize = 8;
/// Decoded sheets kept (each about 200 KB).
const CACHED_SHEETS: usize = 32;
/// A sheet is a few tens of KB; anything far larger is not one.
const MAX_SHEET_BYTES: u64 = 1 << 20;
const PNG: [u8; 4] = [0x89, b'P', b'N', b'G'];

/// Where portrait `id` sits: its sheet and the cell's top-left pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    pub sheet: u16,
    pub x: u32,
    pub y: u32,
}

pub(crate) fn cell(id: u16) -> Option<Cell> {
    let sheet = id >> 3;
    (sheet < SHEETS).then(|| Cell { sheet, x: u32::from(id & 3) * WIDTH, y: u32::from((id >> 2) & 1) * HEIGHT })
}

pub(crate) fn sheet_path(viewer: &Path, sheet: u16) -> PathBuf {
    viewer.join("data").join("icon").join("download").join(format!("hnf{sheet:03}.png"))
}

/// The PlayOnline folder, found as the session line finds it; looked up once.
pub(crate) fn viewer_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| crate::discovery::find(crate::discovery::Product::PlayOnline, None)).as_deref()
}

/// Unscramble a Viewer PNG in place. The seed is the first four bytes minus
/// the PNG signature, bytewise, as a little-endian u32; each following
/// little-endian word is XORed with `state >> 16`, then `state *= 0x19660D`;
/// a last odd byte takes the low byte of `state >> 16`. A plain PNG has seed
/// 0 and passes unchanged. False when the result is not a PNG.
pub(crate) fn descramble(bytes: &mut [u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let mut state = u32::from_le_bytes([0, 1, 2, 3].map(|i| bytes[i].wrapping_sub(PNG[i])));
    bytes[..4].copy_from_slice(&PNG);
    let mut words = bytes[4..].chunks_exact_mut(2);
    for word in &mut words {
        let value = u16::from_le_bytes([word[0], word[1]]) ^ (state >> 16) as u16;
        word.copy_from_slice(&value.to_le_bytes());
        state = state.wrapping_mul(0x19660D);
    }
    if let [last] = words.into_remainder() {
        *last ^= (state >> 16) as u8;
    }
    bytes[4..8] == [0x0d, 0x0a, 0x1a, 0x0a]
}

/// One portrait's pixels: 64×96 RGBA, row by row.
pub(crate) type Pixels = Arc<[u8]>;

/// A decoded sheet's eight cells; `None` for an empty or transparent cell.
type Sheet = [Option<Pixels>; CELLS];

/// The cells of `sheet` from the Viewer folder `viewer`, or `None` when the
/// file is missing or not a portrait sheet.
fn load_sheet(viewer: &Path, sheet: u16) -> Option<Sheet> {
    let path = sheet_path(viewer, sheet);
    if std::fs::metadata(&path).ok()?.len() > MAX_SHEET_BYTES {
        return None;
    }
    let mut bytes = std::fs::read(path).ok()?;
    if !descramble(&mut bytes) || !sheet_size(&bytes) {
        return None;
    }
    let image = imagecrate::load_from_memory_with_format(&bytes, imagecrate::ImageFormat::Png).ok()?.to_rgba8();
    if image.width() != 4 * WIDTH || image.height() < 2 * HEIGHT {
        return None;
    }
    Some(std::array::from_fn(|index| {
        let (x, y) = ((index as u32 & 3) * WIDTH, (index as u32 >> 2) * HEIGHT);
        let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for row in y..y + HEIGHT {
            let start = ((row * image.width() + x) * 4) as usize;
            pixels.extend_from_slice(&image.as_raw()[start..start + (WIDTH * 4) as usize]);
        }
        (!blank(&pixels)).then(|| Pixels::from(pixels))
    }))
}

/// The PNG header (IHDR, read before any decode) declares a sheet's size: 256 wide, 192 to 256
/// tall. A small file claiming a huge image never gets its pixels allocated.
fn sheet_size(png: &[u8]) -> bool {
    let Some(header) = png.get(8..24).filter(|h| &h[4..8] == b"IHDR") else { return false };
    let width = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let height = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
    width == 4 * WIDTH && (2 * HEIGHT..=4 * WIDTH).contains(&height)
}

/// Fully transparent, or one flat colour: no portrait in this cell.
fn blank(pixels: &[u8]) -> bool {
    let mut each = pixels.chunks_exact(4);
    let first = each.next().unwrap_or(&[0; 4]);
    pixels.chunks_exact(4).all(|p| p[3] == 0) || each.all(|p| p == first)
}

struct Cache {
    /// The Viewer folder the sheets came from.
    viewer: PathBuf,
    /// Most recently used last.
    sheets: VecDeque<(u16, Option<Sheet>)>,
    available: Option<Arc<[u16]>>,
}
static CACHE: Mutex<Cache> = Mutex::new(Cache { viewer: PathBuf::new(), sheets: VecDeque::new(), available: None });

/// Portrait `id`'s 64×96 RGBA from the installed Viewer, or `None` when the
/// Viewer, its sheet or that cell is missing (show the initial instead).
/// The first use of a sheet reads and decodes it (about a millisecond).
pub(crate) fn rgba(id: u16) -> Option<Pixels> {
    rgba_in(viewer_dir()?, id)
}

fn rgba_in(viewer: &Path, id: u16) -> Option<Pixels> {
    let cell = cell(id)?;
    let index = (id & 7) as usize;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.viewer != viewer {
        *cache = Cache { viewer: viewer.to_path_buf(), sheets: VecDeque::new(), available: None };
    }
    if let Some(at) = cache.sheets.iter().position(|(sheet, _)| *sheet == cell.sheet) {
        let entry = cache.sheets.remove(at)?;
        let pixels = entry.1.as_ref().and_then(|cells| cells[index].clone());
        cache.sheets.push_back(entry);
        return pixels;
    }
    let loaded = load_sheet(viewer, cell.sheet);
    let pixels = loaded.as_ref().and_then(|cells| cells[index].clone());
    if cache.sheets.len() == CACHED_SHEETS {
        cache.sheets.pop_front();
    }
    cache.sheets.push_back((cell.sheet, loaded));
    pixels
}

/// Every portrait ID with a picture in the installed sheets, ascending: the
/// picker's choices. Reads and decodes every sheet once (a few hundred ms),
/// so call it off the UI thread; later calls return the kept list. Empty
/// without a Viewer install.
pub(crate) fn available() -> Arc<[u16]> {
    let Some(viewer) = viewer_dir() else { return Arc::from([]) };
    let kept = |cache: &Cache| (cache.viewer == viewer).then(|| cache.available.clone()).flatten();
    if let Some(ids) = kept(&CACHE.lock().unwrap_or_else(|e| e.into_inner())) {
        return ids;
    }
    let ids: Arc<[u16]> = available_in(viewer).into();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.viewer == viewer {
        cache.available = Some(ids.clone());
    }
    ids
}

fn available_in(viewer: &Path) -> Vec<u16> {
    (0..SHEETS)
        .filter_map(|sheet| Some((sheet, load_sheet(viewer, sheet)?)))
        .flat_map(|(sheet, cells)| {
            (0..CELLS as u16).filter(move |&index| cells[index as usize].is_some()).map(move |index| sheet << 3 | index)
        })
        .collect()
}

/// The picture to show for `portrait` (a friend's `portrait` or the
/// snapshot's `own_portrait`), or `None` to show the initial avatar. Privacy
/// mode shows the default silhouette for everyone. UI thread only.
pub(crate) fn image(portrait: Option<u16>) -> Option<slint::Image> {
    stock_image(shown(portrait)?)
}

/// The portrait ID `image` draws for `portrait`: the silhouette in privacy mode.
pub(crate) fn shown(portrait: Option<u16>) -> Option<u16> {
    if crate::privacy::on() { Some(SILHOUETTE) } else { portrait }
}

/// Stock portrait `id` itself, privacy mode or not: the picker's choices, which identify nobody.
/// UI thread only.
pub(crate) fn stock_image(id: u16) -> Option<slint::Image> {
    thread_local! {
        static IMAGES: std::cell::RefCell<std::collections::HashMap<u16, slint::Image>> = Default::default();
    }
    if let Some(image) = IMAGES.with(|images| images.borrow().get(&id).cloned()) {
        return Some(image);
    }
    let pixels = rgba(id)?;
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&pixels, WIDTH, HEIGHT);
    let image = slint::Image::from_rgba8(buffer);
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        if images.len() >= CACHED_SHEETS * CELLS {
            images.clear();
        }
        images.insert(id, image.clone());
    });
    Some(image)
}

/// Whether choosing a portrait in Crystal is enabled: only once a live test
/// has confirmed where PlayOnline keeps it (`wire::PORTRAIT_FIELD_CONFIRMED`).
pub(crate) const PICKER_ENABLED: bool = crate::wire::PORTRAIT_FIELD_CONFIRMED;

/// Portraits on one page of the picker: four rows of eight.
pub(crate) const PAGE: usize = 32;

/// The open portrait picker: the account whose portrait it changes, the installed choices once
/// listed (off the UI thread), the page shown and the last refusal.
#[derive(Debug, Default)]
pub(crate) struct Picker {
    pub account: String,
    pub ids: Option<Arc<[u16]>>,
    pub page: usize,
    pub error: Option<String>,
}

impl Picker {
    pub(crate) fn pages(&self) -> usize {
        self.ids.as_ref().map_or(0, |ids| ids.len().div_ceil(PAGE))
    }

    /// The IDs on the page shown.
    pub(crate) fn shown(&self) -> &[u16] {
        let ids = self.ids.as_deref().unwrap_or_default();
        let start = (self.page * PAGE).min(ids.len());
        &ids[start..(start + PAGE).min(ids.len())]
    }

    /// The choices arrived: open on the page holding `current`.
    pub(crate) fn listed(&mut self, ids: Arc<[u16]>, current: Option<u16>) {
        self.page = current.and_then(|id| ids.iter().position(|i| *i == id)).map_or(0, |at| at / PAGE);
        self.ids = Some(ids);
    }
}

/// The note beside a disabled picker, and the refusal of a change meanwhile.
pub(crate) fn unavailable() -> &'static str {
    crate::i18n::t("Changing your portrait is not available yet.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sheet cache is process-wide; tests that use it take turns.
    static SHEETS_IN_USE: Mutex<()> = Mutex::new(());

    /// The Viewer's scrambling, the inverse of `descramble` for `seed`.
    fn scramble(png: &[u8], seed: u32) -> Vec<u8> {
        let mut bytes = png.to_vec();
        let mut state = seed;
        let mut words = bytes[4..].chunks_exact_mut(2);
        for word in &mut words {
            let value = u16::from_le_bytes([word[0], word[1]]) ^ (state >> 16) as u16;
            word.copy_from_slice(&value.to_le_bytes());
            state = state.wrapping_mul(0x19660D);
        }
        if let [last] = words.into_remainder() {
            *last ^= (state >> 16) as u8;
        }
        for (i, b) in seed.to_le_bytes().into_iter().enumerate() {
            bytes[i] = PNG[i].wrapping_add(b);
        }
        bytes
    }

    /// A 256×`height` sheet whose cell `i` is filled with grey `i * 20 + 10`,
    /// except cell 6 (transparent) and cell 7 (one flat colour).
    fn sheet_png(height: u32) -> Vec<u8> {
        let image = imagecrate::RgbaImage::from_fn(256, height, |x, y| {
            let index = (x / WIDTH) + 4 * (y / HEIGHT).min(1);
            match index {
                6 => imagecrate::Rgba([0, 0, 0, 0]),
                7 => imagecrate::Rgba([9, 9, 9, 255]),
                // A dot keeps each portrait cell from being one flat colour.
                _ if x % WIDTH == 3 && y % HEIGHT == 5 => imagecrate::Rgba([255, 0, 0, 255]),
                _ => imagecrate::Rgba([(index * 20 + 10) as u8, 0, 0, 255]),
            }
        });
        let mut png = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut png), imagecrate::ImageFormat::Png).unwrap();
        png
    }

    fn viewer_with(sheets: &[(u16, Vec<u8>)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-portraits-{}-{:?}", std::process::id(), std::thread::current().id()));
        let dir = root.join("data").join("icon").join("download");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&dir).unwrap();
        for (sheet, bytes) in sheets {
            std::fs::write(sheet_path(&root, *sheet), bytes).unwrap();
        }
        root
    }

    #[test]
    fn portrait_ids_map_to_their_sheet_and_cell() {
        assert_eq!(cell(0), Some(Cell { sheet: 0, x: 0, y: 0 }));
        assert_eq!(cell(3), Some(Cell { sheet: 0, x: 192, y: 0 }));
        assert_eq!(cell(5), Some(Cell { sheet: 0, x: 64, y: 96 }));
        assert_eq!(cell(8 * 17 + 6), Some(Cell { sheet: 17, x: 128, y: 96 }));
        assert_eq!(cell(379 * 8 + 7), Some(Cell { sheet: 379, x: 192, y: 96 }));
        assert_eq!(cell(380 * 8), None);
        assert!(sheet_path(Path::new(r"C:\POL"), 7).ends_with(r"data\icon\download\hnf007.png"));
    }

    #[test]
    fn descrambling_restores_the_png_including_an_odd_last_byte() {
        for extra in [0usize, 1] {
            let mut png = sheet_png(192);
            png.extend(std::iter::repeat_n(0xab, extra));
            for seed in [0x1234_5678, 1, 0xffff_ffff] {
                let mut bytes = scramble(&png, seed);
                assert_ne!(bytes, png);
                assert!(descramble(&mut bytes));
                assert_eq!(bytes, png, "seed {seed:#x}, {} bytes", png.len());
            }
            let mut plain = png.clone();
            assert!(descramble(&mut plain), "a plain PNG has seed 0");
            assert_eq!(plain, png);
        }
        assert!(!descramble(&mut [1, 2, 3]));
        assert!(!descramble(&mut b"not a png at all".to_vec()));
    }

    #[test]
    fn a_header_claiming_another_size_is_refused_before_decoding() {
        assert!(sheet_size(&sheet_png(192)) && sheet_size(&sheet_png(256)));
        let mut huge = sheet_png(192);
        huge[16..24].copy_from_slice(&[0, 0, 0x40, 0, 0, 0, 0x40, 0]);
        assert!(!sheet_size(&huge), "16384 x 16384");
        let mut tall = sheet_png(192);
        tall[20..24].copy_from_slice(&257u32.to_be_bytes());
        assert!(!sheet_size(&tall));
        assert!(!sheet_size(&huge[..20]));
    }

    #[test]
    fn sheets_are_cut_into_cells_and_blank_cells_are_skipped() {
        let _turn = SHEETS_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        let viewer = viewer_with(&[(0, scramble(&sheet_png(192), 0xdead_beef)), (2, scramble(&sheet_png(256), 7)), (3, b"junk".to_vec())]);
        let cells = load_sheet(&viewer, 2).unwrap();
        for (index, pixels) in cells.iter().enumerate() {
            match index {
                6 | 7 => assert!(pixels.is_none(), "cell {index} is blank"),
                _ => {
                    let pixels = pixels.as_ref().unwrap();
                    assert_eq!(pixels.len(), (WIDTH * HEIGHT * 4) as usize);
                    assert_eq!(pixels[0], (index * 20 + 10) as u8, "cell {index}'s own pixels");
                    assert_eq!(&pixels[((5 * WIDTH + 3) * 4) as usize..][..4], [255, 0, 0, 255]);
                }
            }
        }
        assert!(load_sheet(&viewer, 1).is_none(), "missing");
        assert!(load_sheet(&viewer, 3).is_none(), "not a sheet");
        assert_eq!(available_in(&viewer), [0, 1, 2, 3, 4, 5, 16, 17, 18, 19, 20, 21]);
        let first = rgba_in(&viewer, 17).unwrap();
        assert_eq!(first[0], 30);
        assert!(Arc::ptr_eq(&first, &rgba_in(&viewer, 17).unwrap()), "decoded once");
        assert!(rgba_in(&viewer, 22).is_none());
        let _ = std::fs::remove_dir_all(viewer);
    }

    #[test]
    fn privacy_mode_shows_the_silhouette_and_nothing_without_a_portrait() {
        let _turn = SHEETS_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        crate::privacy::set(false);
        assert!(image(None).is_none());
        crate::privacy::set(true);
        // The silhouette, if this machine has a Viewer; never a real portrait.
        assert_eq!(image(Some(17)).is_some(), rgba(SILHOUETTE).is_some());
        crate::privacy::set(false);
    }

    #[test]
    fn the_picker_pages_its_choices_and_opens_on_yours() {
        let mut picker = Picker::default();
        assert_eq!((picker.pages(), picker.shown()), (0, &[][..]));
        let ids: Arc<[u16]> = (100..170).collect();
        picker.listed(ids.clone(), Some(140));
        assert_eq!((picker.pages(), picker.page), (3, 1));
        assert_eq!(picker.shown(), &ids[32..64]);
        picker.page = 2;
        assert_eq!(picker.shown(), &ids[64..]);
        picker.listed(ids, Some(7));
        assert_eq!(picker.page, 0, "a portrait not installed opens on the first page");
    }

    #[test]
    fn nothing_is_offered_for_writing_until_the_field_is_confirmed() {
        assert!(!PICKER_ENABLED);
        assert!(!unavailable().is_empty());
    }

    /// The installed Viewer's real sheets, when this machine has them.
    #[test]
    fn installed_viewer_sheets_decode() {
        let _turn = SHEETS_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(viewer) = viewer_dir() else { return eprintln!("no PlayOnline Viewer; skipped") };
        if !sheet_path(viewer, 0).is_file() {
            return eprintln!("no portrait sheets; skipped");
        }
        let mut bytes = std::fs::read(sheet_path(viewer, 0)).unwrap();
        assert!(descramble(&mut bytes));
        let image = imagecrate::load_from_memory_with_format(&bytes, imagecrate::ImageFormat::Png).unwrap();
        assert_eq!(image.width(), 256);
        assert!(matches!(image.height(), 192 | 256), "{}", image.height());
        assert!(rgba(SILHOUETTE).is_some(), "the default silhouette");
        let ids = available_in(viewer);
        assert!(ids.contains(&SILHOUETTE) && ids.windows(2).all(|w| w[0] < w[1]));
    }
}
