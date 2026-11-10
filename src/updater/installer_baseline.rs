//! Identities from Square Enix's pinned US full installer. These supplement
//! retained patch history; they never describe the desired/current version.
use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
struct Identity { len: u64, md5: [u8; 16] }
pub(super) struct Baseline(HashMap<String, Identity>);

impl Baseline {
    pub(super) fn matches(&self, path: &str, len: u64, md5: [u8; 16]) -> bool {
        self.0.get(&path.replace('\\', "/").to_ascii_lowercase())
            .is_some_and(|identity| identity.len == len && identity.md5 == md5)
    }
}

pub(super) fn load() -> Result<&'static Baseline, String> {
    static BASELINE: OnceLock<Result<Baseline, String>> = OnceLock::new();
    BASELINE.get_or_init(|| {
        let compressed = include_bytes!("fixtures/ffxi-us-2019-baseline.tsv.gz");
        let mut text = String::new();
        flate2::read::GzDecoder::new(&compressed[..]).take(4 * 1024 * 1024)
            .read_to_string(&mut text).map_err(|e| format!("Installer baseline: {e}"))?;
        let mut files = HashMap::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 3 { return Err("Invalid embedded installer baseline".into()); }
            let path = fields[0];
            if path.is_empty() || path.contains(['\\', ':']) || path != path.to_ascii_lowercase()
                || path.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                return Err("Invalid embedded installer path".into());
            }
            let len = fields[1].parse::<u64>().map_err(|_| "Invalid embedded installer length")?;
            if fields[2].len() != 32 { return Err("Invalid embedded installer digest".into()); }
            let mut md5 = [0u8; 16];
            for (value, pair) in md5.iter_mut().zip(fields[2].as_bytes().as_chunks::<2>().0) {
                let pair = std::str::from_utf8(pair).map_err(|_| "Invalid embedded installer digest")?;
                *value = u8::from_str_radix(pair, 16).map_err(|_| "Invalid embedded installer digest")?;
            }
            if files.insert(path.into(), Identity { len, md5 }).is_some() {
                return Err("Duplicate embedded installer path".into());
            }
        }
        if files.len() != 61_349 { return Err("Incomplete embedded installer baseline".into()); }
        Ok(Baseline(files))
    }).as_ref().map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn pinned_installer_identities_are_complete_and_bound_to_path_length_and_full_digest() {
        let bytes = include_bytes!("fixtures/ffxi-us-2019-baseline.tsv.gz");
        assert_eq!(format!("{:x}", Sha256::digest(bytes)),
            "be85528a617a559ec90170aae4c4fb29a26c67a428d554954ab90ec86281a26f");
        let baseline = load().unwrap();
        let identity = baseline.0.get("ffximain.dll").unwrap();
        assert_eq!(identity.len, 2_869_840);
        assert!(baseline.matches("FFXiMain.dll", identity.len, identity.md5));
        assert!(!baseline.matches("FFXiResource.dll", identity.len, identity.md5));
        assert!(!baseline.matches("FFXiMain.dll", identity.len + 1, identity.md5));
        let mut changed = identity.md5;
        changed[15] ^= 1; // first four MD5 bytes alone cannot identify stock files
        assert!(!baseline.matches("FFXiMain.dll", identity.len, changed));
        assert!(!baseline.matches("../FFXiMain.dll", identity.len, identity.md5));
    }
}
