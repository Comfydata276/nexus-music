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

/// "Busy, come back", with the seconds the backend said, if it said (8.3: a password may
/// wait its turn; it is never refused for wrong ones before it).
fn busy(seconds: u64) -> String {
    match seconds {
        0 => "Your Nexus server is busy. Try again in a moment.".to_owned(),
        1 => "Your Nexus server is busy. Try again in a second.".to_owned(),
        n => format!("Your Nexus server is busy. Try again in {n} seconds."),
    }
}

pub(super) fn sign_in(state: SignInState, kind: DeviceKind, wait_s: u64) -> String {
    match state {
        SignInState::Idle => String::new(),
        // 8.3: a password may wait its turn, up to a minute after a wrong one.
        SignInState::Working => "Signing in. When your Nexus server is busy this can take up to a minute.".to_owned(),
        SignInState::WrongLogin => "Wrong email or password.".to_owned(),
        SignInState::NeedsHttps => "Your Nexus server's address must start with https://.".to_owned(),
        SignInState::BadAddress => "That doesn't look like a server's address. Check it and try again.".to_owned(),
        SignInState::BadEmail => "That doesn't look like an email address.".to_owned(),
        SignInState::NoPassword => "Type your password.".to_owned(),
        SignInState::NoNexusServer => "There's no Nexus server at this address. Check it with whoever runs your server.".to_owned(),
        SignInState::CantReach => "Your Nexus server can't be reached. Check the address and your connection, then try again.".to_owned(),
        SignInState::NotSecure => "A secure connection to this server couldn't be made. Check the address with whoever runs your server.".to_owned(),
        SignInState::NeedsPermission => {
            "Your Nexus server is on your home network. To reach it, allow Nexus Music to find and connect to nearby devices.".to_owned()
        }
        SignInState::AccountOff => "This account is switched off. Ask whoever runs your Nexus server.".to_owned(),
        SignInState::Busy => busy(wait_s),
        SignInState::AppTooOld => "This copy of Nexus Music is too old for your Nexus server. Update it. Your music plays as usual.".to_owned(),
        SignInState::ServerTooOld => "Your Nexus server is too old for this copy of Nexus Music. Ask whoever runs it to update it.".to_owned(),
        SignInState::Offline => "Offline mode is on. Turn it off to sign in.".to_owned(),
        SignInState::NoRandom => format!("Nexus Music couldn't make a key for {}. Try again.", this(kind)),
        SignInState::PasswordsDiffer => "The two passwords are not the same.".to_owned(),
        SignInState::PasswordNotAllowed => password_rule(),
        SignInState::SignedOut => format!("You're signed out on {}. Your downloads are still here.", this(kind)),
        SignInState::ForcedOut => format!("{} was signed out of Nexus Music. Sign in again to carry on. Your downloads are still here.", capital(this(kind))),
        // Said both when the server's data was put back and when another server now answers
        // at this address (7.1): either way, signing in again is what carries on.
        SignInState::StartAgain => "Your Nexus server needs you to sign in again. Your music plays as usual.".to_owned(),
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

pub(super) fn password(state: PasswordState, wait_s: u64, offline: bool) -> String {
    match state {
        PasswordState::Idle | PasswordState::Working => String::new(),
        PasswordState::Changed => "Your password is changed. Your other devices are signed out.".to_owned(),
        PasswordState::WrongCurrent => "That isn't your present password.".to_owned(),
        PasswordState::NotAllowed => password_rule(),
        PasswordState::Differ => "The two new passwords are not the same.".to_owned(),
        PasswordState::Busy => busy(wait_s),
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
    Stranger,
    NeedsPermission,
    SignedIn(&'a str),
    Down { since: String },
}

pub(super) fn row(row: Row<'_>) -> String {
    match row {
        Row::SignedOut => "Nexus server: not signed in".to_owned(),
        Row::Offline => "Nexus server: not asked while Offline mode is on. Your music plays as usual.".to_owned(),
        Row::TooOld => "This copy of Nexus Music is too old for your Nexus server. Update it. Your music plays as usual.".to_owned(),
        Row::Connected => "Nexus server: connected".to_owned(),
        Row::NeedsPermission => {
            "Nexus server: on your home network. Allow Nexus Music to find and connect to nearby devices to reach it. Your music plays as usual.".to_owned()
        }
        Row::Stranger => "Nexus server: a different server answers at this address, so nothing is sent to it. Your music plays as usual.".to_owned(),
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
    /// The account has no music login set (9.4).
    NoneSet,
    /// The backend has one and cannot use it just now (9.4).
    UnusableNow,
    /// The music server is on the home network and the phone lacks the permission (6.4).
    NeedsPermission,
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
        Music::UnusableNow => "Your music sign-in can't be used just now. What's downloaded plays meanwhile.".to_owned(),
        Music::NeedsPermission => {
            "Your music server is on your home network. To reach it, allow Nexus Music to find and connect to nearby devices. What's downloaded plays meanwhile."
                .to_owned()
        }
    }
}

/// The car's short line while a login is set aside (6.3, in the design's words).
pub(super) fn car() -> String {
    "Online music is unavailable until your sign-in is renewed".to_owned()
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
