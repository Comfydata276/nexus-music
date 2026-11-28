//! Account listening screens. Stats are kept for the current account and life
//! while the app runs. The sign-in driver owns transport and receipt decisions.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use nexus_core::nexus::account::{self, DeleteListeningData, DeleteListeningDataTarget, GrantChange, GrantIssued, GrantRequest, NotAllowed, Password, Reply, SignedOut};
use nexus_core::nexus::requests::{TransportError, TransportReply};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app_nexus_calls::{self as calls, Context};
use crate::ui;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Play {
    title: String,
    artist: String,
    album: String,
    when: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Stats {
    starts: u64,
    recent: Vec<Play>,
}

#[derive(Debug)]
enum Error {
    Transport(TransportError),
    Refused(account::Refusal),
    MissingRevision,
    Invalid,
}

impl From<TransportError> for Error {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl Error {
    fn words(&self) -> String {
        match self {
            Self::Refused(account::Refusal::NotAllowed(NotAllowed::WrongLogin)) => "That password didn't match. Try again.".into(),
            Self::Refused(account::Refusal::Busy { retry_after_seconds: Some(seconds) }) => {
                format!("Try again in {} seconds.", seconds.seconds())
            }
            Self::Refused(account::Refusal::NotSignedIn(_)) | Self::Transport(TransportError::Stopped) => {
                "Sign in again or turn off Offline mode to continue.".into()
            }
            Self::Transport(TransportError::Unreachable) => "Couldn't reach your Nexus server.".into(),
            Self::MissingRevision => "Couldn't check your account. Refresh Your stats and try again.".into(),
            Self::Invalid | Self::Refused(_) => "Your Nexus server couldn't complete this action. Try again.".into(),
        }
    }
}

thread_local! {
    static CACHE: RefCell<Option<(Context, Stats)>> = const { RefCell::new(None) };
    static TURN: Cell<u64> = const { Cell::new(0) };
}

fn cached(context: Context) -> Option<Stats> {
    CACHE.with_borrow(|cache| cache.as_ref()
        .filter(|(owner, _)| owner.same_session(context) && owner.life == context.life)
        .map(|(_, stats)| stats.clone()))
}

fn save_stats(context: Context, stats: Stats) {
    CACHE.with_borrow_mut(|cache| *cache = Some((context, stats)));
}

fn new_turn() -> u64 {
    TURN.with(|turn| {
        let next = turn.get().wrapping_add(1);
        turn.set(next);
        next
    })
}

fn current_turn(turn: u64) -> bool {
    TURN.with(|latest| latest.get() == turn)
}

pub(crate) fn install(window: &ui::MainWindow) {
    let pane = window.global::<ui::Listening>();
    pane.on_show_stats(|| crate::controller::with(|ctl| {
        let pane = ctl.ui.global::<ui::Listening>();
        pane.set_page("stats".into());
        pane.set_result("".into());
        refresh(&ctl.ui);
    }).unwrap_or(()));
    pane.on_show_recording(|| crate::controller::with(|ctl| {
        let pane = ctl.ui.global::<ui::Listening>();
        pane.set_page("recording".into());
    }).unwrap_or(()));
    pane.on_dismiss(|| crate::controller::with(|ctl| {
        let pane = ctl.ui.global::<ui::Listening>();
        if pane.get_initial_notice() {
            pane.set_initial_notice(false);
            calls::notice_seen();
        }
        new_turn();
        pane.set_page("".into());
        pane.set_busy(false);
        pane.set_result("".into());
    }).unwrap_or(()));
    pane.on_refresh(|| crate::controller::with(|ctl| refresh(&ctl.ui)).unwrap_or(()));
    pane.on_delete(move |password| crate::controller::with(|ctl| {
        if let Some(password) = Password::presented(&password) {
            start_delete(&ctl.ui, password);
        } else {
            ctl.ui.global::<ui::Listening>().set_result("Enter your current Nexus password.".into());
        }
    }).unwrap_or(()));
}

/// Invoked by W7 once the machine exposes its pending first-sign-in notice.
pub(crate) fn show_recording_notice(window: &ui::MainWindow) {
    let pane = window.global::<ui::Listening>();
    pane.set_initial_notice(true);
    pane.set_page("recording".into());
}

pub(crate) fn session_changed() {
    crate::controller::post(|ctl| {
        let context = calls::current();
        CACHE.with_borrow_mut(|cache| {
            if cache.as_ref().is_some_and(|(owner, _)| !context.is_some_and(|now| now.same_session(*owner) && now.life == owner.life)) {
                *cache = None;
            }
        });
        let pane = ctl.ui.global::<ui::Listening>();
        if pane.get_page() == "stats" {
            refresh(&ctl.ui);
        } else if pane.get_page() == "delete" && !pane.get_busy() {
            pane.set_page("stats".into());
            refresh(&ctl.ui);
        } else if context.is_none() && pane.get_initial_notice() {
            pane.set_page("".into());
            pane.set_initial_notice(false);
        }
    });
}

fn show(window: &ui::MainWindow, stats: Option<Stats>, status: &str) {
    let pane = window.global::<ui::Listening>();
    pane.set_status(status.into());
    pane.set_summary(stats.as_ref().map_or_else(String::new, |stats| {
        match stats.starts {
            1 => "1 song start across your devices".into(),
            count => format!("{count} song starts across your devices"),
        }
    }).into());
    let recent = stats.map_or_else(Vec::new, |stats| stats.recent);
    let rows = recent.into_iter().map(|row| ui::RecentListen {
        title: row.title.into(), artist: row.artist.into(), album: row.album.into(), when: row.when.into(),
    }).collect::<Vec<_>>();
    pane.set_recent(ModelRc::new(VecModel::from(rows)));
}

fn refresh(window: &ui::MainWindow) {
    let pane = window.global::<ui::Listening>();
    let turn = new_turn();
    let context = calls::current();
    pane.set_busy(false);
    pane.set_can_delete(context.is_some_and(|context| !context.offline && context.target_revision.is_some()));
    let Some(context) = context else {
        return show(window, None, "Sign in to Nexus Music to see your stats.");
    };
    if context.offline {
        return show(window, cached(context), "You're offline. Turn off Offline mode to refresh your stats.");
    }
    pane.set_busy(true);
    show(window, cached(context), "Getting your stats…");
    std::thread::spawn(move || {
        let result = read_stats(context);
        crate::controller::post(move |ctl| {
            if !current_turn(turn) {
                return;
            }
            let pane = ctl.ui.global::<ui::Listening>();
            pane.set_busy(false);
            let Some(now) = calls::current().filter(|now| !now.offline && now.same_session(context)) else {
                show(&ctl.ui, None, "Your account or Offline mode changed. Refresh to see your stats.");
                return;
            };
            match result {
                Ok((life, stats)) if life == now.life => {
                    let status = if stats.recent.is_empty() { "No recent plays yet." } else { "" };
                    save_stats(now, stats.clone());
                    show(&ctl.ui, Some(stats), status);
                }
                Ok(_) => show(&ctl.ui, cached(now), "Your listening history changed. Refresh to see the latest counts."),
                Err(error) => show(&ctl.ui, cached(now), &error.words()),
            }
        });
    });
}

/// C8 owns the strict decoder for the server's stats result. Until it lands,
/// the screen reports that the read is unavailable.
fn read_stats(_context: Context) -> Result<(u64, Stats), Error> {
    Err(Error::Invalid)
}

fn start_delete(window: &ui::MainWindow, password: Password) {
    let pane = window.global::<ui::Listening>();
    if pane.get_busy() || pane.get_page() != "delete" {
        return;
    }
    let Some(context) = calls::current().filter(|context| !context.offline) else {
        pane.set_result("Turn off Offline mode and sign in to delete your history.".into());
        return;
    };
    let turn = new_turn();
    pane.set_result("".into());
    pane.set_busy(true);
    std::thread::spawn(move || {
        let result = delete(context, password);
        let ambiguous = matches!(result, Err(Error::Transport(TransportError::Unreachable) | Error::Refused(account::Refusal::NotAllowed(NotAllowed::Denied))));
        let confirmed = result.is_ok() || (ambiguous && read_stats(context).is_ok_and(|(life, _)| life > context.life));
        crate::controller::post(move |ctl| {
            if !current_turn(turn) {
                return;
            }
            let pane = ctl.ui.global::<ui::Listening>();
            pane.set_busy(false);
            let now = calls::current().filter(|now| now.same_session(context));
            if confirmed && let Some(now) = now {
                let empty = Stats::default();
                save_stats(now, empty.clone());
                pane.set_page("stats".into());
                pane.set_result("Your listening history was deleted.".into());
                show(&ctl.ui, Some(empty), "No recent plays yet.");
            } else if now.is_none() {
                pane.set_page("stats".into());
                refresh(&ctl.ui);
            } else if let Err(error) = result {
                pane.set_result(error.words().into());
            }
        });
    });
}

fn delete(context: Context, password: Password) -> Result<(), Error> {
    delete_using(context, password, |path, body| {
        if path == account::DELETE_LISTENING_DATA && !calls::current().is_some_and(|now| now.life == context.life) {
            return Err(TransportError::Stopped);
        }
        calls::post(context, path, body, account::SMALL_REPLY_MAX_BYTES, Duration::from_secs(12))
    })
}

fn delete_using(
    context: Context,
    password: Password,
    mut post: impl FnMut(&str, &[u8]) -> Result<TransportReply, TransportError>,
) -> Result<(), Error> {
    let target_revision = context.target_revision.ok_or(Error::MissingRevision)?;
    let request_id = calls::new_id()?;
    let grant = account::encode(&GrantRequest {
        head: context.head(),
        change: GrantChange::DeleteListeningData { account: context.account, target_revision, request_id },
        password,
    }).map_err(|_| Error::Invalid)?;
    let answer = post(account::GRANT, grant.bytes())?;
    drop(grant);
    let grant_body = zeroize::Zeroizing::new(answer.body);
    let grant = match account::read_reply::<GrantIssued>(answer.status, &grant_body).map_err(|_| Error::Invalid)? {
        Reply::Done { result, receipt } if receipt.stamp.account == context.account && receipt.stamp.backend == context.backend
            && receipt.stamp.restore == context.restore && receipt.stamp.life == context.life => result.grant,
        Reply::Refused(why) => return Err(Error::Refused(why)),
        _ => return Err(Error::Invalid),
    };
    let command = account::encode(&DeleteListeningData {
        head: context.head(),
        request_id,
        grant,
        change: DeleteListeningDataTarget { account: context.account, target_revision },
    }).map_err(|_| Error::Invalid)?;
    let answer = post(account::DELETE_LISTENING_DATA, command.bytes())?;
    match account::read_reply::<SignedOut>(answer.status, &answer.body).map_err(|_| Error::Invalid)? {
        Reply::Done { receipt, .. } if receipt.stamp.account == context.account && receipt.stamp.backend == context.backend
            && receipt.stamp.restore == context.restore && receipt.stamp.life > context.life => Ok(()),
        Reply::Refused(why) => Err(Error::Refused(why)),
        _ => Err(Error::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::nexus::{account::{GrantSecret, Receipt, Stamp, StampTag}, proto::{BackendId, Speaks}};

    fn context() -> Context {
        Context {
            generation: 1,
            account: account::AccountId::new(1).unwrap(),
            session: account::SessionId::new(2).unwrap(),
            backend: BackendId::from_bytes([7; 16]),
            speaks: Speaks::new(1, 1).unwrap(),
            restore: 0,
            life: 5,
            target_revision: Some(9),
            offline: false,
        }
    }

    fn receipt(context: Context, life: u64) -> Receipt {
        Receipt {
            stamp: Stamp {
                backend: context.backend, account: context.account, restore: context.restore,
                life, change: 17, tag: StampTag::from_bytes([8; 32]),
            },
            music_login_count: 1,
        }
    }

    #[test]
    fn deletion_uses_a_new_grant_and_never_spends_it_for_a_wrong_password() {
        let session = context();
        let mut wrong_calls = 0;
        let error = delete_using(session, Password::presented("wrong").unwrap(), |path, body| {
            wrong_calls += 1;
            assert_eq!(path, account::GRANT);
            assert!(account::read::<GrantRequest>(body).is_ok());
            let reply = Reply::<GrantIssued>::Refused(account::Refusal::NotAllowed(NotAllowed::WrongLogin));
            Ok(TransportReply { status: reply.status(), body: account::encode(&reply).unwrap().bytes().to_vec() })
        }).unwrap_err();
        assert_eq!(error.words(), "That password didn't match. Try again.");
        assert_eq!(wrong_calls, 1);

        let mut grant_request = None;
        delete_using(session, Password::presented("the correct fixture password").unwrap(), |path, body| {
            let (status, body) = match path {
                account::GRANT => {
                    let request = account::read::<GrantRequest>(body).unwrap();
                    let GrantChange::DeleteListeningData { request_id, account, target_revision } = request.change;
                    assert_eq!(account, session.account);
                    assert_eq!(Some(target_revision), session.target_revision);
                    grant_request = Some(request_id);
                    let reply = Reply::Done { result: GrantIssued { grant: GrantSecret::from_bytes([1; 32]) }, receipt: receipt(session, session.life) };
                    (reply.status(), account::encode(&reply).unwrap())
                }
                account::DELETE_LISTENING_DATA => {
                    let request = account::read::<DeleteListeningData>(body).unwrap();
                    assert_eq!(Some(request.request_id), grant_request.take());
                    assert_eq!(request.change.account, session.account);
                    assert_eq!(Some(request.change.target_revision), session.target_revision);
                    let reply = Reply::Done { result: SignedOut {}, receipt: receipt(session, session.life + 1) };
                    (reply.status(), account::encode(&reply).unwrap())
                }
                other => panic!("unexpected call {other}"),
            };
            Ok(TransportReply { status, body: body.bytes().to_vec() })
        }).unwrap();
        assert!(grant_request.is_none());
    }

    #[test]
    fn cached_plays_match_account_session_restore_and_life_even_when_offline() {
        let session = context();
        assert!(cached(session).is_none());
        let stats = Stats {
            starts: 1,
            recent: vec![Play { title: "Fixture song".into(), artist: "Artist".into(), album: "Album".into(), when: "Today".into() }],
        };
        save_stats(session, stats.clone());
        assert_eq!(cached(Context { offline: true, ..session }), Some(stats));
        assert!(cached(Context { life: session.life + 1, ..session }).is_none());
        assert!(cached(Context { restore: session.restore + 1, ..session }).is_none());
        assert!(cached(Context { generation: session.generation + 1, ..session }).is_none());
        save_stats(session, Stats::default());
        assert_eq!(cached(session), Some(Stats::default()));
    }
}
