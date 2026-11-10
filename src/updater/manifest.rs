//! Crystal's parser and streaming file signatures for official patch manifests.
//! A file block contains an ordered version history. Viewer manifests can
//! additionally wrap blocks in Option or Reset2 sections. An end marker is
//! required before a manifest can be used for installation.
//! Format evidence: installed POL core 73b1864b, 1004011e/10040060/10038fed.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileSig {
    pub len: u64,
    pub byte_sum: i32,
    pub md5_word: i32,
}

struct Signature {
    bytes: u64,
    sum: i32,
    digest: md5::Context,
}

impl Signature {
    fn new() -> Self {
        Self {
            bytes: 0,
            sum: 0,
            digest: md5::Context::new(),
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        // Widen each bounded chunk so LLVM can vectorize the reduction.
        for chunk in bytes.chunks(1024 * 1024) {
            let sum: i64 = chunk.iter().map(|&byte| i64::from(byte as i8)).sum();
            self.sum = self.sum.wrapping_add(sum as i32);
        }
        self.digest.consume(bytes);
    }

    fn finish(self) -> FileSig {
        let digest = self.digest.compute();
        FileSig {
            len: self.bytes,
            byte_sum: self.sum,
            md5_word: i32::from_le_bytes(digest.0[..4].try_into().unwrap()),
        }
    }
}

impl FileSig {
    pub fn of(bytes: &[u8]) -> Self {
        let mut signature = Signature::new();
        signature.append(bytes);
        signature.finish()
    }

    pub fn of_path(path: &Path) -> Result<Self, String> {
        let failure = |error| format!("{}: {error}", path.display());
        let mut reader = BufReader::with_capacity(1024 * 1024, File::open(path).map_err(failure)?);
        let mut signature = Signature::new();
        loop {
            let bytes = reader.fill_buf().map_err(failure)?;
            if bytes.is_empty() {
                return Ok(signature.finish());
            }
            let count = bytes.len();
            signature.append(bytes);
            reader.consume(count);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub path: String,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub stamp: String,
    pub sig: FileSig,
    pub direct: String,
    pub direct_len: u64,
    pub indirect: Option<Delta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistory {
    pub path: String,
    pub optional: bool,
    pub versions: Vec<Version>,
}

impl FileHistory {
    pub fn current(&self) -> &Version {
        self.versions
            .last()
            .expect("manifest histories contain at least one version")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub files: Vec<FileHistory>,
}

struct Words<'a>(&'a str);

impl<'a> Words<'a> {
    fn word(&mut self) -> Result<&'a str, String> {
        self.0 = self.0.trim_start();
        if self.0.is_empty() {
            return Err("missing manifest field".into());
        }
        let end = self.0.find(char::is_whitespace).unwrap_or(self.0.len());
        let value = &self.0[..end];
        self.0 = self.0[end..].trim_start();
        Ok(value)
    }

    fn number<T: std::str::FromStr>(&mut self) -> Result<T, String> {
        self.word()?
            .parse()
            .map_err(|_| "invalid manifest number".into())
    }

    fn transfer(&mut self, suffix: &str) -> Result<(String, u64), String> {
        // Filenames contain spaces. A transfer ends with a format suffix
        // followed by its numeric wire length, rather than at a word boundary.
        for (start, _) in self.0.match_indices(suffix) {
            let end = start + suffix.len();
            if !self.0[end..].starts_with(' ') {
                continue;
            }
            let mut remaining = Words(&self.0[end..]);
            if let Ok(length) = remaining.number() {
                let path = self.0[..end].to_owned();
                self.0 = remaining.0;
                return Ok((path, length));
            }
        }
        Err(format!("missing {suffix} transfer path or length"))
    }

    fn version(mut self) -> Result<Version, String> {
        let stamp = self.word()?.to_owned();
        let sig = FileSig {
            len: self.number()?,
            byte_sum: self.number()?,
            md5_word: self.number()?,
        };
        let (direct, direct_len) = self.transfer(".slc")?;
        let indirect = if self.0.is_empty() {
            None
        } else {
            let (path, len) = self.transfer(".olc")?;
            Some(Delta { path, len })
        };
        if !self.0.is_empty() {
            return Err("unexpected fields after manifest version".into());
        }
        Ok(Version {
            stamp,
            sig,
            direct,
            direct_len,
            indirect,
        })
    }
}

struct Scanner<'a> {
    lines: std::iter::Enumerate<std::str::Lines<'a>>,
    viewer: bool,
}

impl<'a> Scanner<'a> {
    fn line(&mut self) -> Option<(usize, &'a str)> {
        self.lines
            .find(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| (index + 1, line.trim_end_matches('\r')))
    }

    fn file(&mut self, path: &str, optional: bool) -> Result<Option<FileHistory>, String> {
        let mut history = FileHistory {
            path: path.to_owned(),
            optional,
            versions: Vec::new(),
        };
        while let Some((number, line)) = self.line() {
            if line == "}" {
                return Ok((!history.versions.is_empty()).then_some(history));
            }
            let version = Words(line)
                .version()
                .map_err(|error| format!("line {number}: {error}"))?;
            history.versions.push(version);
        }
        Err("manifest ends inside a file block".into())
    }

    fn scope(
        &mut self,
        files: &mut Vec<FileHistory>,
        optional: bool,
        section: bool,
    ) -> Result<(), String> {
        while let Some((number, line)) = self.line() {
            match line {
                "end" if !section => return Ok(()),
                "}" if section => return Ok(()),
                "Option {" | "Reset2 {" if self.viewer && !section => {
                    self.scope(files, line == "Option {", true)?;
                }
                _ => {
                    let path = line
                        .strip_prefix("file ")
                        .and_then(|s| s.strip_suffix(" {"))
                        .ok_or_else(|| {
                            format!("line {number}: expected a file block, got {line:?}")
                        })?;
                    if let Some(file) = self.file(path, optional)? {
                        files.push(file);
                    }
                }
            }
        }
        Err(if section {
            "manifest ends inside a section"
        } else {
            "manifest is truncated: missing terminal end"
        }
        .into())
    }
}

fn read_manifest(text: &str, viewer: bool) -> Result<Manifest, String> {
    let mut scanner = Scanner {
        lines: text.lines().enumerate(),
        viewer,
    };
    let mut manifest = Manifest::default();
    scanner.scope(&mut manifest.files, false, false)?;
    Ok(manifest)
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    read_manifest(text, false)
}

pub fn parse_viewer(text: &str) -> Result<Manifest, String> {
    read_manifest(text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(length: usize) -> Vec<u8> {
        (0..length)
            .map(|i| (i.wrapping_mul(19) ^ i.rotate_left(3)) as u8)
            .collect()
    }

    fn reference(bytes: &[u8]) -> FileSig {
        let sum: i64 = bytes.iter().map(|&byte| i64::from(byte as i8)).sum();
        FileSig {
            len: bytes.len() as u64,
            byte_sum: sum as i32,
            md5_word: i32::from_le_bytes(md5::compute(bytes).0[..4].try_into().unwrap()),
        }
    }

    fn entry(stamp: &str, bytes: &[u8], source: &str, delta: bool) -> (String, Version) {
        let direct = format!("{stamp}/Direct/{source}.slc");
        let indirect = delta.then(|| Delta {
            path: format!("{stamp}/Indirect/{source}.olc"),
            len: 43,
        });
        let sig = reference(bytes);
        let mut line = format!(
            "{stamp} {} {} {} {direct} 71",
            sig.len, sig.byte_sum, sig.md5_word
        );
        if let Some(delta) = &indirect {
            line.push_str(&format!(" {} {}", delta.path, delta.len));
        }
        (
            line,
            Version {
                stamp: stamp.into(),
                sig,
                direct,
                direct_len: 71,
                indirect,
            },
        )
    }

    #[test]
    fn synthetic_histories_preserve_each_field_and_ignore_uncommitted_tail() {
        let paths = [
            "Docs/Offline spec.txt",
            "ROM/902/57.DAT",
            "Tools/a.slc archive/config guide.chm",
        ];
        let mut text = String::new();
        let mut expected = Vec::new();
        for (index, path) in paths.into_iter().enumerate() {
            text.push_str(&format!("file {path} {{\n"));
            let mut versions = Vec::new();
            for (revision, delta) in [(7, false), (108, true)] {
                let stamp = format!("30270102_{revision}");
                let (line, version) =
                    entry(&stamp, &sample(89 + index * 17 + revision), path, delta);
                text.push_str(&format!("{line}\n"));
                versions.push(version);
            }
            text.push_str("}\n");
            expected.push(FileHistory {
                path: path.into(),
                optional: false,
                versions,
            });
        }
        text.push_str("file unused-empty-record {\n}\nend\nunfinished bytes after committed end");
        let manifest = parse(&text.replace('\n', "\r\n")).unwrap();
        assert_eq!(manifest.files, expected);
        assert_eq!(crate::updater::Product::Ffxi.latest_stamp(&manifest), Some("30270102_108"));
        for file in manifest.files {
            assert_eq!(file.current().stamp, "30270102_108");
        }
    }

    #[test]
    fn viewer_scope_flags_do_not_leak_into_following_files() {
        let (line, _) = entry("30270103_1", &sample(9), "scope.dat", false);
        let text = format!(
            "Option {{\nfile optional.data {{\n{line}\n}}\n}}\nReset2 {{\nfile reset.data {{\n{line}\n}}\n}}\nfile plain.data {{\n{line}\n}}\nend\n"
        );
        let manifest = parse_viewer(&text).unwrap();
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|file| file.optional)
                .collect::<Vec<_>>(),
            [true, false, false]
        );
        assert!(parse(&text).is_err());
        assert!(parse_viewer(&text.replace("}\nReset2", "Reset2")).is_err());
        assert!(parse_viewer(&text.replace("Option {", "Option {\nReset2 {")).is_err());
        assert!(parse_viewer("Option {\nend\n").is_err());
        assert_eq!(crate::updater::Product::Ffxi.latest_stamp(&parse("end\n").unwrap()), None);
    }

    #[test]
    fn malformed_numbers_transfers_and_unclosed_blocks_cannot_be_installed() {
        let valid = "30270102_108 81 -92 123 fixture.slc 71 fixture.olc 43";
        let broken = [
            valid.replacen("81", "-81", 1),
            valid.replacen("81", "18446744073709551616", 1),
            valid.replacen("-92", "2147483648", 1),
            valid.replacen("123", "-2147483649", 1),
            valid.replace(".slc", ".zip"),
            valid.replace(".olc", ".zip"),
            valid.replace(".slc 71", ".slc -71"),
            valid.replace(".olc 43", ".olc -43"),
            valid.replace(".slc 71", ".slc NaN"),
            format!("{valid} extra"),
            String::new(),
        ];
        for line in broken.into_iter().filter(|line| !line.is_empty()) {
            assert!(
                parse(&format!("file synthetic.data {{\n{line}\n}}\nend\n")).is_err(),
                "{line}"
            );
        }
        for text in [
            format!("file synthetic.data {{\n{valid}\n"),
            format!("file synthetic.data {{\n{valid}\n}}\n"),
            format!("file synthetic.data {{\n{valid}\nend\n"),
            format!("{valid}\nend\n"),
            "}\nend\n".into(),
        ] {
            assert!(parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn signatures_match_wide_signed_arithmetic_including_overflow() {
        for length in [0, 3, 127, 8193, 65537] {
            let bytes = sample(length);
            assert_eq!(FileSig::of(&bytes), reference(&bytes));
        }
        let bytes = vec![128; (1 << 24) + 3];
        assert_eq!(FileSig::of(&bytes), reference(&bytes));
    }

    #[test]
    fn disk_signatures_cross_buffer_boundaries_and_report_missing_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("crystal-signature-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("synthetic-file.bin");
        for length in [0, 17, (1 << 20) - 1, (1 << 20) + 1, (2 << 20) + 137] {
            let bytes = sample(length);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(FileSig::of_path(&path).unwrap(), reference(&bytes));
        }
        std::fs::remove_file(&path).unwrap();
        assert!(FileSig::of_path(&path).is_err());
        std::fs::remove_dir(&directory).unwrap();
    }
}
