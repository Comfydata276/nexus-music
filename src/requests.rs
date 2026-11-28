//! Choose the Requests connection once for each action. Only the Nexus server's
//! status answer can make the released direct connection available.

use std::sync::{Arc, Mutex};

use nexus_core::lidarr::{Offers, RequestLidarr};
use nexus_core::nexus::requests::{Availability, BackendLidarr, LidarrNumber, MusicBrainzId, PendingView, Refusal, RequestError, TransportError};

use crate::app_nexus_calls::{self as calls, Context};

pub(crate) mod batch;
pub(crate) mod fill;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Checking,
    Direct,
    Server,
    Unavailable(String),
}

fn choose(status: Result<Availability, RequestError>) -> Route {
    match status {
        Ok(Availability::Ready | Availability::Unavailable) => Route::Server,
        Ok(Availability::Unconfigured) | Err(RequestError::Refused(Refusal::LidarrNotSetUp)) => Route::Direct,
        Err(error) => Route::Unavailable(error.to_string()),
    }
}

#[derive(Clone)]
struct Selection {
    session: Context,
    route: Route,
    number: u64,
    offers: Arc<Offers>,
}

static SELECTED: Mutex<(Option<Selection>, u64)> = Mutex::new((None, 0));

#[derive(Clone)]
pub(crate) enum Connection {
    Direct { lidarr: crate::lidarr::Lidarr, number: u64 },
    Server { context: Context, client: Arc<BackendLidarr<'static>>, number: u64, offers: Arc<Offers> },
}

impl Connection {
    pub(crate) fn lidarr(&self) -> &dyn RequestLidarr {
        match self {
            Self::Direct { lidarr, .. } => lidarr,
            Self::Server { client, .. } => client.as_ref(),
        }
    }

    pub(crate) fn offers(&self) -> &Offers {
        match self {
            Self::Direct { .. } => crate::lidarr::offers(),
            Self::Server { offers, .. } => offers,
        }
    }

    pub(crate) fn server(&self) -> bool {
        matches!(self, Self::Server { .. })
    }

    /// History is read from the server for this operation. The library can
    /// identify arrivals using the same title-and-artist match as direct mode.
    pub(crate) fn asked(&self) -> Vec<crate::asked::Asked> {
        let Self::Server { client, .. } = self else { return crate::asked::shared() };
        client.asked_history().map(project_history).unwrap_or_default()
    }

    /// Ignore old worker results after the selected session or route changes.
    pub(crate) fn still_selected(&self) -> bool {
        match self {
            Self::Direct { number, .. } => direct_selected() && crate::lidarr::for_connection(*number).is_some(),
            Self::Server { context, number, .. } => selected()
                .is_some_and(|selected| selected.number == *number && selected.session.same_session(*context) && selected.route == Route::Server),
        }
    }
}

pub(crate) fn switched_off(words: &str) -> bool {
    words == Refusal::SwitchedOff.sentence(None)
}

pub(crate) fn removal_id(row: &PendingView) -> i64 {
    if row.removable { row.id.get() } else { 0 }
}

pub(crate) fn cancel(client: &BackendLidarr<'_>, id: i64, album: &str) -> Result<(), String> {
    let (Some(id), Some(release)) = (LidarrNumber::new(id), MusicBrainzId::new(album)) else {
        return Err(Refusal::NoLongerListed.sentence(None).into_owned());
    };
    client.cancel_release(id, release).map_err(|error| error.to_string())
}

pub(crate) fn backend(context: Context) -> BackendLidarr<'static> {
    BackendLidarr::new(
        context.head(),
        move |path, body, _| calls::post(context, path, body),
        move |receipt| {
            // The driver has already processed the typed receipt. Keep the
            // operation's account fence even between successful pages.
            if receipt.stamp.backend != context.backend || receipt.stamp.account != context.account {
                return Err(RequestError::UnexpectedAnswer);
            }
            calls::active(context).then_some(()).ok_or(TransportError::Stopped.into())
        },
        |wait, _| {
            std::thread::sleep(wait);
            Ok(())
        },
        calls::new_id,
    )
}

pub(crate) fn project_history(rows: Vec<nexus_core::nexus::requests::AskedRelease>) -> Vec<crate::asked::Asked> {
    let index = crate::search::current();
    project_history_with(rows, |title, artist| {
        index.as_ref().and_then(|index| nexus_core::requests::Library::same_album(index.as_ref(), title, artist))
    })
}

fn project_history_with(
    rows: Vec<nexus_core::nexus::requests::AskedRelease>,
    album: impl Fn(&str, &str) -> Option<(String, String)>,
) -> Vec<crate::asked::Asked> {
    rows.into_iter().map(|row| {
        let arrived = album(&row.title, &row.artist);
        let at = i64::try_from(row.asked_ms / 1_000).unwrap_or(i64::MAX);
        let (arrived, library_album, cover) = arrived.map_or((0, String::new(), row.cover), |(id, cover)| (at.max(1), id, cover));
        crate::asked::Asked { album: row.release.into(), title: row.title, artist: row.artist, at, arrived, library_album, cover, ..Default::default() }
    }).collect()
}

fn selected() -> Option<Selection> {
    let session = calls::current();
    let state = SELECTED.lock().ok()?;
    state.0.as_ref().filter(|choice| usable(choice, session)).cloned()
}

fn usable(choice: &Selection, session: Option<Context>) -> bool {
    session.is_some_and(|session| !session.offline && session.verified && choice.session.same_session(session))
}

pub(crate) fn direct_selected() -> bool {
    // The recorded released request-order tests install a direct connection
    // without a sign-in driver and exercise the same direct code as before.
    #[cfg(test)]
    if crate::pins::entered() {
        return true;
    }
    selected().is_some_and(|choice| choice.route == Route::Direct)
}

pub(crate) fn current() -> Option<Connection> {
    if direct_selected() {
        let (lidarr, number) = crate::lidarr::current_numbered();
        return lidarr.map(|lidarr| Connection::Direct { lidarr, number });
    }
    let selected = selected().filter(|choice| choice.route == Route::Server)?;
    let context = selected.session;
    Some(Connection::Server { context, client: Arc::new(backend(context)), number: selected.number, offers: selected.offers })
}

pub(crate) fn status_text() -> String {
    if direct_selected() {
        return "Requests are switched off. Turn them on in Settings.".into();
    }
    match (calls::current(), selected().map(|choice| choice.route)) {
        (Some(session), _) if session.offline => "Turn off Offline mode to see Requests.".into(),
        (_, Some(Route::Unavailable(words))) => words,
        (_, Some(Route::Direct)) => "Requests are switched off. Turn them on in Settings.".into(),
        (Some(_), _) => "Checking whether Requests are set up on your Nexus server…".into(),
        (None, _) => "Sign in to Nexus Music to see Requests.".into(),
    }
}

pub(crate) fn settings_text() -> String {
    if direct_selected() {
        String::new()
    } else if matches!(selected().map(|choice| choice.route), Some(Route::Server)) {
        "Your Nexus server handles requests for music.".into()
    } else {
        status_text()
    }
}

/// Call at account changes and when the Requests page is opened. The pin on the
/// queued probe rules out an older answer after a second open or account switch.
pub(crate) fn refresh() {
    let Some(session) = calls::current().filter(|session| !session.offline && session.verified) else {
        if let Ok(mut state) = SELECTED.lock() {
            state.0 = None;
        }
        crate::controller::post(|controller| controller.requests_source_changed());
        return;
    };
    let probe = {
        let Ok(mut state) = SELECTED.lock() else { return };
        state.1 = state.1.wrapping_add(1);
        if !state.0.as_ref().is_some_and(|choice| choice.session.same_session(session)) {
            state.0 = Some(Selection { session, route: Route::Checking, number: state.1, offers: Arc::new(Offers::new()) });
        }
        state.1
    };
    std::thread::spawn(move || {
        let route = choose(backend(session).status());
        if !calls::active(session) {
            return;
        }
        let Ok(mut state) = SELECTED.lock() else { return };
        if state.1 != probe {
            return;
        }
        let (number, offers) = state.0.as_ref().filter(|choice| choice.route == route)
            .map_or_else(|| (probe, Arc::new(Offers::new())), |choice| (choice.number, choice.offers.clone()));
        state.0 = Some(Selection { session, route, number, offers });
        drop(state);
        crate::controller::post(|controller| controller.requests_source_changed());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unconfigured_status_chooses_the_released_connection() {
        assert_eq!(choose(Ok(Availability::Ready)), Route::Server);
        assert_eq!(choose(Ok(Availability::Unconfigured)), Route::Direct);
        assert_eq!(choose(Err(RequestError::Refused(Refusal::LidarrNotSetUp))), Route::Direct);
        assert_eq!(choose(Ok(Availability::Unavailable)), Route::Server);
        for reason in [Refusal::SwitchedOff, Refusal::NotSignedIn, Refusal::Busy, Refusal::LidarrKeyRefused] {
            assert_eq!(choose(Err(RequestError::Refused(reason))), Route::Unavailable(reason.sentence(None).into_owned()));
        }
        assert!(matches!(choose(Err(TransportError::Unreachable.into())), Route::Unavailable(_)));
    }

    #[test]
    fn signed_out_offline_and_a_replaced_session_cannot_reuse_a_route() {
        use nexus_core::nexus::{account::{AccountId, SessionId}, proto::BackendId};
        let session = Context {
            generation: 3, backend: BackendId::from_bytes([1; 16]), account: AccountId::new(1).unwrap(),
            session: SessionId::new(8).unwrap(),
            restore: 0, life: 8, offline: false, verified: true,
        };
        let choice = Selection { session, route: Route::Direct, number: 5, offers: Arc::new(Offers::new()) };
        assert!(usable(&choice, Some(session)));
        assert!(!usable(&choice, None));
        assert!(!usable(&choice, Some(Context { offline: true, ..session })));
        assert!(!usable(&choice, Some(Context { restore: 1, ..session })));
        assert!(!usable(&choice, Some(Context { session: SessionId::new(9).unwrap(), ..session })));
    }

    #[test]
    fn every_server_refusal_uses_the_shared_sentence() {
        use Refusal::*;
        let reasons = [
            SwitchedOff, SearchAgain, Busy, NotAllowed, AskedByOthers, NotSignedIn, StartAgain, Malformed, Incompatible, TooLarge,
            LidarrUnavailable, LidarrRefused, NotAmongReleases, WouldNotKeep, NoLongerListed, LidarrNotSetUp, LidarrNoProfiles,
            LidarrSinglesProfileNoNumber, LidarrSoundtracksProfileNoNumber, LidarrBehindSignIn, LidarrSignInRejected,
            LidarrKeyRefused, LidarrUnrecognized, LidarrCouldNotRestore, JobNotFound, NotDealtWith,
        ];
        for reason in reasons {
            assert_eq!(RequestError::Refused(reason).to_string(), reason.sentence(None));
        }
    }


    #[test]
    fn removal_action_depends_on_the_server_row() {
        let mut row = PendingView {
            id: LidarrNumber::new(14).unwrap(),
            album: MusicBrainzId::new("cea5d1a3-baac-4978-9ff7-c9ce27576cd5").unwrap(),
            title: "Title".into(), artist: "Artist".into(), removable: false, progress: None,
        };
        assert_eq!(removal_id(&row), 0);
        row.removable = true;
        assert_eq!(removal_id(&row), 14);
    }


    #[test]
    fn server_history_includes_library_arrivals_and_other_asked_releases() {
        use nexus_core::nexus::requests::AskedRelease;
        let first = MusicBrainzId::new("cea5d1a3-baac-4978-9ff7-c9ce27576cd5").unwrap();
        let second = MusicBrainzId::new("627fca7b-af0e-4471-9932-ee520b5e0f98").unwrap();
        let history = project_history_with(vec![
            AskedRelease { release: first.clone(), artist_id: second.clone(), title: "Arrived album".into(), artist: "Artist".into(), cover: "".into(), asked_ms: 5000 },
            AskedRelease { release: second.clone(), artist_id: first, title: "Still on the way".into(), artist: "Artist".into(), cover: "".into(), asked_ms: 12000 },
        ], |title, artist| {
            (title == "Arrived album" && artist == "Artist").then(|| ("my-library-album".into(), "my-cover".into()))
        });
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].library_album, "my-library-album");
        assert_ne!(history[0].arrived, 0);
        assert_eq!(history[1].album, second.as_str());
        assert_eq!(history[1].arrived, 0);
    }

}
