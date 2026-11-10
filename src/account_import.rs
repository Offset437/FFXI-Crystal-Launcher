//! Read-only account import. Sources are never changed, and drafts are never saved here.
//! Forest credentials are decrypted only after an explicit import request.

use crate::{i18n::t, backend::LaunchWith};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::{
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

mod pol;

const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACCOUNTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    PlayOnline,
    Forest,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PlayOnline => "PlayOnline",
            Self::Forest => "Forest",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportSource {
    pub kind: SourceKind,
    pub path: PathBuf,
    pub region: Option<u8>,
}

#[derive(Clone, Default)]
pub(crate) struct ImportBatch {
    pub drafts: Vec<ImportDraft>,
    pub warnings: Vec<String>,
}

// Never derive Debug for credential-bearing types, including account labels/IDs.
impl fmt::Debug for ImportBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportBatch")
            .field("draft_count", &self.drafts.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default)]
pub(crate) struct ImportDraft {
    pub label: String,
    pub pol_id: String,
    pub pol_password: String,
    pub se_id: String,
    pub se_password: String,
    pub region: u8,
    pub launch_with: Option<LaunchWith>,
    pub ashita_boot_ini: Option<String>,
    pub windower_profile: Option<String>,
    /// One-based position in the Viewer member list, not the reusable home index.
    pub pol_slot: Option<u8>,
    /// Preview metadata only. Importing never enables automatic character selection.
    pub character_slot: Option<u8>,
    pub notes: Vec<String>,
}

impl fmt::Debug for ImportDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ImportDraft([redacted])")
    }
}

impl Drop for ImportDraft {
    fn drop(&mut self) {
        self.label.zeroize();
        self.pol_id.zeroize();
        self.pol_password.zeroize();
        self.se_id.zeroize();
        self.se_password.zeroize();
    }
}

impl ImportDraft {
    pub fn missing_fields(&self) -> Vec<&'static str> {
        [
            (&self.pol_id, "PlayOnline ID"),
            (&self.pol_password, "PlayOnline password"),
            (&self.se_id, "Square Enix ID"),
            (&self.se_password, "Square Enix password"),
        ]
        .into_iter()
        .filter_map(|(value, name)| value.is_empty().then_some(name))
        .collect()
    }
}

/// Discovery checks names/metadata only; it does not read either credential store.
pub(crate) fn detect_sources() -> Vec<ImportSource> {
    let mut sources = Vec::new();
    for (region, root) in pol_installations() {
        let path = root.join("usr/all/login_w.bin");
        add_source(&mut sources, SourceKind::PlayOnline, path, Some(region));
    }
    if let Some(roaming) = std::env::var_os("APPDATA") {
        for folder in ["Forest", "POLAutoLogin"] {
            add_source(
                &mut sources,
                SourceKind::Forest,
                PathBuf::from(&roaming).join(folder).join("creds.dat"),
                None,
            );
        }
    }
    sources
}

fn add_source(
    sources: &mut Vec<ImportSource>,
    kind: SourceKind,
    path: PathBuf,
    region: Option<u8>,
) {
    if path.is_file()
        && !sources.iter().any(|s| {
            s.path
                .to_string_lossy()
                .eq_ignore_ascii_case(&path.to_string_lossy())
        })
    {
        sources.push(ImportSource { kind, path, region });
    }
}

/// Imports the chosen file into memory. No account vault or source file is written.
/// `login_w.bin` needs its original `noramim.bin` alongside it on current installs.
pub(crate) fn import_file(path: &Path, region: u8) -> Result<ImportBatch, String> {
    if region > 2 {
        return Err(t("Choose US, EU or JP.").into());
    }
    let data = read_bounded(path, MAX_FILE_BYTES)?;
    if data.starts_with(b"PlayOnline Login Setting File Ver ") {
        return pol::import(path, &data, region);
    }
    if path
        .extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("bin"))
    {
        return Err(t("This PlayOnline file format is not supported. Choose usr\\all\\login_w.bin, with its original noramim.bin in the same folder.").into());
    }
    import_forest(&data, region, unprotect_password)
}

/// Explicitly combine a Forest preview with a chosen PlayOnline preview. Matching uses
/// the saved member-list position, so the editor must still show the imported IDs.
/// Existing fields always win; ambiguous matches are left for the user to complete.
pub(crate) fn fill_from_pol(forest: &mut ImportBatch, members: &ImportBatch) {
    for draft in &mut forest.drafts {
        let Some(slot) = draft.pol_slot else { continue };
        let mut matches = members.drafts.iter().filter(|p| {
            p.pol_slot == Some(slot)
                && p.region == draft.region
                && p.pol_id.len() == 8
                && p.pol_id.bytes().all(|b| b.is_ascii_alphanumeric())
        });
        let Some(member) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            continue;
        }
        if (!draft.pol_id.is_empty() && draft.pol_id != member.pol_id)
            || (!draft.se_id.is_empty() && draft.se_id != member.se_id)
        {
            continue;
        }
        let mut filled = false;
        if draft.pol_id.is_empty() {
            draft.pol_id = member.pol_id.clone();
            filled = true;
        }
        if draft.pol_password.is_empty() && !member.pol_password.is_empty() {
            draft.pol_password = member.pol_password.clone();
            filled = true;
        }
        if draft.se_id.is_empty() && !member.se_id.is_empty() {
            draft.se_id = member.se_id.clone();
            filled = true;
        }
        if filled {
            draft.notes.push(
                t("Matched the saved PlayOnline member slot. Verify both account IDs before saving.")
                    .into(),
            );
        }
        for note in &member.notes {
            if !draft.notes.contains(note) {
                draft.notes.push(note.clone());
            }
        }
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let file = File::open(path).map_err(|_| {
        t("The import file could not be opened. Check that it still exists and is readable.")
    })?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= limit as u64)
    {
        return Err(t("The import file is too large or is not a regular file.").into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| t("The import file could not be read."))?;
    if bytes.len() > limit {
        return Err(t("The import file is too large.").into());
    }
    Ok(bytes)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ForestRecord {
    #[serde(default)]
    enc_password: String,
    #[serde(default)]
    enc_totp_secret: Option<String>,
    #[serde(default)]
    windower_profile: Option<String>,
    #[serde(default)]
    ashita_boot_ini: Option<String>,
    #[serde(default)]
    pol_slot: i64,
    #[serde(default)]
    launcher: Option<String>,
    #[serde(default)]
    ingame_slot: i64,
}

impl Drop for ForestRecord {
    fn drop(&mut self) {
        self.enc_password.zeroize();
        self.enc_totp_secret.zeroize();
    }
}

struct ForestEntry {
    label: String,
    record: ForestRecord,
}
impl Drop for ForestEntry {
    fn drop(&mut self) {
        self.label.zeroize();
    }
}
struct ForestEntries(Vec<ForestEntry>);

impl<'de> Deserialize<'de> for ForestEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = ForestEntries;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Forest account map")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries: Vec<ForestEntry> = Vec::new();
                while let Some(label) = map.next_key::<String>()? {
                    let label = Zeroizing::new(label);
                    if entries.len() >= MAX_ACCOUNTS
                        || label.trim().is_empty()
                        || label.len() > 256
                        || label.chars().any(char::is_control)
                        || entries
                            .iter()
                            .any(|e| e.label.to_lowercase() == label.to_lowercase())
                    {
                        return Err(serde::de::Error::custom(
                            "invalid or duplicate account entry",
                        ));
                    }
                    let record = map.next_value()?;
                    entries.push(ForestEntry {
                        label: label.to_string(),
                        record,
                    });
                }
                Ok(ForestEntries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}

fn import_forest(
    bytes: &[u8],
    region: u8,
    decrypt: impl Fn(&[u8]) -> Result<Zeroizing<Vec<u8>>, String>,
) -> Result<ImportBatch, String> {
    // `import_file` read at most MAX_FILE_BYTES (`read_bounded`).
    // File.ReadAllText also accepts a UTF-8 BOM. No other text encodings are guessed.
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let entries: ForestEntries = serde_json::from_slice(bytes)
        .map_err(|_| t("This is not a valid Forest credentials file, or it contains duplicate or unsupported account entries."))?;
    let mut batch = ImportBatch::default();
    for mut entry in entries.0 {
        let record = &entry.record;
        let mut draft = ImportDraft::default();
        draft.label = std::mem::take(&mut entry.label);
        draft.region = region;
        draft.pol_slot = (1..=20)
            .contains(&record.pol_slot)
            .then_some(record.pol_slot as u8);
        draft.character_slot = (1..=16)
            .contains(&record.ingame_slot)
            .then_some(record.ingame_slot as u8);
        draft.launch_with = match record.launcher.as_deref() {
            Some(value) if value.eq_ignore_ascii_case("windower") => Some(LaunchWith::Windower),
            Some(value) if value.eq_ignore_ascii_case("ashita") => Some(LaunchWith::Ashita),
            _ => None,
        };
        if let Some(name) = &record.windower_profile {
            if !name.is_empty()
                && name.len() <= 128
                && !name.contains('"')
                && !name.chars().any(char::is_control)
            {
                draft.windower_profile = Some(name.clone());
            } else if !name.is_empty() {
                draft.notes.push(t("The saved Windower profile name was not imported because its format is unsupported.").into());
            }
        }
        if let Some(name) = &record.ashita_boot_ini {
            if crate::pol_less::tools::valid_boot_ini_name(name) {
                draft.ashita_boot_ini = Some(name.clone());
            } else if !name.is_empty() {
                draft.notes.push(t("The saved Ashita configuration name was not imported because its format is unsupported.").into());
            }
        }
        if record
            .enc_totp_secret
            .as_ref()
            .is_some_and(|s| !s.is_empty())
        {
            draft.notes.push(t("Forest has an authenticator secret for this account. Crystal will continue to ask for a one-time code.").into());
        }
        match decode_password(&record.enc_password, &decrypt) {
            Ok(password) => draft.se_password = password,
            Err(()) => draft.notes.push(t("The Square Enix password could not be imported. Enter it manually, or import while signed in to the Windows account that saved it.").into()),
        }
        draft.notes.push(t("Forest does not store the PlayOnline ID, PlayOnline password or Square Enix ID. Complete any missing fields before saving.").into());
        batch.drafts.push(draft);
    }
    if batch.drafts.is_empty() {
        batch
            .warnings
            .push(t("No saved accounts were found in this file.").into());
    }
    Ok(batch)
}

fn decode_password(
    encoded: &str,
    decrypt: &impl Fn(&[u8]) -> Result<Zeroizing<Vec<u8>>, String>,
) -> Result<String, ()> {
    let encrypted = decode_base64(encoded)?;
    let plain = decrypt(&encrypted).map_err(|_| ())?;
    if plain.is_empty() || plain.len() > 512 || plain.contains(&0) {
        return Err(());
    }
    std::str::from_utf8(&plain)
        .map(str::to_owned)
        .map_err(|_| ())
}

fn decode_base64(input: &str) -> Result<Zeroizing<Vec<u8>>, ()> {
    if input.len() > 16_384 {
        return Err(());
    }
    // .NET's Convert.ToBase64String output, possibly wrapped across lines.
    let text: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if text.is_empty() {
        return Err(());
    }
    crate::b64::decode_padded(&text).map(Zeroizing::new).ok_or(())
}

fn unprotect_password(input: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    crate::win::unprotect(input)
        .ok()
        .filter(|password| !password.is_empty() && password.len() <= 512)
        .ok_or_else(|| t("Windows could not decrypt this account password.").into())
}

/// The PlayOnline Viewer folders the registry names, per region (HKLM and HKCU, both views).
fn pol_installations() -> Vec<(u8, PathBuf)> {
    crate::win::reg::install_folders("1000", &crate::win::reg::REGIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forest_import_is_partial_and_does_not_guess_ids_or_enable_character_selection() {
        let data = br#"{"Demo":{"EncPassword":"YQ==","PolSlot":2,"IngameSlot":3,"Launcher":"Windower","WindowerProfile":"Example","LaunchArgs":"untrusted","EncTotpSecret":"ignored"}}"#;
        let batch = import_forest(data, 1, |_| {
            Ok(Zeroizing::new(b"synthetic-password".to_vec()))
        })
        .unwrap();
        let draft = &batch.drafts[0];
        assert_eq!(draft.label, "Demo");
        assert_eq!(draft.se_password, "synthetic-password");
        assert_eq!(draft.pol_slot, Some(2));
        assert_eq!(draft.character_slot, Some(3));
        assert_eq!(
            draft.missing_fields(),
            ["PlayOnline ID", "PlayOnline password", "Square Enix ID"]
        );
        assert!(!format!("{batch:?} {draft:?}").contains("synthetic-password"));
        assert!(!format!("{batch:?} {draft:?}").contains("Demo"));
    }

    #[test]
    fn unreadable_password_keeps_editable_metadata_and_never_echoes_input() {
        let batch = import_forest(br#"{"Demo":{"EncPassword":"YQ=="}}"#, 0, |_| {
            Err("sensitive detail".into())
        })
        .unwrap();
        assert_eq!(batch.drafts.len(), 1);
        assert!(batch.drafts[0].se_password.is_empty());
        assert!(!batch.drafts[0].notes.join(" ").contains("sensitive detail"));
        let bad_utf8 = import_forest(br#"{"Demo":{"EncPassword":"YQ=="}}"#, 0, |_| {
            Ok(Zeroizing::new(vec![0xff]))
        })
        .unwrap();
        assert!(bad_utf8.drafts[0].se_password.is_empty());
    }

    #[test]
    fn forest_rejects_ambiguous_maps_and_unsafe_metadata() {
        for input in [
            br#"{"Demo":{},"demo":{}}"#.as_slice(),
            br#"{"":{}}"#,
            b"[]",
            b"{} trailing",
        ] {
            assert!(import_forest(input, 0, |_| unreachable!()).is_err());
        }
        let batch = import_forest(br#"{"Demo":{"PolSlot":-1,"IngameSlot":90,"WindowerProfile":"bad\nname","AshitaBootIni":"../escape.ini"}}"#, 0, |_| unreachable!()).unwrap();
        let d = &batch.drafts[0];
        assert!(d.pol_slot.is_none() && d.character_slot.is_none());
        assert!(d.windower_profile.is_none() && d.ashita_boot_ini.is_none());
    }

    #[test]
    fn base64_accepts_dotnet_output_and_rejects_truncation_and_hidden_padding() {
        assert_eq!(&*decode_base64(" Zm9v\r\nYmFy ").unwrap(), b"foobar");
        assert_eq!(&*decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(&*decode_base64("Zm8=").unwrap(), b"fo");
        for bad in ["", "Zg=", "Zg==AAAA", "Zh==", "Zm9=", "====", "%%%%"] {
            assert!(decode_base64(bad).is_err());
        }
    }

    #[test]
    fn windows_dpapi_recovers_only_the_synthetic_current_user_password() {
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::Cryptography::{
                CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
            },
        };
        let password = Zeroizing::new(b"synthetic import password".to_vec());
        let input = CRYPT_INTEGER_BLOB {
            cbData: password.len() as u32,
            pbData: password.as_ptr() as _,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let success = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        assert_ne!(success, 0);
        let protected =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(output.pbData as _);
        }
        assert_eq!(&*unprotect_password(&protected).unwrap(), &*password);
        let mut corrupted = protected;
        let last = corrupted.len() - 1;
        corrupted[last] ^= 1;
        assert!(unprotect_password(&corrupted).is_err());
    }

    #[test]
    fn joining_pol_members_never_replaces_existing_fields_or_uses_ambiguous_slots() {
        let mut member = ImportDraft::default();
        member.pol_slot = Some(1);
        member.pol_id = "TEST0001".into();
        member.pol_password = "synthetic-pol".into();
        member.se_id = "sample".into();
        let members = ImportBatch {
            drafts: vec![member],
            warnings: vec![],
        };
        let mut draft = ImportDraft::default();
        draft.pol_slot = Some(1);
        draft.pol_password = "keep-pol".into();
        draft.se_password = "keep".into();
        let mut batch = ImportBatch {
            drafts: vec![draft],
            warnings: vec![],
        };
        fill_from_pol(&mut batch, &members);
        assert_eq!(batch.drafts[0].pol_id, "TEST0001");
        assert_eq!(batch.drafts[0].pol_password, "keep-pol");
        assert_eq!(batch.drafts[0].se_password, "keep");
        batch.drafts[0].pol_id = "OTHER001".into();
        batch.drafts[0].pol_password.clear();
        batch.drafts[0].se_id.clear();
        fill_from_pol(&mut batch, &members);
        assert!(batch.drafts[0].pol_password.is_empty());
        assert!(batch.drafts[0].se_id.is_empty());
        let mut ambiguous = members.clone();
        ambiguous.drafts.push(members.drafts[0].clone());
        batch.drafts[0].pol_id.clear();
        fill_from_pol(&mut batch, &ambiguous);
        assert!(batch.drafts[0].pol_id.is_empty());
        assert!(batch.drafts[0].pol_password.is_empty());
    }

    #[test]
    fn forest_and_pol_passwords_combine_into_a_complete_import() {
        let mut batch = import_forest(
            br#"{"Demo":{"EncPassword":"YQ==","PolSlot":1}}"#,
            0,
            |_| Ok(Zeroizing::new(b"synthetic-se".to_vec())),
        )
        .unwrap();
        let mut member = ImportDraft::default();
        member.pol_slot = Some(1);
        member.pol_id = "TEST0001".into();
        member.pol_password = "synthetic-pol".into();
        member.se_id = "sample".into();
        let members = ImportBatch {
            drafts: vec![member],
            warnings: vec![],
        };

        fill_from_pol(&mut batch, &members);

        assert!(batch.drafts[0].missing_fields().is_empty());
        assert_eq!(batch.drafts[0].pol_password, "synthetic-pol");
        assert_eq!(batch.drafts[0].se_password, "synthetic-se");
        assert_eq!(members.drafts[0].pol_password, "synthetic-pol");
    }
}
