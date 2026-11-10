//! The launcher's own UI language (not the game's: FFXI follows its
//! PlayOnline region and is never changed from here).
//!
//! English text is the key. Wrap a UI string literal where it is written:
//! `t("Settings")` returns the Japanese text when Japanese is on and the table
//! has an entry, else the English text, so a missing entry is never blank.
//! Text with values uses named placeholders, which a translation may reorder:
//! `tf("Signed in as {name}", &[("name", &label)])`.
mod ja;

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Write};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Lang {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ja")]
    Ja,
}

impl Lang {
    pub(crate) const ALL: [Lang; 2] = [Lang::En, Lang::Ja];
}

/// Each language is named in itself, whatever language is showing.
impl Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Lang::En => "English",
            Lang::Ja => "日本語",
        })
    }
}

// The view runs on one thread, so a plain atomic carries the language. Tests
// run in parallel threads, so there each thread has its own and a test that
// switches to Japanese cannot change another test's English text.
#[cfg(not(test))]
static CURRENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::Cell<Lang> = const { std::cell::Cell::new(Lang::En) };
}

#[cfg(not(test))]
pub(crate) fn set(lang: Lang) {
    CURRENT.store(lang as u8, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(test)]
pub(crate) fn set(lang: Lang) {
    CURRENT.with(|c| c.set(lang));
}

#[cfg(not(test))]
pub(crate) fn current() -> Lang {
    match CURRENT.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Lang::Ja,
        _ => Lang::En,
    }
}
#[cfg(test)]
pub(crate) fn current() -> Lang {
    CURRENT.with(|c| c.get())
}

/// The UI text for `en` in the current language.
pub(crate) fn t(en: &'static str) -> &'static str {
    lookup(current(), en)
}

/// `t` with `{name}` placeholders filled from `args`. A placeholder without an
/// argument is left as written, so a mistake shows instead of vanishing.
pub(crate) fn tf(en: &'static str, args: &[(&str, &dyn Display)]) -> String {
    fill(t(en), args)
}

/// The text for `en` in `lang`, whatever language is showing. For reading
/// back text made earlier in another language; UI code uses `t`.
pub(crate) fn t_in(lang: Lang, en: &'static str) -> &'static str {
    lookup(lang, en)
}

/// `tf` in `lang`, whatever language is showing.
pub(crate) fn tf_in(lang: Lang, en: &'static str, args: &[(&str, &dyn Display)]) -> String {
    fill(lookup(lang, en), args)
}

fn lookup(lang: Lang, en: &'static str) -> &'static str {
    match lang {
        Lang::En => en,
        Lang::Ja => ja::JA
            .binary_search_by(|(key, _)| (*key).cmp(en))
            .map_or(en, |i| ja::JA[i].1),
    }
}

fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after
            .find('}')
            .and_then(|close| args.iter().find(|(name, _)| *name == &after[..close]).map(|(_, v)| (close, v)));
        match value {
            Some((close, value)) => {
                let _ = write!(out, "{value}");
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The language a launcher without a saved choice starts in: Japanese when
/// the Windows display language is Japanese, else English.
pub(crate) fn system_default() -> Lang {
    const LANG_JAPANESE: u16 = 0x11;
    // The low 10 bits of a LANGID are the primary language.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == LANG_JAPANESE { Lang::Ja } else { Lang::En }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    #[test]
    fn english_is_the_key_and_a_missing_entry_falls_back_to_it() {
        set(Lang::En);
        assert_eq!(t("Settings"), "Settings");
        set(Lang::Ja);
        assert_eq!(t("Settings"), "設定");
        assert_eq!(lookup(Lang::Ja, "No such entry, kept in English"), "No such entry, kept in English");
        set(Lang::En);
    }

    #[test]
    fn placeholders_fill_by_name_in_any_order() {
        assert_eq!(fill("{b} then {a}", &[("a", &1), ("b", &"two")]), "two then 1");
        assert_eq!(fill("{a}{a}", &[("a", &'x')]), "xx");
        // Unknown or unclosed placeholders are left visible.
        assert_eq!(fill("{missing} and {open", &[("a", &1)]), "{missing} and {open");
        assert_eq!(fill("no braces", &[]), "no braces");
    }

    #[test]
    fn language_is_stored_as_a_short_code_and_named_in_itself() {
        assert_eq!(serde_json::to_string(&Lang::Ja).unwrap(), "\"ja\"");
        assert_eq!(serde_json::from_str::<Lang>("\"en\"").unwrap(), Lang::En);
        assert_eq!(Lang::En.to_string(), "English");
        set(Lang::Ja);
        assert_eq!(Lang::En.to_string(), "English");
        assert_eq!(Lang::Ja.to_string(), "日本語");
        set(Lang::En);
    }

    #[test]
    fn japanese_table_is_sorted_unique_and_complete() {
        for pair in ja::JA.windows(2) {
            assert!(pair[0].0 < pair[1].0, "ja.rs must stay sorted by key with no duplicates: {:?} then {:?}", pair[0].0, pair[1].0);
        }
        for (key, value) in ja::JA {
            assert!(!value.trim().is_empty(), "empty Japanese text for {key:?}");
            assert_eq!(placeholders(key), placeholders(value), "placeholders differ between {key:?} and {value:?}");
        }
    }

    #[test]
    fn every_japanese_entry_is_still_used() {
        let used: BTreeSet<String> = ui_strings().into_keys().collect();
        let stale: Vec<&str> = ja::JA.iter().map(|(key, _)| *key).filter(|key| !used.contains(*key)).collect();
        assert!(stale.is_empty(), "ja.rs entries no t()/tf() call uses any more (remove them): {stale:#?}");
    }

    /// A new `t()`/`tf()` string needs its Japanese in `ja.rs`;
    /// the failure lists each one with `file:line`.
    #[test]
    fn every_ui_string_has_japanese() {
        let missing: Vec<(String, String)> = ui_strings()
            .into_iter()
            .filter(|(key, _)| ja::JA.binary_search_by(|(k, _)| (*k).cmp(key.as_str())).is_err())
            .collect();
        let list: String = missing.iter().map(|(key, at)| format!("\n  {at}: {key:?}")).collect();
        assert!(missing.is_empty(), "{} t()/tf() strings have no Japanese entry:{list}", missing.len());
    }
    #[test]
    fn scanner_reads_keys_and_skips_comments_and_raw_strings() {
        let source = r####"
            // t("in a comment")
            let a = t("Plain");
            let b = i18n::tf(
                "Line {n}\n\"quoted\" \u{65e5}", &[("n", &1)]);
            let c = r#"t("in a raw string")"#;
            let d = split("not a key"); let e = 't'; fn f<'a>(x: &'a str) {}
            let g = t("After \
                       continuation");
        "####;
        let keys: Vec<String> = keys_in(source).into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["Plain", "Line {n}\n\"quoted\" 日", "After continuation"]);
    }

    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(close) = rest.find('}') {
                names.insert(&rest[..close]);
                rest = &rest[close + 1..];
            }
        }
        names
    }

    /// Every `t("…")` / `tf("…")` key in `src`, with its first `file:line`.
    fn ui_strings() -> BTreeMap<String, String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut keys = BTreeMap::new();
        for file in files {
            if file.ends_with(Path::new("i18n").join("ja.rs")) {
                continue;
            }
            let source = std::fs::read_to_string(&file).unwrap();
            let name = file.strip_prefix(&root).unwrap().display().to_string();
            for (key, line) in keys_in(&source) {
                keys.entry(key).or_insert_with(|| format!("src\\{name}:{line}"));
            }
        }
        keys
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// A small Rust lexer: skips comments, char literals and raw strings, and
    /// returns each plain string literal that is the first argument of a call
    /// to `t` or `tf` (with its line).
    fn keys_in(source: &str) -> Vec<(String, usize)> {
        let b = source.as_bytes();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let line_at = |i: usize| source[..i].matches('\n').count() + 1;
        let mut keys = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    i = source[i + 2..].find("*/").map_or(b.len(), |end| i + 2 + end + 2);
                }
                b'\'' => {
                    // A char literal ('x', '\n', '日'), else a lifetime.
                    if b.get(i + 1) == Some(&b'\\') {
                        i = source[i + 2..].find('\'').map_or(b.len(), |end| i + 2 + end + 1);
                    } else if let Some(c) = source[i + 1..].chars().next() {
                        let close = i + 1 + c.len_utf8();
                        i = if b.get(close) == Some(&b'\'') { close + 1 } else { i + 1 };
                    } else {
                        i += 1;
                    }
                }
                b'r' if (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
                    && matches!(b.get(i + 1), Some(b'"' | b'#')) =>
                {
                    let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                    if b.get(i + 1 + hashes) != Some(&b'"') {
                        i += 1;
                        continue;
                    }
                    let end = format!("\"{}", "#".repeat(hashes));
                    let body = i + 2 + hashes;
                    i = source[body..].find(&end).map_or(b.len(), |e| body + e + end.len());
                }
                b'"' => {
                    let (text, end) = string_literal(source, i + 1);
                    if is_translation_call(b, i) {
                        keys.push((text, line_at(i)));
                    }
                    i = end;
                }
                _ => {
                    i += source[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
        }
        keys
    }

    /// Whether the literal opening at `quote` follows `t(` or `tf(`.
    fn is_translation_call(b: &[u8], quote: usize) -> bool {
        let mut j = quote;
        while j > 0 && b[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        if j == 0 || b[j - 1] != b'(' {
            return false;
        }
        j -= 1;
        let end = j;
        while j > 0 && (b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'_') {
            j -= 1;
        }
        matches!(&b[j..end], b"t" | b"tf")
    }

    /// Decodes a plain string literal whose body starts at `start`; returns the
    /// text and the index after its closing quote.
    fn string_literal(source: &str, start: usize) -> (String, usize) {
        let mut text = String::new();
        let mut chars = source[start..].char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return (text, start + at + 1),
                '\\' => match chars.next().map(|(_, c)| c) {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('r') => text.push('\r'),
                    Some('0') => text.push('\0'),
                    Some('u') => {
                        let hex: String = chars.by_ref().map(|(_, c)| c).skip(1).take_while(|&c| c != '}').collect();
                        if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            text.push(c);
                        }
                    }
                    Some('\n' | '\r') => {
                        // A line continuation drops the newline and the next line's indent.
                        let mut ahead = chars.clone();
                        while let Some((_, c)) = ahead.next() {
                            if !c.is_whitespace() {
                                break;
                            }
                            chars = ahead.clone();
                        }
                    }
                    Some(other) => text.push(other),
                    None => {}
                },
                c => text.push(c),
            }
        }
        (text, source.len())
    }
}
