//! Account listening screens. Stats are kept for the current account and life
//! while the app runs. The sign-in driver owns transport and receipt decisions.

use std::cell::{Cell, RefCell};

use nexus_core::nexus::account::{self, NotAllowed, Password, Reply, Stamp};
use nexus_core::nexus::events::{self, HistoryDeleteError, HistoryDeletion, Kind, StatsPage, StatsRequest};
use nexus_core::nexus::requests::TransportError;
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
    AlreadyDeleted,
    Invalid,
}

impl From<TransportError> for Error {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<HistoryDeleteError> for Error {
    fn from(error: HistoryDeleteError) -> Self {
        match error {
            HistoryDeleteError::MissingRevision => Self::MissingRevision,
            HistoryDeleteError::Invalid => Self::Invalid,
            HistoryDeleteError::Refused(reason) => Self::Refused(reason),
        }
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
            Self::MissingRevision => "This Nexus server didn't provide the account revision needed to delete history.".into(),
            Self::AlreadyDeleted => "Your listening history was already deleted.".into(),
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
    pane.set_can_delete(false);
    let Some(context) = context else {
        let status = if window.global::<ui::App>().get_nexus_signed_in() {
            "Your stats are unavailable right now."
        } else {
            "Sign in to Nexus Music to see your stats."
        };
        return show(window, None, status);
    };
    if context.offline {
        return show(window, cached(context), "You're offline. Turn off Offline mode to refresh your stats.");
    }
    if !context.verified {
        return show(window, cached(context), "Waiting to check your Nexus server…");
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
                Ok((stamp, page)) if stamp.life == now.life => {
                    pane.set_can_delete(page.account_revision.is_some());
                    let stats = project(page);
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

fn read_stats(context: Context) -> Result<(Stamp, StatsPage), Error> {
    let ask = account::encode(&StatsRequest { head: context.head(), after: None }).map_err(|_| Error::Invalid)?;
    let answer = calls::post(context, events::STATS_PATH, ask.bytes())?;
    match events::read_stats_reply(answer.status, &answer.body).map_err(|_| Error::Invalid)? {
        Reply::Done { result, receipt } if receipt.stamp.account == context.account
            && receipt.stamp.backend == context.backend && receipt.stamp.restore == context.restore => {
            Ok((receipt.stamp, result))
        }
        Reply::Refused(why) => Err(Error::Refused(why)),
        _ => Err(Error::Invalid),
    }
}

fn project(page: StatsPage) -> Stats {
    let recent = page.recent.into_iter().filter_map(|play| {
        let record = play.event.record();
        let Kind::Start { title, artist, album, .. } = record.kind() else { return None };
        let when = chrono::DateTime::from_timestamp_millis(record.at_ms() as i64)
            .map(|date| date.with_timezone(&chrono::Local).format("%-d %b %Y, %-I:%M %p").to_string()).unwrap_or_default();
        Some(Play { title: events::clean_display_text(title), artist: events::clean_display_text(artist),
            album: events::clean_display_text(album), when })
    }).collect();
    Stats { starts: page.starts, recent }
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
        crate::controller::post(move |ctl| {
            if !current_turn(turn) {
                return;
            }
            let pane = ctl.ui.global::<ui::Listening>();
            pane.set_busy(false);
            let now = calls::current().filter(|now| now.same_session(context));
            if let (Ok(life), Some(now)) = (&result, now)
                && now.life == *life
            {
                let empty = Stats::default();
                save_stats(now, empty.clone());
                pane.set_can_delete(!now.offline && now.verified);
                pane.set_page("stats".into());
                pane.set_result("Your listening history was deleted.".into());
                show(&ctl.ui, Some(empty), "No recent plays yet.");
            } else if now.is_none() || result.is_ok() {
                pane.set_page("stats".into());
                refresh(&ctl.ui);
            } else if let Err(error) = result {
                pane.set_result(error.words().into());
            }
        });
    });
}

fn check_delete_life(started: u64, current: u64) -> Result<(), Error> {
    match current.cmp(&started) {
        std::cmp::Ordering::Greater => Err(Error::AlreadyDeleted),
        std::cmp::Ordering::Less => Err(Error::Transport(TransportError::Stopped)),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

fn delete(context: Context, password: Password) -> Result<u64, Error> {
    let (stamp, latest) = read_stats(context)?;
    check_delete_life(context.life, stamp.life)?;
    let deletion = HistoryDeletion::new(context.head(), &stamp, &latest, calls::new_id()?)?;
    let grant_request = deletion.grant(password)?;
    let grant = calls::post(context, account::GRANT, grant_request.bytes())?;
    drop(grant_request);
    let body = zeroize::Zeroizing::new(grant.body);
    let command = deletion.spend(grant.status, &body)?;
    drop(body);
    let answer = calls::post(context, account::DELETE_LISTENING_DATA, command.bytes())?;
    Ok(deletion.confirmed_life(answer.status, &answer.body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::nexus::proto::BackendId;

    fn context() -> Context {
        Context {
            generation: 1,
            account: account::AccountId::new(1).unwrap(),
            session: account::SessionId::new(2).unwrap(),
            backend: BackendId::from_bytes([7; 16]),
            restore: 0,
            life: 5,
            offline: false,
            verified: true,
        }
    }

    #[test]
    fn retry_after_a_lost_delete_reply_reports_the_advanced_life() {
        let started = context().life;
        assert!(check_delete_life(started, started).is_ok());
        assert!(matches!(check_delete_life(started, started - 1), Err(Error::Transport(TransportError::Stopped))));
        let error = check_delete_life(started, started + 1).unwrap_err();
        assert_eq!(error.words(), "Your listening history was already deleted.");
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
