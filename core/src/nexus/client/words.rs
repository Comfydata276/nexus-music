//! Every sentence the backend's part of the app shows, for a PC and for a phone.
// to be nexus::words': design 5.4 gives every sentence a person is shown a module of its own.
// Until it is made, these are the sentences, and nothing else in either app says them.

use super::{DeviceKind, PasswordState, SignInState};

/// "this PC" or "this phone".
fn this(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Pc => "this PC",
        DeviceKind::Phone => "this phone",
    }
}

/// Whole minutes, rounded up, at least one.
fn minutes(ms: u64) -> u64 {
    (ms.saturating_add(59_999) / 60_000).max(1)
}

fn wait(ms: u64) -> String {
    match minutes(ms) {
        1 => "Too many tries. Wait a minute, then try again.".to_owned(),
        n => format!("Too many tries. Wait {n} minutes, then try again."),
    }
}

pub(super) fn sign_in(state: SignInState, kind: DeviceKind, wait_ms: u64) -> String {
    match state {
        SignInState::Idle | SignInState::Working => String::new(),
        SignInState::WrongLogin => "Wrong email or password.".to_owned(),
        SignInState::TooManyTries => wait(wait_ms),
        SignInState::NeedsHttps => "Your Nexus server's address must start with https://.".to_owned(),
        SignInState::BadAddress => "That doesn't look like a server's address. Check it and try again.".to_owned(),
        SignInState::BadEmail => "That doesn't look like an email address.".to_owned(),
        SignInState::NoPassword => "Type your password.".to_owned(),
        SignInState::NoNexusServer => "There's no Nexus server at this address. Check it with whoever runs your server.".to_owned(),
        SignInState::CantReach => "Your Nexus server can't be reached. Check the address and your connection, then try again.".to_owned(),
        SignInState::NeedsPermission => {
            "Your Nexus server is on your home network. To reach it, allow Nexus Music to find and connect to nearby devices.".to_owned()
        }
        SignInState::AccountOff => "This account is switched off. Ask whoever runs your Nexus server.".to_owned(),
        SignInState::Busy => "Your Nexus server is busy. Try again in a moment.".to_owned(),
        SignInState::AppTooOld => "This copy of Nexus Music is too old for your Nexus server. Update it. Your music plays as usual.".to_owned(),
        SignInState::ServerTooOld => "Your Nexus server is too old for this copy of Nexus Music. Ask whoever runs it to update it.".to_owned(),
        SignInState::Offline => "Offline mode is on. Turn it off to sign in.".to_owned(),
        SignInState::NoRandom => format!("Nexus Music couldn't make a key for {}. Try again.", this(kind)),
        SignInState::PasswordsDiffer => "The two passwords are not the same.".to_owned(),
        SignInState::PasswordNotAllowed => password_rule(),
        SignInState::SignedOut => format!("You're signed out on {}. Your downloads are still here.", this(kind)),
        SignInState::ForcedOut => format!("{} was signed out of Nexus Music. Sign in again to carry on. Your downloads are still here.", capital(this(kind))),
        SignInState::StartAgain => "Your Nexus server was put back from a copy. Sign in again. Your music plays as usual.".to_owned(),
    }
}

fn capital(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn password_rule() -> String {
    format!(
        "A password is {} to {} characters, and not your email.",
        super::account::PASSWORD_MIN_CHARS,
        super::account::PASSWORD_MAX_CHARS
    )
}

pub(super) fn password(state: PasswordState, wait_ms: u64, offline: bool) -> String {
    match state {
        PasswordState::Idle | PasswordState::Working => String::new(),
        PasswordState::Changed => "Your password is changed. Your other devices are signed out.".to_owned(),
        PasswordState::WrongCurrent => "That isn't your present password.".to_owned(),
        PasswordState::NotAllowed => password_rule(),
        PasswordState::Differ => "The two new passwords are not the same.".to_owned(),
        PasswordState::TooManyTries => wait(wait_ms),
        PasswordState::CantReach if offline => "Offline mode is on. Turn it off to change your password.".to_owned(),
        PasswordState::CantReach => "Your Nexus server can't be reached. Try again when it can.".to_owned(),
    }
}

/// The row under Settings (17).
pub(super) enum Row<'a> {
    SignedOut,
    Offline,
    TooOld,
    Connected,
    SignedIn(&'a str),
    Down { since: String },
}

pub(super) fn row(row: Row<'_>) -> String {
    match row {
        Row::SignedOut => "Nexus server: not signed in".to_owned(),
        Row::Offline => "Nexus server: not asked while Offline mode is on. Your music plays as usual.".to_owned(),
        Row::TooOld => "This copy of Nexus Music is too old for your Nexus server. Update it. Your music plays as usual.".to_owned(),
        Row::Connected => "Nexus server: connected".to_owned(),
        Row::SignedIn(email) => format!("Nexus server: signed in as {email}"),
        Row::Down { since } => format!("Nexus server: not reachable since {since}. Your music plays as usual."),
    }
}

/// A time of day, as `14:02`, from milliseconds since 1970 UTC and minutes east of UTC.
pub(super) fn time_of_day(ms: u64, offset_minutes: i32) -> String {
    let minutes = i64::try_from(ms / 60_000).unwrap_or(0).saturating_add(i64::from(offset_minutes)).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// What is said of the music login (6.3).
pub(super) enum Music {
    /// Refused, and being renewed now.
    Renewing,
    /// Refused, and the backend can't be reached to renew it.
    WhenReachable,
    /// Refused, Offline is on.
    WhenOnline,
    /// Refused, and the backend has none newer.
    NoNewer,
    /// Refused, and nobody is signed in to renew it.
    SignInToRenew,
    /// The account has no music login set.
    NoneSet,
}

pub(super) fn music(music: Music) -> String {
    match music {
        Music::Renewing => "Your music sign-in is being renewed. What's downloaded plays meanwhile.".to_owned(),
        Music::WhenReachable => "Your music sign-in needs renewing. It will be, when your Nexus server can be reached.".to_owned(),
        Music::WhenOnline => "Your music sign-in needs renewing. It will be, when Offline mode is off.".to_owned(),
        Music::NoNewer => {
            "Your music server turned down your sign-in, and your Nexus server has no newer one. Ask whoever runs your server. What's downloaded plays meanwhile."
                .to_owned()
        }
        Music::SignInToRenew => "Your music sign-in needs renewing. Sign in to your Nexus server to renew it. What's downloaded plays meanwhile.".to_owned(),
        Music::NoneSet => "Your Nexus server has no music sign-in for you yet. Ask whoever runs it.".to_owned(),
    }
}

/// How recently a device in the list was used.
pub(super) fn seen(this_one: bool, signed_in: bool, last_ms: u64, now_ms: u64) -> String {
    if this_one {
        return "This device".to_owned();
    }
    if !signed_in {
        return "Signed out".to_owned();
    }
    const HOUR: u64 = 60 * 60 * 1000;
    let age = now_ms.saturating_sub(last_ms);
    match age / (24 * HOUR) {
        _ if age < HOUR => "Used in the last hour".to_owned(),
        0 => "Used today".to_owned(),
        1 => "Used yesterday".to_owned(),
        days => format!("Used {days} days ago"),
    }
}

pub(super) enum Devices {
    CantReach,
    Offline,
    Failed,
    ForcedOut { music_cut_off: bool },
    Gone,
}

pub(super) fn devices(devices: Devices) -> String {
    match devices {
        Devices::CantReach => "Your Nexus server can't be reached. Your music plays as usual.".to_owned(),
        Devices::Offline => "Offline mode is on. Turn it off to see your devices.".to_owned(),
        Devices::Failed => "Your devices couldn't be listed. Try again in a moment.".to_owned(),
        Devices::ForcedOut { music_cut_off: true } => "That device is signed out, and can no longer play your music.".to_owned(),
        Devices::ForcedOut { music_cut_off: false } => {
            "That device can no longer use Nexus Music. It still knows your music server's password: change that too.".to_owned()
        }
        Devices::Gone => "That device was already gone.".to_owned(),
    }
}
