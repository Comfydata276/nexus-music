//! The calls of signing in and of a session, as the door hands them on (7.4, 12.1): each a
//! POST under `/nexus/api/` with a JSON body of its stated largest size (already held to it
//! from the head), read by C7's strict reader, and answered with C7's shapes.
//!
//! The body of a call that carries a password is wiped once it is read, where the buffer is
//! this request's alone.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::routing::post;
use nexus_core::nexus::account::{self, ChangePassword, ForceOut, GetMusicLogin, ListDevices, NotSignedIn, Refusal, SignIn, SignOut};
use nexus_core::nexus::admin::{self, GetMusicServer, RequestGrant, SetMusicLogin, SetMusicServer};
use zeroize::Zeroize;

use super::session::{Accounts, Answer, refused};

/// What every call is handed: the accounts, and the admin's page's own origin.
#[derive(Clone)]
struct Calls {
    accounts: Arc<Accounts>,
    public_origin: Option<Arc<str>>,
}

/// 7.3: a call from a browser (it says it is the page's, or a browser sent an `Origin`) is
/// taken only as the page's own: JSON, marked `X-Nexus-Admin: 1`, from exactly
/// `NEXUS_PUBLIC_ORIGIN` (S10a's check). An app's call carries neither.
fn from_a_browser_not_the_page(calls: &Calls, headers: &HeaderMap) -> bool {
    crate::admin::is_browser(headers) && !crate::admin::browser_origin_allowed(headers, calls.public_origin.as_deref())
}

/// The routes of S8 added to `base` (`hello`'s and the admin's document).
pub fn router(base: Router, accounts: Arc<Accounts>, public_origin: Option<Arc<str>>) -> Router {
    let calls: Router = Router::new()
        .route(account::SIGN_IN, post(sign_in))
        .route(account::SIGN_OUT, post(sign_out))
        .route(account::LIST_DEVICES, post(devices))
        .route(account::FORCE_OUT, post(force_out))
        .route(account::CHANGE_PASSWORD, post(change_password))
        .route(account::MUSIC_LOGIN, post(music_login))
        .route(admin::REQUEST_GRANT, post(admin_grant))
        .route(admin::SET_MUSIC_LOGIN, post(set_music_login))
        .route(admin::SET_MUSIC_SERVER, post(set_music_server))
        .route(admin::GET_MUSIC_SERVER, post(get_music_server))
        .with_state(Calls { accounts, public_origin });
    base.merge(calls)
}

fn respond(answer: Answer) -> Response<Body> {
    let mut response = Response::new(Body::from(answer.body.bytes().to_vec()));
    *response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// Reads a body of C7's shape `T`, then wipes it if it is this request's alone.
fn read<T>(body: Bytes, reader: impl FnOnce(&[u8]) -> Result<T, account::BadMessage>) -> Result<T, Answer> {
    let read = reader(&body).map_err(|_| refused(Refusal::Malformed));
    if let Ok(mut owned) = body.try_into_mut() {
        owned.as_mut().zeroize();
    }
    read
}

/// The session the caller shows, in its `Authorization` header (8.7). None if it shows none,
/// or one not written as a session is.
fn bearer(headers: &HeaderMap) -> Option<account::Session> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    account::Session::from_authorization(value.to_str().ok()?)
}

fn ended() -> Response<Body> {
    respond(refused(Refusal::NotSignedIn(NotSignedIn::SessionEnded)))
}

async fn sign_in(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let page = crate::admin::is_browser(&headers);
    match read(body, account::read::<SignIn>) {
        Ok(request) => respond(calls.accounts.sign_in(request, page).await),
        Err(answer) => respond(answer),
    }
}

async fn sign_out(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let accounts = &calls.accounts;
    match read(body, account::read::<SignOut>) {
        Ok(request) => respond(accounts.sign_out(bearer(&headers), request).await),
        Err(answer) => respond(answer),
    }
}

async fn devices(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let accounts = &calls.accounts;
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ListDevices>) {
        Ok(request) => respond(accounts.devices(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn force_out(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let accounts = &calls.accounts;
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ForceOut>) {
        Ok(request) => respond(accounts.force_out(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn change_password(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let accounts = &calls.accounts;
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ChangePassword>) {
        Ok(request) => respond(accounts.change_password(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn music_login(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if from_a_browser_not_the_page(&calls, &headers) {
        let _ = read(body, |_| Ok(()));
        return respond(refused(Refusal::Malformed));
    }
    let accounts = &calls.accounts;
    let Some(session) = bearer(&headers) else { return ended() };
    let page = crate::admin::is_browser(&headers);
    match read(body, account::read::<GetMusicLogin>) {
        Ok(request) => respond(accounts.music_login(session, request.head, page).await),
        Err(answer) => respond(answer),
    }
}

/// The admin's calls of S9 (9.3, 8.8): a grant for one change; a person's music login set; the
/// address people use set; the two addresses read. Each with a session, from the page or not:
/// the grant is what makes a change allowed, not the page. Refusals are `nexus::admin`'s.
macro_rules! admin_call {
    ($name:ident, $shape:ty, $method:ident) => {
        async fn $name(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
            if from_a_browser_not_the_page(&calls, &headers) {
                let _ = read(body, |_| Ok(()));
                return respond(super::music::refused(admin::Refusal::Malformed));
            }
            let Some(session) = bearer(&headers) else {
                let _ = read(body, |_| Ok(()));
                return respond(super::music::refused(admin::Refusal::NotSignedIn(NotSignedIn::SessionEnded)));
            };
            match read(body, admin::read::<$shape>) {
                Ok(request) => respond(calls.accounts.$method(session, request).await),
                Err(_) => respond(super::music::refused(admin::Refusal::Malformed)),
            }
        }
    };
}

admin_call!(admin_grant, RequestGrant, admin_grant);
admin_call!(set_music_login, SetMusicLogin, set_music_login);
admin_call!(set_music_server, SetMusicServer, set_music_server);
admin_call!(get_music_server, GetMusicServer, get_music_server);
