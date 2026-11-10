//! The Friends page's view model (ui/friends.slint): `wire` turns its callbacks into core
//! messages, `sync` pushes `app.friends` into its properties after each batch of messages.
//!
//! Every control sends its core message only while the core allows it (the enabled controls'
//! messages are worked out at sync time and kept), and privacy mode masks names, characters and
//! mail. `sync` sets a property only when its value differs and
//! updates list models row by row, so an unchanged state redraws nothing. Per-row targets (a
//! friend, a mail header, a blocked handle) are kept between syncs and replaced only when they
//! change; the row's message is built when it is clicked.
use super::sync::{put, strings_model, sync_model};
use super::{BlockedLine, ChatBlock, ChatItem, FriendLine, FriendsStrings, FriendsView, MailLine, MainWindow};
use crate::friends::{message_time, AutoAway, Body, Entry, Friend, Pane, Snapshot, SUBJECT_MAX, TEXT_MAX};
use crate::i18n::{t, tf, Lang};
use crate::privacy;
use crate::session_worker::NativeControl;
use crate::social::Request;
use crate::{App, Message};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

/// The Viewer's three own-status choices, in the drop-down's order.
const STATUSES: [NativeControl; 3] = [NativeControl::Visible, NativeControl::Away, NativeControl::Invisible];
/// Mailbox folders in the tabs' order: Inbox, Sent, Archived.
const FOLDERS: [u8; 3] = [0, 2, 1];
/// Quiet avatar tints (the colours live in friends.slint).
const TINTS: u32 = 6;

/// The message each control sends right now (`None`: disabled), and what the inputs act on.
/// Rebuilt every sync; the per-row targets live in `State` and outlast it.
#[derive(Default)]
struct Actions {
    sign_out: Option<Message>,
    play: Option<Message>,
    connect: Option<Message>,
    otp_key: Option<String>,
    account_keys: Vec<String>,
    status: bool,
    auto_away: bool,
    query: bool,
    /// The friend list shows: its rows act on `State::rows_to`.
    rows: bool,
    check: Option<Message>,
    /// Remove acts on `State::remove_to`.
    remove: bool,
    confirm: Option<Message>,
    subject: bool,
    draft: bool,
    add_subject: Option<Message>,
    send: Option<Message>,
    folders: [Option<Message>; 3],
    previous: Option<Message>,
    next: Option<Message>,
    /// The Mailbox shows and can read: its lines act on `State::mail_to`.
    reads: bool,
    /// Each blocked line's Unblock, acting on `State::blocked_to`.
    unblocks: Vec<bool>,
}

/// What a friend-list row opens: a section's fold, or a friend's conversation.
#[derive(Clone, PartialEq, Debug)]
enum RowTarget {
    Section(usize),
    Friend { slot: u16, name: String },
}

impl RowTarget {
    fn message(&self) -> Message {
        match self {
            Self::Section(index) => Message::FriendsSection(*index),
            Self::Friend { slot, name } => Message::FriendsSelect(Some(Pane::Friend { slot: *slot, name: name.clone() })),
        }
    }
}

/// `friend` as the kept copy, cloned only when it differs from the kept one.
fn keep_friend(kept: &mut Option<Friend>, friend: &Friend) {
    let same = kept.as_ref().is_some_and(|k| {
        k.slot == friend.slot
            && k.alternate == friend.alternate
            && k.uid == friend.uid
            && k.online == friend.online
            && k.name == friend.name
            && k.location == friend.location
            && k.character == friend.character
    });
    if !same {
        *kept = Some(friend.clone());
    }
}

/// Sets row `i` of `list` to `value` (pushing when it is the next one), unless it already is.
fn keep_at<T, V: ?Sized>(list: &mut Vec<T>, i: usize, value: &V, same: impl Fn(&T, &V) -> bool, make: impl FnOnce(&V) -> T) {
    match list.get(i) {
        Some(old) if same(old, value) => {}
        Some(_) => list[i] = make(value),
        None => list.push(make(value)),
    }
}

/// One chat block as plain data, compared with the shown one before the model is touched.
#[derive(Clone, PartialEq, Debug)]
enum Block {
    Divider { label: String, accent: bool },
    Group { mine: bool, who: String, time: String, initial: String, tint: i32, items: Vec<Item> },
}

#[derive(Clone, PartialEq, Debug, Default)]
struct Item {
    subject: String,
    text: String,
    italic: bool,
    skeleton: bool,
    pending: bool,
}

struct State {
    lang: Option<Lang>,
    /// Privacy mode as of the last sync (the fixed mask follows it).
    privacy: Option<bool>,
    actions: Actions,
    /// Each friend-list row's target (`None`: a note).
    rows_to: Vec<Option<RowTarget>>,
    /// The friend Remove asks about.
    remove_to: Option<Friend>,
    /// Each Mailbox line's raw header, for Read.
    mail_to: Vec<String>,
    /// Each blocked line's handle, for Unblock.
    blocked_to: Vec<Option<Friend>>,
    /// The open conversation, and a count that moves when another one opens.
    open: Option<(u16, String)>,
    chat_key: i32,
    /// The draft as the core last had it, without its trailing line breaks.
    draft: String,
    blocks: Vec<Block>,
    problems: Rc<VecModel<SharedString>>,
    accounts: Rc<VecModel<SharedString>>,
    statuses: Rc<VecModel<SharedString>>,
    auto_away: Rc<VecModel<SharedString>>,
    rows: Rc<VecModel<FriendLine>>,
    chat: Rc<VecModel<ChatBlock>>,
    folders: Rc<VecModel<SharedString>>,
    mail: Rc<VecModel<MailLine>>,
    blocked: Rc<VecModel<BlockedLine>>,
}

impl State {
    fn new() -> Self {
        Self {
            lang: None,
            privacy: None,
            actions: Actions::default(),
            rows_to: Vec::new(),
            remove_to: None,
            mail_to: Vec::new(),
            blocked_to: Vec::new(),
            open: None,
            chat_key: 0,
            draft: String::new(),
            blocks: Vec::new(),
            problems: Rc::default(),
            accounts: Rc::default(),
            statuses: Rc::default(),
            auto_away: Rc::default(),
            rows: Rc::default(),
            chat: Rc::default(),
            folders: Rc::default(),
            mail: Rc::default(),
            blocked: Rc::default(),
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The window closed: the page's models go with it (nothing stays in the background).
pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}

#[cfg(not(test))]
fn deliver(message: Message) {
    super::send(message);
}

#[cfg(test)]
thread_local! {
    /// What the page sent, in tests (there is no host to deliver to).
    static SENT: RefCell<Vec<Message>> = const { RefCell::new(Vec::new()) };
}
#[cfg(test)]
fn deliver(message: Message) {
    SENT.with(|sent| sent.borrow_mut().push(message));
}

/// Sends the message `f` picks from the last sync's actions, after releasing them.
fn act(f: impl FnOnce(&Actions) -> Option<Message>) {
    act_on(|state| f(&state.actions));
}

/// As `act`, with the kept per-row targets too.
fn act_on(f: impl FnOnce(&State) -> Option<Message>) {
    let message = STATE.with(|cell| cell.borrow().as_ref().and_then(f));
    if let Some(message) = message {
        deliver(message);
    }
}

fn index(i: i32) -> Option<usize> {
    usize::try_from(i).ok()
}

/// Connect the page's callbacks: each sends its control's core message.
pub(super) fn wire(ui: &MainWindow) {
    let state = State::new();
    let view = ui.global::<FriendsView>();
    view.set_problems(ModelRc::from(state.problems.clone()));
    view.set_accounts(ModelRc::from(state.accounts.clone()));
    view.set_statuses(ModelRc::from(state.statuses.clone()));
    view.set_auto_away_items(ModelRc::from(state.auto_away.clone()));
    view.set_rows(ModelRc::from(state.rows.clone()));
    view.set_chat(ModelRc::from(state.chat.clone()));
    view.set_folders(ModelRc::from(state.folders.clone()));
    view.set_mail(ModelRc::from(state.mail.clone()));
    view.set_blocked(ModelRc::from(state.blocked.clone()));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));

    view.on_sign_out(|| act(|a| a.sign_out.clone()));
    view.on_play(|| act(|a| a.play.clone()));
    view.on_connect(|| act(|a| a.connect.clone()));
    view.on_otp_edited(|value| act(|a| a.otp_key.clone().map(|key| Message::OtpChanged(key, value.into()))));
    view.on_account_picked(|i| act(|a| a.account_keys.get(index(i)?).cloned().map(Message::FriendsAccount)));
    view.on_status_picked(|i| act(|a| a.status.then(|| STATUSES.get(index(i)?).copied().map(Message::FriendsStatus)).flatten()));
    view.on_status_options(|| deliver(Message::FriendsStatusOptions));
    view.on_groups_toggled(|on| deliver(Message::FriendsStatusGroups(on)));
    view.on_auto_away_picked(|i| {
        act(|a| a.auto_away.then(|| crate::pol_less::AUTO_AWAY_CHOICES.get(index(i)?).copied().map(Message::FriendsAutoAway)).flatten())
    });
    view.on_query_edited(|value| act(|a| a.query.then(|| Message::FriendsQuery(value.into()))));
    view.on_row_clicked(|i| act_on(|s| s.actions.rows.then(|| s.rows_to.get(index(i)?).cloned().flatten()).flatten().map(|to| to.message())));
    view.on_open_mailbox(|| deliver(Message::FriendsSelect(Some(Pane::Mailbox))));
    view.on_open_blocked(|| deliver(Message::FriendsSelect(Some(Pane::Blocked))));
    view.on_check(|| act(|a| a.check.clone()));
    view.on_remove(|| act_on(|s| s.actions.remove.then(|| s.remove_to.clone().map(Message::FriendsRemove)).flatten()));
    view.on_confirm_cancel(|| deliver(Message::FriendsDraftClose));
    view.on_confirm(|| act(|a| a.confirm.clone()));
    view.on_subject_edited(|value| act(|a| a.subject.then(|| Message::FriendsSubject(value.into()))));
    view.on_draft_edited(|value| draft_edited(value.as_str()));
    view.on_add_subject(|| act(|a| a.add_subject.clone()));
    view.on_send(|| act(|a| a.send.clone()));
    view.on_folder_picked(|i| act(|a| a.folders.get(index(i)?).cloned().flatten()));
    view.on_previous(|| act(|a| a.previous.clone()));
    view.on_next(|| act(|a| a.next.clone()));
    view.on_read(|i| {
        act_on(|s| s.actions.reads.then(|| s.mail_to.get(index(i)?).map(|raw| Message::FriendsSocial(Request::Read { header: raw.clone() }))).flatten())
    });
    view.on_unblock(|i| {
        act_on(|s| {
            let i = index(i)?;
            (*s.actions.unblocks.get(i)?).then(|| s.blocked_to.get(i).cloned().flatten().map(Message::FriendsRemove)).flatten()
        })
    });
}

/// The message box changed: its text becomes the draft's.
fn draft_edited(text: &str) {
    let editable = STATE.with(|cell| cell.borrow().as_ref().is_some_and(|state| state.actions.draft));
    if !editable {
        return;
    }
    deliver(Message::FriendsMessageEdit(text.to_owned()));
}

/// `text` with the parts naming `names` masked while privacy mode is on.
fn masked(text: &str, names: &[&str]) -> String {
    if !privacy::on() {
        return text.to_owned();
    }
    privacy::split(text, names).into_iter().map(|(part, hide)| if hide { privacy::mask(&part) } else { part }).collect()
}

/// Free text (a problem, a notice) with whatever identifies someone masked.
/// The names are gathered only while privacy mode is on.
fn private(app: &App, text: &str) -> String {
    if !privacy::on() {
        return text.to_owned();
    }
    masked(text, &crate::privacy::identities(app))
}

/// A name or a message: wholly masked while privacy mode is on (privacy::name).
fn hidden(text: &str) -> String {
    privacy::mask(text)
}

/// An avatar's initial and tint for `name`: none in privacy mode (the person glyph, neutral).
fn face(name: &str) -> (String, i32) {
    if privacy::on() {
        return (String::new(), -1);
    }
    let hash = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    (name.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default(), (hash % TINTS) as i32)
}

fn display_name(friend: &Friend) -> String {
    if friend.name.is_empty() { t("Unnamed handle").to_owned() } else { friend.name.clone() }
}

fn presence_of(friend: &Friend) -> i32 {
    match friend.online {
        Some(true) => 0,
        Some(false) => 2,
        None => 3,
    }
}

/// The line under a friend's name: where they are when online, else their state; the
/// character's name in it masked.
fn secondary(friend: &Friend) -> String {
    let line = match friend.online {
        Some(true) => friend.location.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| t("Online").to_owned()),
        Some(false) => t("Offline").to_owned(),
        None => t("Waiting for PlayOnline status").to_owned(),
    };
    let names: Vec<&str> = friend.character.iter().map(String::as_str).collect();
    masked(&line, &names)
}

/// What the page can do right now.
struct Ctx<'a> {
    app: &'a App,
    account: Option<&'a crate::AccountRow>,
    busy: bool,
    idle: bool,
    ready: bool,
    in_game: bool,
}

impl<'a> Ctx<'a> {
    fn new(app: &'a App) -> Self {
        let state = &app.friends;
        let account = app.accounts.iter().find(|a| state.account.as_ref() == Some(&a.key));
        let busy = account.is_some_and(|a| a.busy) || state.pending.is_some() || state.snapshot.as_ref().is_some_and(|s| s.social.busy);
        let idle = state.connected && state.snapshot.as_ref().is_some_and(|s| s.game_running == Some(false));
        let ready = idle && !busy && state.snapshot.as_ref().is_some_and(|s| s.social.ready);
        let in_game = state.connected && state.snapshot.as_ref().is_some_and(|s| s.game_running == Some(true));
        Self { app, account, busy, idle, ready, in_game }
    }
}

/// Push `app.friends` into the window. Called after every batch of messages; only what changed
/// is set. Nothing is worked out while another page shows.
pub(super) fn sync(ui: &MainWindow, app: &App) {
    if app.page != crate::Page::Friends {
        return;
    }
    let view = ui.global::<FriendsView>();
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(State::new);
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(view, get_strings, set_strings, strings());
            strings_model(&state.statuses, [t("Visible"), t("Away"), t("Invisible")].map(str::to_owned));
            strings_model(&state.folders, [t("Inbox"), t("Sent"), t("Archived")].map(str::to_owned));
            strings_model(&state.auto_away, crate::pol_less::AUTO_AWAY_CHOICES.map(|m| AutoAway(m).to_string()));
        }
        if state.privacy != Some(privacy::on()) {
            state.privacy = Some(privacy::on());
            view.set_mask(privacy::mask("•").into());
        }
        let mut actions = Actions::default();
        let x = Ctx::new(app);
        header(&view, &x, &mut actions);
        problems(&x, state);
        put!(view, get_side_w, set_side_w, if app.viewport.width >= 1200.0 { 296.0 } else { 256.0 });
        let friends = &app.friends;
        let shown = friends.snapshot.as_ref().filter(|s| s.fetched_at != 0 && friends.connected);
        put!(view, get_shown, set_shown, shown.is_some());
        account_face(&view, &x, state, &mut actions);
        match shown {
            Some(snapshot) => {
                sidebar(&view, &x, snapshot, state, &mut actions);
                panes(&view, &x, snapshot, state, &mut actions);
            }
            None => sign_in(&view, &x, &mut actions),
        }
        state.actions = actions;
    });
}

fn strings() -> FriendsStrings {
    FriendsStrings {
        title: t("PlayOnline friends").into(),
        sign_out: t("Sign out").into(),
        start_ffxi: t("Start FFXI").into(),
        your_friends: t("Your PlayOnline friends").into(),
        otp_placeholder: t("One-time code (optional)").into(),
        choose_account: t("Choose the account to sign in").into(),
        status_options: t("Status options").into(),
        groups: t("Also set for my groups").into(),
        auto_away_note: t("Set Away when Crystal sees no input while FFXI is closed. Applies at the next sign-in.").into(),
        find: t("Find a friend…").into(),
        mailbox: t("Mailbox").into(),
        mailbox_desc: t("All your stored PlayOnline mail, from friends and others.").into(),
        blocked: t("Blocked").into(),
        blocked_desc: t("People you block can't message you.").into(),
        welcome: t("Choose a friend to see your messages").into(),
        remove: t("Remove from friends").into(),
        cancel: t("Cancel").into(),
        in_game_title: t("FINAL FANTASY XI is running").into(),
        in_game_body: t("Messages and friend changes open again when FINAL FANTASY XI closes.").into(),
        cant_receive: t("This friend can't receive messages from Crystal.").into(),
        subject_placeholder: t("Subject (optional)").into(),
        draft_placeholder: t("Write a PlayOnline message…").into(),
        send: t("Send").into(),
        newest: t("Showing your newest PlayOnline mail. Older messages are in the Mailbox.").into(),
        sending: t("Sending…").into(),
        you: t("You").into(),
        previous: t("Previous").into(),
        next: t("Next").into(),
        unblock: t("Unblock").into(),
        nobody: t("Nobody is blocked").into(),
        new_mail: t("New PlayOnline mail has arrived. Open Inbox to read it.").into(),
    }
}

/// The page header: the session line and, while connected, Sign out and Start FFXI.
fn header(view: &FriendsView, x: &Ctx, actions: &mut Actions) {
    let state = &x.app.friends;
    let mut description = String::new();
    if state.connected {
        description.push_str(match state.snapshot.as_ref().and_then(|s| s.game_running) {
            Some(false) => t("Signed in to PlayOnline"),
            Some(true) => t("FFXI session active"),
            None if x.busy => t("Signing in to PlayOnline…"),
            None => t("PlayOnline session active"),
        });
        if let Some(snapshot) = state.snapshot.as_ref().filter(|s| s.fetched_at != 0) {
            let stamp = chrono::DateTime::from_timestamp(snapshot.fetched_at as i64, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
                .unwrap_or_default();
            let count = snapshot.entries.iter().filter(|f| !f.alternate).count();
            description.push_str(" · ");
            description.push_str(&tf("{count} friends · Updated {time}", &[("count", &count), ("time", &stamp)]));
        }
    }
    put!(view, get_description, set_description, description.into());
    put!(view, get_connected, set_connected, state.connected);
    actions.sign_out = (state.connected && x.idle && !x.busy).then_some(Message::FriendsSignOut);
    actions.play = (state.connected && x.idle && !x.busy && !x.app.launch_blocked()).then_some(Message::FriendsPlay);
    put!(view, get_sign_out_enabled, set_sign_out_enabled, actions.sign_out.is_some());
    put!(view, get_play_enabled, set_play_enabled, actions.play.is_some());
}

/// Session problems belong to the whole page.
fn problems(x: &Ctx, state: &State) {
    let friends = &x.app.friends;
    let mut problems: Vec<String> = Vec::new();
    if !friends.error.is_empty() {
        problems.push(friends.error.clone());
    }
    if let Some(error) = friends.snapshot.as_ref().and_then(|s| s.session_error.as_ref()) {
        problems.push(error.clone());
    }
    let shown = friends.snapshot.as_ref().filter(|s| s.fetched_at != 0 && friends.connected);
    if let Some(error) = shown.and_then(|s| s.error.as_ref()) {
        problems.push(tf("{error} The previous list is still shown.", &[("error", error)]));
    }
    strings_model(&state.problems, problems.iter().map(|problem| private(x.app, problem)));
}

/// The account block: its name (or the switcher's choices) for the sign-in card and sidebar.
fn account_face(view: &FriendsView, x: &Ctx, state: &State, actions: &mut Actions) {
    let app = x.app;
    let label = x.account.map(|a| a.account.label.clone()).unwrap_or_default();
    let shown = if label.is_empty() { t("No account").to_owned() } else { hidden(&label) };
    put!(view, get_account_label, set_account_label, shown.into());
    let retail: Vec<&crate::AccountRow> = app.accounts.iter().filter(|a| a.account.mode == crate::AccountMode::Retail).collect();
    put!(view, get_switchable, set_switchable, retail.len() > 1);
    // A drop-down draws its own labels: masked in privacy mode.
    strings_model(&state.accounts, retail.iter().map(|a| privacy::mask(&a.account.label)));
    let current = retail.iter().position(|a| app.friends.account.as_ref() == Some(&a.key)).map_or(-1, |i| i as i32);
    put!(view, get_account_current, set_account_current, current);
    actions.account_keys = if retail.len() > 1 { retail.iter().map(|a| a.key.clone()).collect() } else { Vec::new() };
}

fn sign_in(view: &FriendsView, x: &Ctx, actions: &mut Actions) {
    let app = x.app;
    let state = &app.friends;
    let retail = app.accounts.iter().any(|a| a.account.mode == crate::AccountMode::Retail);
    let message = if !retail {
        t("Add a retail account to use PlayOnline friends.")
    } else if state.snapshot.as_ref().is_some_and(|s| s.error.is_some()) {
        t("Sign in again to reconnect to PlayOnline.")
    } else if state.connected {
        t("Loading your PlayOnline friend list…")
    } else {
        t("Sign in to use PlayOnline friends.")
    };
    put!(view, get_sign_in_message, set_sign_in_message, message.into());
    let error = state.snapshot.as_ref().and_then(|s| s.error.as_ref()).map(|error| private(app, &tf("Could not load friends: {error}", &[("error", error)])));
    put!(view, get_sign_in_error, set_sign_in_error, error.unwrap_or_default().into());
    let controls = retail && !state.connected;
    put!(view, get_sign_in_controls, set_sign_in_controls, controls);
    let several = app.accounts.iter().filter(|a| a.account.mode == crate::AccountMode::Retail).count() > 1;
    put!(view, get_sign_in_switcher, set_sign_in_switcher, several);
    put!(view, get_otp_shown, set_otp_shown, x.account.is_some());
    put!(view, get_otp, set_otp, x.account.map(|a| a.otp.as_str()).unwrap_or_default().into());
    actions.otp_key = controls.then(|| x.account.map(|a| a.key.clone())).flatten();
    put!(view, get_connect_label, set_connect_label, (if x.busy { t("Connecting…") } else { t("Sign in to PlayOnline") }).into());
    actions.connect = (controls && x.account.is_some() && !x.busy && app.vault_ready && !app.launch_blocked()).then_some(Message::FriendsConnect);
    put!(view, get_connect_enabled, set_connect_enabled, actions.connect.is_some());
    if !controls {
        actions.account_keys.clear();
    }
}

fn sidebar(view: &FriendsView, x: &Ctx, snapshot: &Snapshot, state: &mut State, actions: &mut Actions) {
    let app = x.app;
    let friends = &app.friends;
    // Own status: the Viewer's three choices, inline under the name.
    let selected = match snapshot.own_mode {
        Some(1) => Some(1),
        Some(4) => Some(2),
        Some(0) => Some(0),
        _ => None,
    };
    let presence = match snapshot.own_mode {
        Some(0) => 0,
        Some(1) => 1,
        _ => 3,
    };
    put!(view, get_own_presence, set_own_presence, presence);
    actions.status = x.idle && !x.busy;
    put!(view, get_status_editable, set_status_editable, actions.status);
    let status_text = if actions.status {
        selected.map_or(t("Set your status"), |i: usize| [t("Visible"), t("Away"), t("Invisible")][i])
    } else {
        match (x.in_game, selected) {
            (true, _) => t("In FINAL FANTASY XI"),
            (false, Some(1)) => t("Away"),
            (false, Some(2)) => t("Invisible"),
            (false, Some(_)) => t("Visible"),
            (false, None) => t("Waiting for PlayOnline status"),
        }
    };
    put!(view, get_status_text, set_status_text, status_text.into());
    put!(view, get_status_current, set_status_current, selected.map_or(-1, |i| i as i32));
    put!(view, get_options_open, set_options_open, friends.status_options);
    put!(view, get_groups, set_groups, !friends.skip_groups);
    actions.auto_away = x.account.is_some();
    put!(view, get_auto_away_shown, set_auto_away_shown, actions.auto_away);
    let current = x.account.map(|account| app.settings.auto_away_for(&account.account));
    let current = current.and_then(|minutes| crate::pol_less::AUTO_AWAY_CHOICES.iter().position(|m| *m == minutes)).map_or(-1, |i| i as i32);
    put!(view, get_auto_away_current, set_auto_away_current, current);

    // The search, masked and closed to typing in privacy mode.
    actions.query = !privacy::on();
    put!(view, get_query_enabled, set_query_enabled, actions.query);
    put!(view, get_query, set_query, privacy::mask(&friends.query).into());

    let (list, _) = crate::friends::split_lists(&snapshot.entries, &friends.query);
    let any_match = !list.is_empty();
    // No status yet does not mean offline: those friends get their own section.
    let (mut online, mut offline, mut waiting) = (Vec::new(), Vec::new(), Vec::new());
    for friend in list {
        match friend.online {
            Some(true) => online.push(friend),
            Some(false) => offline.push(friend),
            None => waiting.push(friend),
        }
    }
    let open = friends.friend();
    let mut rows = Vec::new();
    let line = |kind: i32, text: String| FriendLine { kind, text: text.into(), ..FriendLine::default() };
    // Each row's target, kept from the last sync where it is the same.
    let targets = &mut state.rows_to;
    let section = |targets: &mut Vec<Option<RowTarget>>, i: usize, index: usize| {
        keep_at(targets, i, &index, |old, index| *old == Some(RowTarget::Section(*index)), |index| Some(RowTarget::Section(*index)));
    };
    let person = |targets: &mut Vec<Option<RowTarget>>, i: usize, friend: &Friend| {
        keep_at(
            targets,
            i,
            friend,
            |old, f| matches!(old, Some(RowTarget::Friend { slot, name }) if *slot == f.slot && *name == f.name),
            |f| Some(RowTarget::Friend { slot: f.slot, name: f.name.clone() }),
        );
    };
    for (index, (label, group)) in [(t("Online"), online), (t("Offline"), offline), (t("Waiting"), waiting)].into_iter().enumerate() {
        if index == 2 && group.is_empty() {
            continue;
        }
        let folded = friends.collapsed[index];
        section(targets, rows.len(), index);
        rows.push(FriendLine { folded, ..line(0, format!("{} · {}", label.to_uppercase(), group.len())) });
        if folded {
            continue;
        }
        for friend in group {
            let selected = open.is_some_and(|f| f.slot == friend.slot && f.name == friend.name);
            let unread = friends.unread_from(friend);
            let (initial, tint) = face(&friend.name);
            person(targets, rows.len(), friend);
            rows.push(
                FriendLine {
                    kind: 1,
                    text: hidden(&display_name(friend)).into(),
                    folded: false,
                    secondary: secondary(friend).into(),
                    initial: initial.into(),
                    tint,
                    presence: presence_of(friend),
                    selected,
                    unread: if unread > 0 { unread.to_string().into() } else { SharedString::new() },
                    bold: unread > 0,
                    strong: unread > 0 || selected,
                },
            );
        }
    }
    let note = if !snapshot.entries.iter().any(|f| !f.alternate) {
        Some(t("No friends returned for this account."))
    } else if !friends.query.is_empty() && !any_match {
        Some(t("No friends match your search."))
    } else {
        None
    };
    if let Some(note) = note {
        keep_at(targets, rows.len(), &(), |old, _| old.is_none(), |_| None);
        rows.push(line(2, note.into()));
    }
    targets.truncate(rows.len());
    actions.rows = true;
    sync_model(&state.rows, rows);

    let unread = snapshot.social.inbox_unread.unwrap_or(0) as usize;
    let mailbox = if unread > 0 { unread.to_string() } else if snapshot.social.inbox_changed { t("New").to_owned() } else { String::new() };
    put!(view, get_mailbox_count, set_mailbox_count, mailbox.into());
    let blocked = snapshot.entries.iter().filter(|f| f.alternate).count();
    put!(view, get_blocked_count, set_blocked_count, if blocked > 0 { blocked.to_string().into() } else { SharedString::new() });
}

/// The right side: the conversation, the Mailbox, the block list or the welcome note.
fn panes(view: &FriendsView, x: &Ctx, snapshot: &Snapshot, state: &mut State, actions: &mut Actions) {
    let friends = &x.app.friends;
    let social = &snapshot.social;
    put!(view, get_in_game, set_in_game, x.in_game);
    // "Check for new messages", enabled while a request can start.
    let checking = x.busy && social.action == Some(crate::social::Action::Check);
    put!(view, get_check_label, set_check_label, (if checking { t("Checking…") } else { t("Check for new messages") }).into());
    actions.check = x.ready.then_some(Message::FriendsSocial(Request::Check {}));
    put!(view, get_check_enabled, set_check_enabled, actions.check.is_some());
    let localized = |text: &String| private(x.app, &crate::social::localize(text));
    let pane = match &friends.pane {
        Some(Pane::Friend { .. }) => match friends.friend() {
            Some(friend) => {
                conversation(view, x, friend, state, actions);
                // A plain "sent" is the message appearing; other notices still show.
                let notice = social.notice.as_ref().filter(|n| n.as_str() != "Message sent by PlayOnline.");
                put!(view, get_pane_notice, set_pane_notice, notice.map(localized).unwrap_or_default().into());
                confirm(view, x, friends.remove.as_ref().filter(|f| !f.alternate), actions);
                1
            }
            None => 0,
        },
        Some(Pane::Mailbox) => {
            mailbox(view, x, snapshot, state, actions);
            2
        }
        Some(Pane::Blocked) => {
            blocked(x, snapshot, state, actions);
            put!(view, get_pane_notice, set_pane_notice, social.notice.as_ref().map(localized).unwrap_or_default().into());
            confirm(view, x, friends.remove.as_ref().filter(|f| f.alternate), actions);
            3
        }
        None => 0,
    };
    put!(view, get_pane, set_pane, pane);
    put!(view, get_pane_error, set_pane_error, social.error.as_ref().map(localized).unwrap_or_default().into());
}

/// The Remove / Unblock question for `friend`, when one is waiting.
fn confirm(view: &FriendsView, x: &Ctx, friend: Option<&Friend>, actions: &mut Actions) {
    put!(view, get_confirm_shown, set_confirm_shown, friend.is_some());
    let Some(friend) = friend else { return };
    let shown = display_name(friend);
    let (question, action) = if friend.alternate {
        (tf("Unblock {name}? Their messages will reach you again.", &[("name", &shown)]), t("Unblock"))
    } else {
        (tf("Remove {name} from your PlayOnline friends?", &[("name", &shown)]), t("Remove"))
    };
    put!(view, get_confirm_question, set_confirm_question, private(x.app, &question).into());
    put!(view, get_confirm_action, set_confirm_action, action.into());
    put!(view, get_confirm_danger, set_confirm_danger, !friend.alternate);
    actions.confirm = x.ready.then_some(Message::FriendsRemoveConfirm);
    put!(view, get_confirm_enabled, set_confirm_enabled, actions.confirm.is_some());
}

fn conversation(view: &FriendsView, x: &Ctx, friend: &Friend, state: &mut State, actions: &mut Actions) {
    let friends = &x.app.friends;
    // Another friend's conversation starts at its newest message.
    if state.open.as_ref().is_none_or(|(slot, name)| *slot != friend.slot || *name != friend.name) {
        state.open = Some((friend.slot, friend.name.clone()));
        state.chat_key = state.chat_key.wrapping_add(1);
        view.set_chat_key(state.chat_key);
    }
    let (initial, tint) = face(&friend.name);
    put!(view, get_friend_name, set_friend_name, hidden(&display_name(friend)).into());
    put!(view, get_friend_secondary, set_friend_secondary, secondary(friend).into());
    put!(view, get_friend_initial, set_friend_initial, initial.into());
    put!(view, get_friend_tint, set_friend_tint, tint);
    put!(view, get_friend_presence, set_friend_presence, presence_of(friend));
    actions.remove = x.ready && friend.uid.is_some();
    keep_friend(&mut state.remove_to, friend);
    put!(view, get_remove_enabled, set_remove_enabled, actions.remove);

    let chat = messages(x, friend);
    match chat {
        Chat::Empty { title, body } => {
            put!(view, get_chat_state, set_chat_state, 1);
            put!(view, get_chat_title, set_chat_title, title.into());
            put!(view, get_chat_body, set_chat_body, body.into());
            set_blocks(state, Vec::new());
        }
        Chat::Blocks { newest, blocks } => {
            put!(view, get_chat_state, set_chat_state, 0);
            put!(view, get_chat_newest, set_chat_newest, newest);
            set_blocks(state, blocks);
        }
    }

    let composer = if x.in_game { 0 } else if friends.composer.is_some() { 1 } else { 2 };
    put!(view, get_composer, set_composer, composer);
    if let (1, Some(draft)) = (composer, friends.composer.as_ref()) {
        composer_view(view, x, draft, state, actions);
    }
}

/// Replaces the chat model's blocks that changed (each block's items as one new model).
fn set_blocks(state: &mut State, blocks: Vec<Block>) {
    let model = &state.chat;
    let old = model.row_count();
    for (i, block) in blocks.iter().enumerate() {
        if i < old && state.blocks.get(i) == Some(block) {
            continue;
        }
        let row = to_row(block);
        if i < old { model.set_row_data(i, row) } else { model.push(row) }
    }
    for i in (blocks.len()..old).rev() {
        model.remove(i);
    }
    state.blocks = blocks;
}

fn to_row(block: &Block) -> ChatBlock {
    match block {
        Block::Divider { label, accent } => ChatBlock { kind: 0, label: label.into(), accent: *accent, ..ChatBlock::default() },
        Block::Group { mine, who, time, initial, tint, items } => ChatBlock {
            kind: 1,
            mine: *mine,
            who: who.into(),
            time: time.into(),
            initial: initial.into(),
            tint: *tint,
            items: ModelRc::new(VecModel::from(
                items
                    .iter()
                    .map(|item| ChatItem {
                        subject: item.subject.as_str().into(),
                        text: item.text.as_str().into(),
                        italic: item.italic,
                        skeleton: item.skeleton,
                        pending: item.pending,
                    })
                    .collect::<Vec<_>>(),
            )),
            ..ChatBlock::default()
        },
    }
}

enum Chat {
    Empty { title: String, body: String },
    Blocks { newest: bool, blocks: Vec<Block> },
}

/// A day divider's label, against `today` (read once per sync).
fn day_label(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    if day == today {
        return t("Today").to_owned();
    }
    if Some(day) == today.pred_opt() {
        return t("Yesterday").to_owned();
    }
    match crate::i18n::current() {
        Lang::Ja => {
            const WEEKDAYS: [&str; 7] = ["月", "火", "水", "木", "金", "土", "日"];
            let weekday = WEEKDAYS[day.weekday().num_days_from_monday() as usize];
            if day.year() == today.year() {
                format!("{}月{}日（{weekday}）", day.month(), day.day())
            } else {
                format!("{}年{}月{}日（{weekday}）", day.year(), day.month(), day.day())
            }
        }
        Lang::En => {
            if day.year() == today.year() { day.format("%A, %B %-d").to_string() } else { day.format("%A, %B %-d, %Y").to_string() }
        }
    }
}

/// One message's subject (small, muted) and text, or the skeleton while it is read.
fn message_item(entry: &Entry, body: Option<&Body>) -> Item {
    let subject = body.map_or(entry.subject.as_str(), |b| b.subject.as_str()).trim().to_owned();
    let unsupported = body.is_some_and(Body::unsupported);
    Item {
        subject: if subject.is_empty() || unsupported { String::new() } else { hidden(&subject) },
        text: match body {
            Some(body) if unsupported => crate::social::localize(&body.message),
            Some(body) => hidden(&body.message),
            None => String::new(),
        },
        italic: unsupported,
        skeleton: body.is_none(),
        pending: false,
    }
}

/// The conversation's messages grouped for display:
/// a day divider at each new day, the "new" divider before the first unread, and runs from one
/// side within ten minutes under one name.
fn messages(x: &Ctx, friend: &Friend) -> Chat {
    let state = &x.app.friends;
    let thread = state.thread(friend);
    let outgoing = state.outgoing.as_ref().filter(|o| Some(o.uid) == friend.uid);
    if thread.is_empty() && outgoing.is_none() {
        let unlisted = state.mail.listed[0].is_none() || state.mail.listed[2].is_none();
        let loading = state.mail.listed[0].is_none() || (state.syncing() && state.mail.listed[2].is_none());
        let (title, body) = if x.in_game && unlisted {
            (t("FINAL FANTASY XI is running"), "")
        } else if state.mail.stalled && unlisted {
            (t("Check for new messages"), "")
        } else if loading {
            (t("Loading messages…"), "")
        } else {
            (t("No messages yet"), t("Send a message to start the conversation."))
        };
        return Chat::Empty { title: title.into(), body: body.into() };
    }
    let newest = state.mail.listed.iter().flatten().any(|page| page.len() >= 32);
    let shown = hidden(&display_name(friend));
    let (friend_initial, friend_tint) = face(&friend.name);
    let group = |mine: bool, time: Option<chrono::DateTime<chrono::Local>>| Block::Group {
        mine,
        who: if mine { t("You").to_owned() } else { shown.clone() },
        time: time.map(|time| time.format("%H:%M").to_string()).unwrap_or_default(),
        initial: if mine { String::new() } else { friend_initial.clone() },
        tint: if mine { -1 } else { friend_tint },
        items: Vec::new(),
    };
    let push = |blocks: &mut Vec<Block>, item: Item| {
        if let Some(Block::Group { items, .. }) = blocks.last_mut() {
            items.push(item);
        }
    };
    let mut blocks: Vec<Block> = Vec::new();
    let today = chrono::Local::now().date_naive();
    let fresh =thread.iter().position(|e| !e.mine() && state.mail.fresh.contains(&e.key));
    let mut day: Option<chrono::NaiveDate> = None;
    let mut last: Option<(bool, Option<i64>)> = None;
    for (index, entry) in thread.iter().enumerate() {
        let time = message_time(entry.stamp);
        let mut breaks = false;
        if let Some(time) = time {
            if day != Some(time.date_naive()) {
                day = Some(time.date_naive());
                blocks.push(Block::Divider { label: day_label(time.date_naive(), today), accent: false });
                breaks = true;
            }
        }
        if fresh == Some(index) {
            blocks.push(Block::Divider { label: t("New messages").to_owned(), accent: true });
            breaks = true;
        }
        let seconds = time.map(|t| t.timestamp());
        let continues = !breaks
            && last.is_some_and(|(mine, at)| mine == entry.mine() && match (at, seconds) {
                (Some(a), Some(b)) => b - a < 10 * 60,
                _ => true,
            });
        if !continues {
            blocks.push(group(entry.mine(), time));
        }
        push(&mut blocks, message_item(entry, state.mail.bodies.get(&entry.key)));
        last = Some((entry.mine(), seconds));
    }
    if let Some(outgoing) = outgoing {
        if !last.is_some_and(|(mine, _)| mine) {
            blocks.push(group(true, None));
        }
        push(&mut blocks, Item {
            subject: hidden(&outgoing.subject),
            text: hidden(&outgoing.message),
            pending: !outgoing.sent,
            ..Item::default()
        });
    }
    Chat::Blocks { newest, blocks }
}

fn composer_view(view: &FriendsView, x: &Ctx, composer: &crate::friends::Composer, state: &mut State, actions: &mut Actions) {
    let friends = &x.app.friends;
    let sending = friends.outgoing.as_ref().is_some_and(|o| !o.sent);
    let can_send = x.ready && !sending && friends.send_request().is_some();
    let editable = !x.busy;
    let hide = privacy::on();
    actions.send = can_send.then_some(Message::FriendsSend);
    put!(view, get_can_send, set_can_send, can_send);
    put!(view, get_show_subject, set_show_subject, composer.show_subject);
    actions.subject = editable && !hide;
    put!(view, get_subject_enabled, set_subject_enabled, actions.subject);
    put!(view, get_subject, set_subject, privacy::mask(&composer.subject).into());
    // Privacy mode: no editor for the real content, only the fixed mask.
    actions.draft = editable && !hide;
    put!(view, get_draft_masked, set_draft_masked, hide);
    put!(view, get_draft_enabled, set_draft_enabled, actions.draft);
    let draft = composer.text.clone();
    let core = draft.trim_end_matches('\n');
    put!(view, get_draft_lines, set_draft_lines, (composer.text.matches('\n').count() + 1).clamp(1, 6) as i32);
    if hide {
        put!(view, get_draft, set_draft, SharedString::new());
        state.draft.clear();
    } else {
        // The box keeps what was typed (its line breaks and cursor); the core's text replaces it
        // only when they differ (a send clearing the draft, another friend's conversation).
        if view.get_draft().trim_end_matches('\n') != core {
            view.set_draft(core.into());
        }
        state.draft = core.to_owned();
    }

    // Under the box: the subject switch, then the keys or the limits.
    let text_len = crate::friends::cp932_len(core);
    let subject_len = crate::friends::cp932_len(&composer.subject);
    let (note, tone) = if hide {
        (t("Privacy mode").to_owned(), 0)
    } else {
        match (text_len, subject_len) {
            (None, _) | (_, None) if composer.show_subject || text_len.is_none() => (t("Contains characters PlayOnline can't send.").to_owned(), 2),
            // Whatever blocks Send is named first: an over-long message, then an over-long subject.
            (Some(n), _) if n > TEXT_MAX => (tf("{used} of {limit} bytes", &[("used", &n), ("limit", &TEXT_MAX)]), 2),
            (_, Some(n)) if composer.show_subject && n > SUBJECT_MAX => (tf("Subject: {used} of {limit} bytes", &[("used", &n), ("limit", &SUBJECT_MAX)]), 2),
            (Some(n), _) if n > TEXT_MAX * 3 / 4 => (tf("{used} of {limit} bytes", &[("used", &n), ("limit", &TEXT_MAX)]), 1),
            _ => (t("Enter to send · Shift+Enter for a new line").to_owned(), 0),
        }
    };
    put!(view, get_note, set_note, note.into());
    put!(view, get_note_tone, set_note_tone, tone);
    put!(view, get_subject_toggle, set_subject_toggle, (if composer.show_subject { t("Remove subject") } else { t("Add subject") }).into());
    actions.add_subject = (editable && !hide).then_some(Message::FriendsAddSubject(!composer.show_subject));
    put!(view, get_subject_toggle_enabled, set_subject_toggle_enabled, actions.add_subject.is_some());
}

fn mailbox(view: &FriendsView, x: &Ctx, snapshot: &Snapshot, state: &mut State, actions: &mut Actions) {
    let social = &snapshot.social;
    let enabled = x.ready;
    put!(view, get_folder, set_folder, FOLDERS.iter().position(|f| *f == social.folder).map_or(-1, |i| i as i32));
    put!(view, get_folders_enabled, set_folders_enabled, enabled);
    actions.folders = FOLDERS.map(|folder| enabled.then_some(Message::FriendsSocial(Request::Inbox { folder, page: 0 })));
    let page = social.page;
    let pages = social.total.div_ceil(32).max(1);
    put!(view, get_mail_count, set_mail_count, tf("{total} messages · Page {page} of {pages}", &[("total", &social.total), ("page", &(page + 1)), ("pages", &pages)]).into());
    // Paging only when there is more than one page.
    put!(view, get_paging, set_paging, pages > 1);
    actions.previous = (pages > 1 && enabled && page > 0).then(|| Message::FriendsSocial(Request::Inbox { folder: social.folder, page: page - 1 }));
    actions.next = (pages > 1 && enabled && page + 1 < pages).then(|| Message::FriendsSocial(Request::Inbox { folder: social.folder, page: page + 1 }));
    put!(view, get_previous_enabled, set_previous_enabled, actions.previous.is_some());
    put!(view, get_next_enabled, set_next_enabled, actions.next.is_some());
    put!(view, get_mail_changed, set_mail_changed, social.inbox_changed);

    let names = if privacy::on() { crate::privacy::identities(x.app) } else { Vec::new() };
    let correspondent = |sender: &str, recipient: Option<u64>| -> String {
        let line = if social.folder == 2 {
            snapshot
                .entries
                .iter()
                .find(|f| f.uid.is_some() && f.uid == recipient)
                .map_or_else(|| t("Sent message").to_owned(), |f| tf("To {name}", &[("name", &f.name)]))
        } else {
            tf("From {sender}", &[("sender", &sender)])
        };
        masked(&line, &names)
    };
    put!(view, get_opened, set_opened, social.opened.is_some());
    if let Some(opened) = &social.opened {
        put!(view, get_opened_from, set_opened_from, correspondent(&opened.sender, opened.recipient).into());
        put!(view, get_opened_subject, set_opened_subject, hidden(&opened.subject).into());
        put!(view, get_opened_text, set_opened_text, hidden(&crate::social::localize(&opened.message)).into());
    }
    let mut lines = Vec::with_capacity(social.headers.len());
    actions.reads = enabled;
    for (i, header) in social.headers.iter().enumerate() {
        keep_at(&mut state.mail_to, i, header.raw.as_str(), |old, raw| old == raw, str::to_owned);
        let mine = social.folder == 2;
        let (initial, tint) = if mine { (String::new(), -1) } else { face(&header.sender) };
        lines.push(MailLine {
            unread: header.unread,
            mine,
            initial: initial.into(),
            tint,
            correspondent: correspondent(&header.sender, header.recipient).into(),
            subject: (if header.subject.is_empty() { t("No subject").to_owned() } else { hidden(&header.subject) }).into(),
            enabled,
        });
    }
    state.mail_to.truncate(social.headers.len());
    sync_model(&state.mail, lines);
}

fn blocked(x: &Ctx, snapshot: &Snapshot, state: &mut State, actions: &mut Actions) {
    let mut lines = Vec::new();
    actions.unblocks.clear();
    for (i, friend) in snapshot.entries.iter().filter(|f| f.alternate).enumerate() {
        let (initial, tint) = face(&friend.name);
        let enabled = x.ready && friend.uid.is_some();
        if state.blocked_to.len() <= i {
            state.blocked_to.push(None);
        }
        keep_friend(&mut state.blocked_to[i], friend);
        lines.push(BlockedLine { name: hidden(&display_name(friend)).into(), initial: initial.into(), tint, enabled });
        actions.unblocks.push(enabled);
    }
    state.blocked_to.truncate(lines.len());
    sync_model(&state.blocked, lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::sync::headless;
    use slint::platform::software_renderer::MinimalSoftwareWindow;

    /// The real window on a headless software renderer (one per test thread), at 1440x900.
    fn window() -> MainWindow {
        let window = headless::window(1440, 900);
        SCREEN.with(|screen| *screen.borrow_mut() = Some(window));
        let ui = MainWindow::new().unwrap();
        super::super::view::wire(&ui);
        wire(&ui);
        ui.show().unwrap();
        ui
    }

    /// The Friends preview's data: Alyria's conversation open (or the Mailbox).
    fn preview(mailbox: bool, in_game: bool) -> App {
        let mut app = crate::tests::app();
        app.load_preview();
        app.load_friends_preview(mailbox, in_game, false, false);
        app
    }

    thread_local! {
        static SCREEN: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    }

    /// Syncs the window to `app` and draws a frame (which builds the shown page's items).
    fn render(ui: &MainWindow, app: &App) {
        super::super::view::sync(ui, app);
        sync(ui, app);
        SCREEN.with(|screen| {
            if let Some(window) = screen.borrow().as_ref() {
                headless::draw(window);
            }
        });
    }

    fn sent() -> Vec<Message> {
        SENT.with(|sent| std::mem::take(&mut *sent.borrow_mut()))
    }

    #[test]
    fn the_conversation_shows_sections_groups_and_the_composer() {
        privacy::set(false);
        let ui = window();
        let mut app = preview(false, false);
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        assert!(view.get_shown());
        assert_eq!(view.get_pane(), 1, "Alyria's conversation");
        assert_eq!(view.get_friend_name(), "Alyria");
        assert_eq!(view.get_friend_secondary(), "Lyrielle · Port Jeuno");
        let rows: Vec<FriendLine> = view.get_rows().iter().collect();
        assert_eq!(rows[0].kind, 0);
        assert_eq!(rows[0].text, "ONLINE · 3");
        assert!(rows.iter().any(|r| r.kind == 0 && r.text == "WAITING · 1"), "the Waiting section shows when it has friends");
        let alyria = rows.iter().find(|r| r.text == "Alyria").unwrap();
        assert!(alyria.selected && alyria.strong && alyria.presence == 0);
        assert_eq!(view.get_mailbox_count(), "1");
        assert_eq!(view.get_blocked_count(), "1");
        // Day dividers, the "new" divider before the unread one, and grouped runs.
        assert_eq!(view.get_chat_state(), 0);
        let blocks: Vec<ChatBlock> = view.get_chat().iter().collect();
        assert_eq!(blocks[0].kind, 0);
        assert_eq!(blocks[0].label, t("Yesterday"));
        assert!(blocks.iter().any(|b| b.kind == 0 && b.accent && b.label == t("New messages")));
        let mine = blocks.iter().find(|b| b.kind == 1 && b.mine).unwrap();
        assert_eq!(mine.who, t("You"));
        assert_eq!(mine.items.row_count(), 2, "two of mine within ten minutes are one group");
        assert_eq!(view.get_composer(), 1);
        assert!(!view.get_can_send(), "an empty draft can't be sent");
        assert_eq!(view.get_note(), t("Enter to send · Shift+Enter for a new line"));

        // A second sync with nothing changed replaces nothing.
        let before = view.get_chat().row_data(1).unwrap().items;
        render(&ui, &app);
        assert!(view.get_chat().row_data(1).unwrap().items == before, "unchanged blocks keep their models");

        // Each control sends its message.
        sent();
        view.invoke_row_clicked(0);
        view.invoke_check();
        view.invoke_remove();
        view.invoke_open_mailbox();
        view.invoke_add_subject();
        view.invoke_status_picked(1);
        view.invoke_send();
        let messages = sent();
        assert!(matches!(messages[0], Message::FriendsSection(0)));
        assert!(matches!(messages[1], Message::FriendsSocial(Request::Check {})));
        assert!(matches!(&messages[2], Message::FriendsRemove(f) if f.name == "Alyria"));
        assert!(matches!(messages[3], Message::FriendsSelect(Some(Pane::Mailbox))));
        assert!(matches!(messages[4], Message::FriendsAddSubject(true)));
        assert!(matches!(messages[5], Message::FriendsStatus(NativeControl::Away)));
        assert_eq!(messages.len(), 6, "Send is disabled: it sends nothing");

        // Typing goes through the editor's own actions, and the core's text comes back.
        view.invoke_draft_edited("Hi there".into());
        for message in sent() {
            let _ = app.update(message);
        }
        assert_eq!(app.friends.composer.as_ref().unwrap().text.trim_end_matches('\n'), "Hi there");
        render(&ui, &app);
        assert!(view.get_can_send());
        view.invoke_send();
        assert!(matches!(sent().as_slice(), [Message::FriendsSend]));

        // A friend row opens that friend; another conversation moves the chat key (the messages
        // start at its newest), the same one leaves it.
        let key = view.get_chat_key();
        render(&ui, &app);
        assert_eq!(view.get_chat_key(), key, "the same conversation keeps its scroll");
        let balthier = view.get_rows().iter().position(|r| r.text == "Balthier").unwrap();
        view.invoke_row_clicked(balthier as i32);
        let message = sent().pop().unwrap();
        assert!(matches!(&message, Message::FriendsSelect(Some(Pane::Friend { name, .. })) if name == "Balthier"));
        let _ = app.update(message);
        render(&ui, &app);
        assert_eq!(view.get_friend_name(), "Balthier");
        assert_ne!(view.get_chat_key(), key);
        view.invoke_remove();
        assert!(matches!(sent().as_slice(), [Message::FriendsRemove(f)] if f.name == "Balthier"), "Remove acts on the open friend");
    }

    /// The focused control's name: the focus scope's or text input's, else its parent's.
    fn focused(ui: &MainWindow) -> String {
        use i_slint_core::accessibility::AccessibleStringProperty;
        let inner = i_slint_core::window::WindowInner::from_pub(ui.window());
        let item = inner.focus_item.borrow().upgrade();
        let Some(item) = item else { return String::new() };
        let own = item.accessible_string_property(AccessibleStringProperty::Label).unwrap_or_default();
        if !own.is_empty() {
            return own.to_string();
        }
        item.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::StopAtPopups)
            .and_then(|parent| parent.accessible_string_property(AccessibleStringProperty::Label))
            .unwrap_or_default()
            .to_string()
    }

    /// Every control the D-pad reaches from the first one, trying each way from each, by name.
    /// With `stop`, the walk ends with the focus on the first control of that name.
    fn walk(ui: &MainWindow, stop: Option<&str>) -> std::collections::BTreeSet<String> {
        use super::super::pad::{step, Way};
        let inner = i_slint_core::window::WindowInner::from_pub(ui.window());
        assert!(step(ui.window(), Way::Down));
        let mut found = vec![inner.focus_item.borrow().upgrade().unwrap()];
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(focused(ui));
        let mut next = 0;
        while next < found.len() && found.len() < 300 {
            let from = found[next].clone();
            next += 1;
            for way in [Way::Up, Way::Down, Way::Left, Way::Right] {
                inner.set_focus_item(&from, true, i_slint_core::input::FocusReason::TabNavigation);
                if !step(ui.window(), way) {
                    continue;
                }
                let name = focused(ui);
                if stop == Some(name.as_str()) {
                    seen.insert(name);
                    return seen;
                }
                seen.insert(name);
                let at = inner.focus_item.borrow().upgrade().unwrap();
                if !found.contains(&at) {
                    found.push(at);
                }
            }
        }
        seen
    }

    fn reachable(ui: &MainWindow) -> std::collections::BTreeSet<String> {
        walk(ui, None)
    }

    /// The status options: the groups box and the Auto-Away choice, named by its value (not
    /// "Status options", the button beside it), both reachable.
    #[test]
    fn the_status_options_panel_is_reachable_and_names_its_choice() {
        privacy::set(false);
        let ui = window();
        let mut app = preview(false, false);
        let _ = app.update(Message::FriendsStatusOptions);
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        assert!(view.get_options_open());
        assert!(view.get_auto_away_shown());
        let current = view.get_auto_away_current();
        assert!(current >= 0);
        let choice = view.get_auto_away_items().row_data(current as usize).unwrap().to_string();
        let seen = reachable(&ui);
        for name in [t("Also set for my groups"), choice.as_str(), t("Status options")] {
            assert!(seen.contains(name), "{name} is reachable: {seen:?}");
        }
        sent();
        view.invoke_groups_toggled(false);
        view.invoke_auto_away_picked(0);
        let messages = sent();
        assert!(matches!(messages[0], Message::FriendsStatusGroups(false)));
        assert!(matches!(messages[1], Message::FriendsAutoAway(m) if m == crate::pol_less::AUTO_AWAY_CHOICES[0]));
    }

    /// Enter in the one-time code field does nothing (no Enter-to-sign-in); the
    /// button signs in.
    #[test]
    fn enter_in_the_code_field_does_not_sign_in() {
        privacy::set(false);
        let ui = window();
        let mut app = crate::tests::app();
        app.load_preview();
        app.page = crate::Page::Friends;
        app.friends.account = app.accounts.first().map(|a| a.key.clone());
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        assert!(view.get_otp_shown() && view.get_connect_enabled());
        let code = t("One-time code (optional)");
        walk(&ui, Some(code));
        assert_eq!(focused(&ui), code, "the walker reaches the code field");
        use super::super::pad::press;
        sent();
        press(ui.window());
        assert!(!sent().iter().any(|m| matches!(m, Message::FriendsConnect)), "Enter in the field doesn't sign in");
    }

    #[test]
    fn privacy_masks_names_messages_and_closes_the_fields() {
        privacy::set(true);
        let ui = window();
        let app = preview(false, false);
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        let mask = privacy::mask("x");
        assert_eq!(view.get_friend_name(), mask.as_str());
        assert!(!view.get_friend_secondary().contains("Lyrielle"), "the character is masked");
        assert!(view.get_friend_secondary().contains("Port Jeuno"), "the area is not a name");
        assert_eq!(view.get_friend_initial(), "", "no initial on the avatar");
        assert_eq!(view.get_friend_tint(), -1);
        for row in view.get_rows().iter().filter(|r| r.kind == 1) {
            assert_eq!(row.text, mask.as_str());
        }
        for block in view.get_chat().iter().filter(|b| b.kind == 1) {
            for item in block.items.iter() {
                assert!(item.text.is_empty() || item.text == mask.as_str(), "{}", item.text);
            }
        }
        assert!(!view.get_query_enabled());
        assert!(view.get_draft_masked());
        assert!(!view.get_draft_enabled());
        assert!(!view.get_subject_toggle_enabled());
        assert_eq!(view.get_note(), t("Privacy mode"));
        assert!(view.get_accounts().iter().all(|label| label == mask.as_str()));
        sent();
        view.invoke_query_edited("Al".into());
        view.invoke_draft_edited("leak".into());
        view.invoke_subject_edited("leak".into());
        assert!(sent().is_empty(), "nothing can be typed in privacy mode");
        privacy::set(false);
    }

    #[test]
    fn the_mailbox_blocked_list_and_in_game_states() {
        privacy::set(false);
        let ui = window();
        let mut app = preview(true, false);
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        assert_eq!(view.get_pane(), 2);
        assert_eq!(view.get_folder(), 0);
        assert!(view.get_opened());
        assert_eq!(view.get_opened_from(), "From Alyria");
        let mail: Vec<MailLine> = view.get_mail().iter().collect();
        assert_eq!(mail.len(), 5);
        assert!(mail.iter().any(|m| m.unread));
        sent();
        view.invoke_folder_picked(1);
        view.invoke_read(0);
        let messages = sent();
        assert!(matches!(messages[0], Message::FriendsSocial(Request::Inbox { folder: 2, page: 0 })));
        assert!(matches!(&messages[1], Message::FriendsSocial(Request::Read { .. })));

        // The block list, and its Unblock confirmation.
        let _ = app.update(Message::FriendsSelect(Some(Pane::Blocked)));
        render(&ui, &app);
        assert_eq!(view.get_pane(), 3);
        let blocked: Vec<BlockedLine> = view.get_blocked().iter().collect();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].name, "Mallory");
        view.invoke_unblock(0);
        let message = sent().pop().unwrap();
        assert!(matches!(&message, Message::FriendsRemove(f) if f.alternate));
        let _ = app.update(message);
        render(&ui, &app);
        assert!(view.get_confirm_shown());
        assert!(!view.get_confirm_danger(), "an unblock is the primary action");
        view.invoke_confirm_cancel();
        assert!(matches!(sent().as_slice(), [Message::FriendsDraftClose]));

        // FFXI running: presence only, the notice instead of the composer.
        let app = preview(false, true);
        render(&ui, &app);
        assert!(view.get_in_game());
        assert_eq!(view.get_composer(), 0);
        assert!(!view.get_status_editable());
        assert_eq!(view.get_status_text(), t("In FINAL FANTASY XI"));
        assert!(!view.get_check_enabled());
    }

    #[test]
    fn signed_out_shows_the_sign_in_card() {
        privacy::set(false);
        let ui = window();
        let mut app = crate::tests::app();
        app.load_preview();
        app.page = crate::Page::Friends;
        app.friends.account = app.accounts.first().map(|a| a.key.clone());
        render(&ui, &app);
        let view = ui.global::<FriendsView>();
        assert!(!view.get_shown());
        assert!(!view.get_connected());
        assert_eq!(view.get_sign_in_message(), t("Sign in to use PlayOnline friends."));
        assert!(view.get_sign_in_controls());
        assert!(view.get_sign_in_switcher(), "three retail accounts");
        assert!(view.get_connect_enabled());
        sent();
        view.invoke_otp_edited("123456".into());
        view.invoke_account_picked(1);
        view.invoke_connect();
        let messages = sent();
        assert!(matches!(&messages[0], Message::OtpChanged(_, code) if code == "123456"));
        assert!(matches!(&messages[1], Message::FriendsAccount(key) if *key == app.accounts[1].key));
        assert!(matches!(messages[2], Message::FriendsConnect));
    }

    /// The controller walker reaches the page's controls: from the composer down to Send.
    #[test]
    fn the_controller_walker_reaches_the_friends_page() {
        privacy::set(false);
        let ui = window();
        let app = preview(false, false);
        render(&ui, &app);
        let seen = reachable(&ui);
        for name in [
            "Sign out", "Start FFXI", "Main account", "Visible", "Status options", "Find a friend…", "ONLINE · 3", "Alyria", "Nocturne",
            "Mailbox", "Blocked", "Check for new messages", "Remove from friends", "Write a PlayOnline message…", "Add subject",
        ] {
            assert!(seen.contains(name), "{name} is reachable: {seen:?}");
        }
    }
}
