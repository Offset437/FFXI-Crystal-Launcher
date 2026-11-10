//! Privacy mode, for screenshots: names and identifiers are masked.
//!
//! Display only. Settings, the account vault, logs, the friend cache and
//! everything sent to the loader keep the real values; views pass identifying
//! text through here. What is masked: account labels, PlayOnline / Square Enix
//! IDs, character and friend names (and other PlayOnline handles on mail),
//! message subjects and bodies, and the Windows user name inside paths.
//!
//! Every masked value shows the same eight bullets, so neither its length nor
//! its shape is revealed.

// One view thread in the app; per test thread in tests (as `i18n::current`),
// so parallel tests cannot switch each other's privacy.
#[cfg(not(test))]
static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROFILE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
pub(crate) fn set(on: bool) {
    ON.store(on, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(test)]
pub(crate) fn set(on: bool) {
    ON.with(|c| c.set(on));
}
#[cfg(not(test))]
pub(crate) fn on() -> bool {
    ON.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(test)]
pub(crate) fn on() -> bool {
    ON.with(|c| c.get())
}

/// The one stand-in for every masked value.
const MASKED_TEXT: &str = "••••••••";

/// A nonempty value while privacy mode is on: always eight bullets. Empty
/// values stay empty, so fields keep their ordinary placeholders.
pub(crate) fn mask(value: &str) -> String {
    if !on() || value.is_empty() {
        return value.to_owned();
    }
    MASKED_TEXT.to_owned()
}

/// Everything on screen that identifies the user or a contact: account
/// labels and IDs, friend and character names, mail handles. Privacy mode
/// masks these wherever they appear inside other text.
pub(crate) fn identities(app: &crate::App) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    for row in &app.accounts {
        names.extend([row.account.label.as_str(), &row.account.pol_id, &row.account.se_id]);
    }
    if let Some(snapshot) = &app.friends.snapshot {
        for friend in &snapshot.entries {
            names.push(&friend.name);
            names.extend(friend.character.as_deref());
        }
        names.extend(snapshot.social.headers.iter().map(|h| h.sender.as_str()));
    }
    names.retain(|n| n.trim().chars().count() > 1);
    names
}

/// `text` split into plain parts and the parts that identify someone: any of
/// `names` (as whole words) and the Windows user name inside a path.
pub(crate) fn split(text: &str, names: &[&str]) -> Vec<(String, bool)> {
    let user = user_name();
    let mut needles: Vec<&str> = names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect();
    needles.extend(user.as_deref());
    needles.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    needles.dedup();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for needle in &needles {
            if rest.len() >= needle.len() && rest.is_char_boundary(needle.len()) && rest[..needle.len()].eq_ignore_ascii_case(needle) {
                let before = plain.chars().next_back().or_else(|| parts.last().and_then(|(p, _)| p.chars().next_back()));
                let after = rest[needle.len()..].chars().next();
                if !word(before) && !word(after) {
                    if !plain.is_empty() {
                        parts.push((std::mem::take(&mut plain), false));
                    }
                    parts.push((rest[..needle.len()].to_owned(), true));
                    rest = &rest[needle.len()..];
                    continue 'scan;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    if !plain.is_empty() {
        parts.push((plain, false));
    }
    parts
}

// ---- Paths ---------------------------------------------------------------

/// The Windows user folder (`C:\Users\<name>`), whose last part names the user.
#[cfg(not(test))]
fn profile_dir() -> Option<String> {
    std::env::var("USERPROFILE").ok().filter(|p| p.len() > 3)
}
#[cfg(test)]
fn profile_dir() -> Option<String> {
    PROFILE.with(|p| p.borrow().clone())
}
#[cfg(test)]
pub(crate) fn set_profile_dir_for_test(dir: &str) {
    PROFILE.with(|p| *p.borrow_mut() = Some(dir.to_owned()));
}

/// The Windows user name, as it appears inside the user's paths.
fn user_name() -> Option<String> {
    let dir = profile_dir()?;
    let name = dir.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next()?.to_owned();
    (name.len() > 1).then_some(name)
}

/// A path for a text field: with privacy on, everything after the drive is
/// bullets, so no folder name shows. Path fields are read-only while privacy
/// is on (see [`editable`]), so a masked value is never typed back.
pub(crate) fn mask_path(value: &str) -> String {
    if !on() || value.trim().is_empty() {
        return value.to_owned();
    }
    let drive = value.char_indices().nth(2).filter(|_| value.get(1..3) == Some(":\\")).map_or(0, |(i, _)| i + 1);
    format!("{}{}", &value[..drive], mask(&value[drive..]))
}

/// Whether a path field takes typing: not while privacy mode hides it.
pub(crate) fn editable() -> bool {
    !on()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifying_parts_are_split_out_as_whole_words() {
        set_profile_dir_for_test(r"C:\Users\Jane");
        let parts = split(r"Al: log at C:\users\jane\x.log. Also Alyria.", &["Al", "Alyria"]);
        let hidden: Vec<_> = parts.iter().filter(|(_, h)| *h).map(|(p, _)| p.as_str()).collect();
        assert_eq!(hidden, ["Al", "jane", "Alyria"]);
        assert_eq!(parts.iter().map(|(p, _)| p.as_str()).collect::<String>(), r"Al: log at C:\users\jane\x.log. Also Alyria.");
    }

    #[test]
    fn fields_mask_the_user_folder_and_ids() {
        set_profile_dir_for_test(r"C:\Users\Jane");
        set(false);
        assert_eq!(mask("ABC12345"), "ABC12345");
        assert_eq!(mask_path(r"C:\Users\Jane\FFXI"), r"C:\Users\Jane\FFXI");
        set(true);
        for value in ["Al", "ABC12345", "An exceptionally long account name", "アリスドラゴン", "e\u{301}🐉"] {
            assert_eq!(mask(value), "••••••••");
        }
        assert_eq!(mask(""), "");
        // Whole locations are hidden, not just the user name; the fields are read-only meanwhile.
        assert_eq!(mask_path(r"C:\Users\Jane\Downloads\FFXI"), r"C:\••••••••");
        assert_eq!(mask_path(r"C:\Al"), mask_path(r"C:\An exceptionally long user\FFXI"));
        assert_eq!(mask_path(r"C:\アリス\FFXI"), r"C:\••••••••");
        assert!(!mask_path(r"D:\Games\FFXI").contains("Games"));
        assert!(mask_path(r"D:\Games\FFXI").starts_with(r"D:\"));
        assert_eq!(mask_path(""), "");
        assert!(!editable());
        set(false);
        assert!(editable());
    }
}
