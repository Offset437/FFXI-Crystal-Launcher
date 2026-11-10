//! FFXI's play flag: the small public file the PlayOnline Viewer reads to enable its FFXI
//! Play button (`$playbutton_enable`), with the server's own status line (`$status`).
//!
//! Crystal reads it only while a "Wait and play" is waiting (owner decision 2026-10-13) or FFXI
//! is in maintenance, for `$patch_enable` (the update as soon as it is out, patch_watch.rs): one
//! plain GET at a time (`anews.pml`, then `sfst02.pml`, as the FFXI page loads them), every minute, with the same web client and browser User-Agent as the
//! playonline.com reads (`services::web_client`). It never imitates the Viewer: no Viewer
//! User-Agent, X-POL-VIEWER-VERSION or Accept header, no credentials and no Viewer page
//! sequence. A fresh reading decides the Play gate both ways over maintenance (`decide`); without
//! one, and for an outage, the official status decides (services.rs, Round 4).
use crate::i18n::{t, tf};
use crate::services::Availability;
use chrono::{DateTime, Utc};
use std::time::{Duration, Instant};

/// The English Viewer's FFXI flag file (UTF-8 with a BOM).
pub const FLAG_URL: &str = "http://wh000.pol.com:51304/pcd/sftp/en-US/ff11/anews.pml";
/// The FFXI page's second public file (its "scroll news"). The page loads it after `anews.pml`,
/// and Square Enix may put the switches in either: on 2026-11-10 they were only here.
pub const SCROLL_URL: &str = "http://wh000.pol.com:51304/pcd/sftp/en-US/ff11/sfst02.pml";
/// The fixed interval between reads while a Play waits.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);
/// How long a reading decides the gate. An older one leaves it to the official status.
const FRESH_FOR: chrono::Duration = chrono::Duration::minutes(2);
/// Ticks come at least once a second, so a wall-clock gap this long between two of them means
/// the PC slept (or the window was frozen): the flag is read again at once.
const RESUME_GAP: chrono::Duration = chrono::Duration::seconds(30);
/// Tonight's file is about 200 bytes.
const MAX_BODY: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(12);
/// The longest `$status` line shown.
const MAX_STATUS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flag {
    /// `$playbutton_enable` is 1: FFXI is open.
    Up,
    /// It is 0: FFXI is closed. The server's `$status` line, when it sent one.
    Down(Option<String>),
}

/// One read of the flag: when it was requested and what it said. Anything that is not a clear
/// 0 or 1 (no file, an HTTP error, a missing define, another value) is an error, never "up".
/// `patch`: the same file's `$patch_enable` (the stock page's "Check Version" switch), `None`
/// unless it is a clear 0 or 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub at: DateTime<Utc>,
    pub result: Result<Flag, String>,
    pub patch: Option<bool>,
}

fn fresh(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(at);
    age >= chrono::Duration::zero() && age <= FRESH_FOR
}

/// What Crystal knows of the flag: its last good read, and the last read's failure when the
/// last read failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    good: Option<(DateTime<Utc>, Flag)>,
    failed: Option<(DateTime<Utc>, String)>,
    /// `$patch_enable` as the last good read gave it.
    patch: Option<bool>,
}

impl Known {
    pub fn note(&mut self, reading: Reading) {
        match reading.result {
            Ok(flag) => {
                self.good = Some((reading.at, flag));
                self.failed = None;
                self.patch = reading.patch;
            }
            Err(error) => self.failed = Some((reading.at, error)),
        }
    }
    /// `$patch_enable` from a read within the last two minutes: `Some(true)` only for a clear 1.
    pub fn patch(&self, now: DateTime<Utc>) -> Option<bool> {
        self.fresh(now).and(self.patch)
    }
    /// When the flag was last read, whatever came of it.
    pub fn checked(&self) -> Option<DateTime<Utc>> {
        self.failed.as_ref().map(|(at, _)| *at).or(self.good.as_ref().map(|(at, _)| *at))
    }
    /// The flag, if it was read within the last two minutes.
    pub fn fresh(&self, now: DateTime<Utc>) -> Option<&Flag> {
        self.good.as_ref().filter(|(at, _)| fresh(*at, now)).map(|(_, flag)| flag)
    }
    /// Why the last read, a recent one, gave no flag.
    fn fresh_failure(&self, now: DateTime<Utc>) -> Option<&str> {
        self.failed.as_ref().filter(|(at, _)| fresh(*at, now)).map(|(_, error)| error.as_str())
    }
}

/// The Play gate's verdict at `now`. A flag read within the last two minutes wins both ways over
/// `official` (the Round 4 verdict, `ServiceSnapshot::recent_status`): 1 is up even inside the
/// posted window, while the status page lags or when it could not be read, and 0 is still down.
/// The flag is FFXI's maintenance switch, so a 1 does not overrule an Outage (every world down
/// outside maintenance): that stays the status page's to end. Without a fresh read, `official`
/// decides; when that is because the last read failed, its note says so.
pub fn decide(known: &Known, official: (Availability, String), now: DateTime<Utc>) -> (Availability, String) {
    match known.fresh(now) {
        Some(Flag::Up) if matches!(official.0, Availability::Maintenance | Availability::Unknown) =>
            (Availability::Operational, t("PlayOnline's play flag says FFXI is open.").into()),
        Some(Flag::Up) => official,
        Some(Flag::Down(None)) => (Availability::Maintenance, t("PlayOnline's play flag says FFXI is closed.").into()),
        Some(Flag::Down(Some(status))) => (Availability::Maintenance,
            tf("PlayOnline's play flag says FFXI is closed: {status}", &[("status", status)])),
        None => match known.fresh_failure(now) {
            Some(error) => (official.0, tf("{note} The play flag could not be read ({error}), so this follows the official status.",
                &[("note", &official.1), ("error", &error)]).trim_start().to_owned()),
            None => official,
        },
    }
}

/// Reads the flag once: both files the FFXI page loads, in its order.
pub async fn read() -> Reading {
    let at = Utc::now();
    let client = match crate::services::web_client() {
        Ok(client) => client,
        Err(error) => return Reading { at, result: Err(error), patch: None },
    };
    let first = fetch(&client, FLAG_URL).await;
    let second = fetch(&client, SCROLL_URL).await;
    merge(at, [first, second])
}

/// One reading from the page's files in load order. As on the page, a later file's define wins,
/// so each switch comes from the last file that sets it. A file that failed is skipped; when
/// every file failed, the first failure is the reading's.
fn merge(at: DateTime<Utc>, bodies: [Result<Vec<u8>, String>; 2]) -> Reading {
    let read: Vec<&[u8]> = bodies.iter().filter_map(|body| body.as_deref().ok()).collect();
    let Some(last) = read.last() else {
        let error = bodies.into_iter().find_map(Result::err).unwrap_or_default();
        return Reading { at, result: Err(error), patch: None };
    };
    let sets = |body: &&[u8], name: &str| !define_values(&body_defines(body), name).is_empty();
    let result = parse(read.iter().rev().find(|body| sets(body, "$playbutton_enable")).unwrap_or(last));
    let patch = read.iter().rev().find(|body| sets(body, "$patch_enable")).and_then(|body| parse_patch(body));
    Reading { at, result, patch }
}

/// `read` from `url` (a local fake server in tests). Stamped when the request is made.
pub(crate) async fn read_from(url: &str) -> Reading {
    let at = Utc::now();
    let body = match crate::services::web_client() {
        Ok(client) => fetch(&client, url).await,
        Err(error) => Err(error),
    };
    match body {
        Ok(body) => Reading { at, result: parse(&body), patch: parse_patch(&body) },
        Err(error) => Reading { at, result: Err(error), patch: None },
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    tokio::time::timeout(TIMEOUT, async {
        let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
        // A redirect is not followed (the client's policy) and is no answer either.
        if !response.status().is_success() {
            return Err(tf("The play flag server answered HTTP {status}.", &[("status", &response.status().as_u16())]));
        }
        if response.content_length().is_some_and(|n| n > MAX_BODY as u64) {
            return Err(t("The play flag response is larger than expected.").into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(t("The play flag response is larger than expected.").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| t("The play flag request timed out.").to_owned())?
}

/// The flag in an `anews.pml` body: `<define name="$playbutton_enable" value="1">`, and the
/// `$status` define when present. Tag, attribute and define names in any case, attributes in
/// any order, quoted either way or not at all, any whitespace and line ends, with or without
/// a BOM. A missing, repeated-and-different or unknown `$playbutton_enable` is an error.
pub fn parse(body: &[u8]) -> Result<Flag, String> {
    let defines = body_defines(body);
    let values = |wanted: &str| define_values(&defines, wanted);
    let play = values("$playbutton_enable");
    let Some(first) = play.first() else { return Err(t("The play flag has no $playbutton_enable value.").into()) };
    if play.iter().any(|value| value != first) {
        return Err(t("The play flag values disagree.").into());
    }
    match *first {
        "1" => Ok(Flag::Up),
        "0" => {
            let status = values("$status").into_iter().map(clean_status).find(|status| !status.is_empty());
            Ok(Flag::Down(status))
        }
        other => Err(tf("The play flag value {value} is not recognized.", &[("value", &other.chars().take(16).collect::<String>())])),
    }
}

/// `$patch_enable` in an `anews.pml` body, parsed as `parse` parses the play flag: `Some` only
/// for a clear 0 or 1 that every repeat agrees on. Missing or anything else is unknown.
pub fn parse_patch(body: &[u8]) -> Option<bool> {
    let defines = body_defines(body);
    let values = define_values(&defines, "$patch_enable");
    let first = *values.first()?;
    if values.iter().any(|value| *value != first) { return None; }
    match first { "1" => Some(true), "0" => Some(false), _ => None }
}

fn body_defines(body: &[u8]) -> Vec<(String, String)> {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    defines(&String::from_utf8_lossy(body))
}

fn define_values<'a>(defines: &'a [(String, String)], wanted: &str) -> Vec<&'a str> {
    defines.iter().filter(|(name, _)| name.eq_ignore_ascii_case(wanted)).map(|(_, value)| value.trim()).collect()
}

/// Each `<define …>` tag's `name` and `value` attributes, in order.
fn defines(text: &str) -> Vec<(String, String)> {
    // ASCII lower-casing keeps every byte offset, so positions found in `lower` index `text`.
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = lower[from..].find("<define") {
        let start = from + open + "<define".len();
        from = start;
        // `<defined…>` or `<define_…>` is another tag.
        if !lower[start..].starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/') { continue; }
        // A tag that never closes (a cut-off body) says nothing.
        let Some((attributes, length)) = attributes(&text[start..]) else { break };
        let get = |wanted: &str| attributes.iter().find(|(name, _)| name == wanted).map(|(_, value)| value.clone());
        if let (Some(name), Some(value)) = (get("name"), get("value")) { found.push((name, value)); }
        from = start + length;
    }
    found
}

/// The attributes at the start of `tag` (after its name), with lower-cased names, up to the
/// closing `>` outside quotes, and how many bytes that took. `None` when the tag or a quoted
/// value never closes.
fn attributes(tag: &str) -> Option<(Vec<(String, String)>, usize)> {
    let bytes = tag.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    let skip_space = |mut i: usize| { while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; } i };
    loop {
        i = skip_space(i);
        match bytes.get(i) {
            None => return None,
            Some(b'>') => return Some((found, i + 1)),
            Some(b'/') => { i += 1; continue; }
            Some(_) => {}
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'=' | b'>' | b'/') { i += 1; }
        let name = tag[name_start..i].to_ascii_lowercase();
        i = skip_space(i);
        if i >= bytes.len() || bytes[i] != b'=' { found.push((name, String::new())); continue; }
        i = skip_space(i + 1);
        let value = match bytes.get(i) {
            Some(&quote @ (b'"' | b'\'')) => {
                let end = i + 1 + tag[i + 1..].find(quote as char)?;
                let value = &tag[i + 1..end];
                i = end + 1;
                value
            }
            _ => {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' { i += 1; }
                &tag[start..i]
            }
        };
        found.push((name, value.to_owned()));
    }
}

/// `$status` as one plain line: the common entities decoded, control characters and runs of
/// whitespace folded, and capped.
fn clean_status(raw: &str) -> String {
    let decoded = raw.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&");
    decoded.split(|c: char| c.is_whitespace() || c.is_control()).filter(|word| !word.is_empty())
        .collect::<Vec<_>>().join(" ").chars().take(MAX_STATUS).collect()
}

/// When to read the flag: only while a Play waits or FFXI is in maintenance (`wanted`), one
/// read at a time, every CHECK_EVERY, and at once when the PC wakes from sleep. The first wait
/// reads at once because no read was made in the last CHECK_EVERY; a wait that starts while
/// another waits (or within a minute of the last read) relies on that read, so more waits never
/// mean more reads.
#[derive(Debug, Default)]
pub struct Schedule {
    in_flight: bool,
    last: Option<Instant>,
    now_due: bool,
    last_tick: Option<DateTime<Utc>>,
}

impl Schedule {
    /// Each Tick, with the wall clock: a gap of RESUME_GAP or more since the last Tick is a
    /// resume from sleep, which reads at once (a read that was under way when the PC slept
    /// may come back failed).
    pub fn tick(&mut self, wall: DateTime<Utc>) {
        if self.last_tick.is_some_and(|last| wall.signed_duration_since(last) >= RESUME_GAP) { self.now_due = true; }
        self.last_tick = Some(wall);
    }
    /// Whether a read starts now. Never while it is not `wanted`.
    pub fn due(&self, wanted: bool, now: Instant) -> bool {
        wanted && !self.in_flight
            && (self.now_due || self.last.is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY))
    }
    /// When the next read falls due, for a quiet window's timer (none while one is in flight).
    pub fn next(&self) -> Option<Instant> {
        if self.in_flight { return None; }
        self.last.map(|at| at + CHECK_EVERY)
    }
    pub fn begin(&mut self, now: Instant) {
        self.in_flight = true;
        self.now_due = false;
        self.last = Some(now);
    }
    pub fn finish(&mut self) {
        self.in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-13 during All Worlds Maintenance, and at 01:17:30 local (13 minutes before the
    /// posted end), as served: UTF-8 with a BOM.
    const MAINTENANCE: &str = "\u{feff}<meta http-equiv=\"Cache-Control\" content=\"no-cache\">\n<define name=\"$status\" value=\"The game is unavailable for play during maintenance.\">\n<define name=\"$playbutton_enable\" value=\"0\"><define name=\"$patch_enable\" value=\"1\">";
    const EMPTY: &str = "\u{feff}<meta http-equiv=\"Cache-Control\" content=\"no-cache\">\n<define name=\"$length\" value=\"0\">";

    /// 2026-11-10: `anews.pml` carried no switches and `sfst02.pml` carried both. Each switch comes
    /// from the last file that sets it; a failed file is skipped.
    #[test]
    fn switches_come_from_the_last_file_that_sets_them() {
        let at = Utc::now();
        let ok = |s: &str| Ok(s.as_bytes().to_vec());
        let r = merge(at, [ok(EMPTY), ok(MAINTENANCE)]);
        assert!(matches!(r.result, Ok(Flag::Down(Some(_)))));
        assert_eq!(r.patch, Some(true));
        let r = merge(at, [ok(UP), ok(EMPTY)]);
        assert_eq!(r.result, Ok(Flag::Up));
        let r = merge(at, [ok(MAINTENANCE), ok(UP)]);
        assert_eq!(r.result, Ok(Flag::Up), "the later file wins");
        let r = merge(at, [ok(MAINTENANCE), Err("down".into())]);
        assert_eq!(r.patch, Some(true));
        let r = merge(at, [ok(EMPTY), ok(EMPTY)]);
        assert!(r.result.is_err());
        assert_eq!(r.patch, None);
        let r = merge(at, [Err("a".into()), Err("b".into())]);
        assert_eq!(r.result, Err("a".into()));
    }

    const UP: &str = "\u{feff}<meta http-equiv=\"Cache-Control\" content=\"no-cache\">\n<define name=\"$playbutton_enable\" value=\"1\"><define name=\"$patch_enable\" value=\"1\">";

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn both_served_bodies_parse() {
        assert_eq!(parse(MAINTENANCE.as_bytes()), Ok(Flag::Down(Some("The game is unavailable for play during maintenance.".into()))));
        assert_eq!(parse(UP.as_bytes()), Ok(Flag::Up));
        // Without the BOM too.
        assert_eq!(parse(UP.trim_start_matches('\u{feff}').as_bytes()), Ok(Flag::Up));
    }

    #[test]
    fn the_markup_may_vary_in_case_order_quotes_and_line_ends() {
        for body in [
            "<DEFINE NAME=\"$PLAYBUTTON_ENABLE\" VALUE=\"1\">",
            "<define value=\"1\" name=\"$playbutton_enable\">",
            "<define\r\n  name = '$playbutton_enable'\r\n  value = '1' />",
            "<define name=$playbutton_enable value=1>",
            "<define name=\"$playbutton_enable\" value=\" 1 \">\r\n",
            "<!-- x --><define name=\"$other\" value=\"0\"><define name=\"$playbutton_enable\" value=\"1\">",
            "<define name=\"$playbutton_enable\" value=\"1\"><define name=\"$playbutton_enable\" value=\"1\">",
        ] {
            assert_eq!(parse(body.as_bytes()), Ok(Flag::Up), "{body:?}");
        }
        let crlf = MAINTENANCE.replace('\n', "\r\n").replace("$status", "$STATUS");
        assert_eq!(parse(crlf.as_bytes()), Ok(Flag::Down(Some("The game is unavailable for play during maintenance.".into()))));
        // Status after the flag, with entities and a line break; an empty one is none.
        assert_eq!(parse(b"<define name='$playbutton_enable' value='0'><define name='$status' value='Back at 1:30 &amp; later.\r\n Thanks'>"),
            Ok(Flag::Down(Some("Back at 1:30 & later. Thanks".into()))));
        assert_eq!(parse(b"<define name=\"$status\" value=\"  \"><define name=\"$playbutton_enable\" value=\"0\">"), Ok(Flag::Down(None)));
    }

    #[test]
    fn anything_but_a_clear_flag_is_unknown_never_up() {
        for body in [
            "",
            "<html>Not Found</html>",
            "<define name=\"$patch_enable\" value=\"1\">",
            "<define name=\"$playbutton_enable\">",
            "<define name=\"$playbutton_enable\" value=\"\">",
            "<define name=\"$playbutton_enable\" value=\"2\">",
            "<define name=\"$playbutton_enable\" value=\"true\">",
            "<define name=\"$playbutton_enable\" value=\"10\">",
            "<define name=\"$playbutton_enable\" value=\"1\"><define name=\"$playbutton_enable\" value=\"0\">",
            "<defined name=\"$playbutton_enable\" value=\"1\">",
            "<define name=\"$playbutton_enable_x\" value=\"1\">",
            "<define name=\"$playbutton_enable\" value=\"1",
        ] {
            assert!(parse(body.as_bytes()).is_err(), "{body:?}");
        }
        // Bytes that are not UTF-8 are no flag either.
        assert!(parse(&[0xff, 0xfe, 0x00, 0x31]).is_err());
    }

    /// `$patch_enable`, the stock page's "Check Version" switch: a clear 0 or 1 in either served
    /// body, with the same markup leniency as the play flag; missing or unclear is unknown,
    /// never ready.
    #[test]
    fn patch_enable_parses_and_anything_unclear_is_unknown() {
        assert_eq!(parse_patch(MAINTENANCE.as_bytes()), Some(true));
        assert_eq!(parse_patch(UP.as_bytes()), Some(true));
        for (body, expected) in [
            ("<define name=\"$playbutton_enable\" value=\"0\"><define name=\"$patch_enable\" value=\"0\">", Some(false)),
            ("<DEFINE VALUE='1' NAME='$PATCH_ENABLE'>", Some(true)),
            ("<define name=$patch_enable value= 0 >", Some(false)),
            ("<define name=\"$playbutton_enable\" value=\"0\">", None),
            ("", None),
            ("<define name=\"$patch_enable\">", None),
            ("<define name=\"$patch_enable\" value=\"\">", None),
            ("<define name=\"$patch_enable\" value=\"yes\">", None),
            ("<define name=\"$patch_enable\" value=\"1\"><define name=\"$patch_enable\" value=\"0\">", None),
            ("<define name=\"$patch_enable_x\" value=\"1\">", None),
            ("<define name=\"$patch_enable\" value=\"1", None),
        ] {
            assert_eq!(parse_patch(body.as_bytes()), expected, "{body:?}");
        }
        // Only a fresh good read speaks for it; a failed read keeps the last good one's.
        let now = instant("2026-10-13T08:17:30Z");
        let mut known = Known::default();
        assert_eq!(known.patch(now), None);
        known.note(Reading { at: now - chrono::Duration::seconds(30), result: Ok(Flag::Down(None)), patch: Some(true) });
        assert_eq!(known.patch(now), Some(true));
        known.note(Reading { at: now, result: Err("timed out".into()), patch: None });
        assert_eq!(known.patch(now), Some(true));
        assert_eq!(known.patch(now + chrono::Duration::minutes(3)), None, "stale");
        known.note(Reading { at: now, result: Ok(Flag::Down(None)), patch: None });
        assert_eq!(known.patch(now), None, "a read without the define");
    }

    /// The decision table: flag fresh, stale, failed or missing, against every official verdict
    /// (a posted window or the status page's maintenance icon is `Maintenance`).
    #[test]
    fn a_fresh_flag_wins_both_ways_and_otherwise_the_official_status_decides() {
        let now = instant("2026-10-13T08:17:30Z");
        let official = |state| (state, "official note".to_owned());
        let known = |reads: &[(i64, Result<Flag, String>)]| {
            let mut known = Known::default();
            for (age, result) in reads { known.note(Reading { at: now - chrono::Duration::seconds(*age), result: result.clone(), patch: None }); }
            known
        };
        let down = Ok(Flag::Down(Some("The game is unavailable for play during maintenance.".into())));
        for state in [Availability::Maintenance, Availability::Outage, Availability::Operational, Availability::PartialOutage, Availability::Unknown] {
            // Fresh 1: up, even inside the posted window, with a page that lags or with no page
            // read. Review finding: never over an Outage (every world down outside maintenance),
            // which the maintenance switch does not speak for.
            let up = decide(&known(&[(30, Ok(Flag::Up))]), official(state), now);
            if matches!(state, Availability::Maintenance | Availability::Unknown) {
                assert_eq!(up, (Availability::Operational, "PlayOnline's play flag says FFXI is open.".into()));
            } else {
                assert_eq!(up, official(state), "nothing to overrule, or an outage the flag does not end");
            }
            // Fresh 0: still down, with the server's line, even when the page shows the worlds up.
            let (verdict, note) = decide(&known(&[(119, down.clone())]), official(state), now);
            assert_eq!(verdict, Availability::Maintenance, "{state:?}");
            assert!(note.ends_with("The game is unavailable for play during maintenance."), "{note}");
            // A good read within two minutes still decides after a later read failed.
            assert_eq!(decide(&known(&[(90, down.clone()), (30, Err("timed out".into()))]), official(state), now).0, Availability::Maintenance);
            assert_eq!(decide(&known(&[(90, Ok(Flag::Up)), (30, Err("timed out".into()))]), official(Availability::Maintenance), now).0,
                Availability::Operational);
            assert_eq!(decide(&known(&[(90, Ok(Flag::Up)), (30, Err("timed out".into()))]), official(Availability::Outage), now).0,
                Availability::Outage);
            // Stale (over two minutes), from the future, or never read: the official verdict as it is.
            for reads in [vec![(121, Ok(Flag::Up))], vec![(121, down.clone())], vec![(-5, Ok(Flag::Up))], vec![], vec![(500, Err("old".into()))]] {
                assert_eq!(decide(&known(&reads), official(state), now), official(state), "{state:?} {reads:?}");
            }
            // No fresh flag because the last read failed: the official verdict, and its note says so.
            for reads in [vec![(10, Err("timed out".into()))], vec![(200, Ok(Flag::Up)), (10, Err("HTTP 503".into()))]] {
                let (verdict, note) = decide(&known(&reads), official(state), now);
                assert_eq!(verdict, state);
                assert!(note.starts_with("official note The play flag could not be read") && note.ends_with("so this follows the official status."), "{note}");
            }
        }
        assert_eq!(decide(&known(&[(0, Ok(Flag::Down(None)))]), official(Availability::Operational), now),
            (Availability::Maintenance, "PlayOnline's play flag says FFXI is closed.".into()));
        // An unread official status has no note to lead with.
        let (_, note) = decide(&known(&[(1, Err("timed out".into()))]), (Availability::Unknown, String::new()), now);
        assert!(note.starts_with("The play flag could not be read (timed out)"), "{note}");
        let checked = known(&[(90, Ok(Flag::Up)), (30, Err("timed out".into()))]).checked();
        assert_eq!(checked, Some(now - chrono::Duration::seconds(30)), "the last read, whatever came of it");
    }

    #[test]
    fn reads_happen_only_while_waiting_one_at_a_time_every_minute() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        assert!(!schedule.due(false, start), "nothing waits: never");
        // The first wait: at once.
        assert!(schedule.due(true, start));
        schedule.begin(start);
        assert!(!schedule.due(true, start + Duration::from_secs(90)), "one at a time");
        schedule.finish();
        // Review finding: more waits (four rows clicked in a row, Cancel then Play again) are
        // no extra reads; the read just made serves them.
        assert!(!schedule.due(true, start + Duration::from_secs(5)));
        assert!(!schedule.due(true, start + CHECK_EVERY - Duration::from_millis(1)));
        assert!(schedule.due(true, start + CHECK_EVERY));
        assert!(!schedule.due(false, start + CHECK_EVERY * 10), "the waits ended: no more reads");
        // A wait that starts a minute or more after the last read: at once.
        assert!(schedule.due(true, start + CHECK_EVERY * 10));
    }

    #[test]
    fn a_resume_from_sleep_reads_at_once() {
        let start = Instant::now();
        let wall = instant("2026-10-13T08:00:00Z");
        let mut schedule = Schedule::default();
        schedule.begin(start);
        schedule.finish();
        schedule.tick(wall);
        schedule.tick(wall + chrono::Duration::seconds(1));
        assert!(!schedule.due(true, start + Duration::from_secs(2)));
        // The monotonic clock may not have counted the sleep; the wall clock did.
        schedule.tick(wall + chrono::Duration::minutes(40));
        assert!(schedule.due(true, start + Duration::from_secs(3)));
        assert!(!schedule.due(false, start + Duration::from_secs(3)), "still only while waiting");
        schedule.begin(start + Duration::from_secs(3));
        assert!(!schedule.due(true, start + Duration::from_secs(4)));
    }

    /// One local fake server answer: the request it saw (lower-cased header names).
    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/pcd/sftp/en-US/ff11/anews.pml", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 { break; }
                request.extend_from_slice(&buffer[..read]);
            }
            // The client may hang up early (an oversized answer).
            let _ = stream.write_all(&response);
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, server)
    }

    fn http(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn the_flag_is_one_plain_get_with_the_browser_user_agent() {
        let (url, server) = serve_once(http("200 OK", MAINTENANCE.as_bytes()));
        let before = Utc::now();
        let reading = read_from(&url).await;
        let request = server.join().unwrap();
        assert!(reading.at >= before && reading.at <= Utc::now());
        assert_eq!(reading.result, Ok(Flag::Down(Some("The game is unavailable for play during maintenance.".into()))));
        assert_eq!(reading.patch, Some(true), "the same read gives $patch_enable");
        let mut lines = request.lines();
        assert_eq!(lines.next(), Some("GET /pcd/sftp/en-US/ff11/anews.pml HTTP/1.1"));
        let headers: Vec<(String, String)> = lines.filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned())).collect();
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(header("user-agent"), Some(crate::services::browser_user_agent().as_str()));
        // The web client's own Accept, as for the playonline.com reads; nothing of the Viewer
        // and nothing of an account.
        assert_eq!(header("accept"), Some("*/*"));
        for name in ["x-pol-viewer-version", "cookie", "authorization"] { assert_eq!(header(name), None, "{name}"); }
        assert!(headers.iter().all(|(name, value)| !name.contains("pol") && !value.to_ascii_lowercase().contains("pol")), "{request}");
    }

    #[tokio::test]
    async fn http_errors_and_oversized_answers_are_unknown() {
        let (url, server) = serve_once(http("503 Service Unavailable", b"busy"));
        let reading = read_from(&url).await;
        server.join().unwrap();
        assert_eq!(reading.result, Err("The play flag server answered HTTP 503.".into()));
        assert_eq!(reading.patch, None);
        let (url, server) = serve_once(http("302 Found", b""));
        assert!(read_from(&url).await.result.is_err(), "a redirect is no answer");
        server.join().unwrap();
        let (url, server) = serve_once(http("200 OK", &vec![b' '; MAX_BODY + 1]));
        assert_eq!(read_from(&url).await.result, Err("The play flag response is larger than expected.".into()));
        server.join().unwrap();
        // Nobody listening.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/anews.pml", closed.local_addr().unwrap());
        drop(closed);
        assert!(read_from(&url).await.result.is_err());
    }
}
