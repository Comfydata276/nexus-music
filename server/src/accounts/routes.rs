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
use zeroize::Zeroize;

use super::session::{Accounts, Answer, refused};

/// The routes of S8 added to `base` (`hello`'s).
pub fn router(base: Router, accounts: Arc<Accounts>) -> Router {
    let calls: Router = Router::new()
        .route(account::SIGN_IN, post(sign_in))
        .route(account::SIGN_OUT, post(sign_out))
        .route(account::LIST_DEVICES, post(devices))
        .route(account::FORCE_OUT, post(force_out))
        .route(account::CHANGE_PASSWORD, post(change_password))
        .route(account::MUSIC_LOGIN, post(music_login))
        .with_state(accounts);
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

async fn sign_in(State(accounts): State<Arc<Accounts>>, body: Bytes) -> Response<Body> {
    match read(body, account::read::<SignIn>) {
        Ok(request) => respond(accounts.sign_in(request).await),
        Err(answer) => respond(answer),
    }
}

async fn sign_out(State(accounts): State<Arc<Accounts>>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    match read(body, account::read::<SignOut>) {
        Ok(request) => respond(accounts.sign_out(bearer(&headers), request).await),
        Err(answer) => respond(answer),
    }
}

async fn devices(State(accounts): State<Arc<Accounts>>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ListDevices>) {
        Ok(request) => respond(accounts.devices(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn force_out(State(accounts): State<Arc<Accounts>>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ForceOut>) {
        Ok(request) => respond(accounts.force_out(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn change_password(State(accounts): State<Arc<Accounts>>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<ChangePassword>) {
        Ok(request) => respond(accounts.change_password(session, request).await),
        Err(answer) => respond(answer),
    }
}

async fn music_login(State(accounts): State<Arc<Accounts>>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let Some(session) = bearer(&headers) else { return ended() };
    match read(body, account::read::<GetMusicLogin>) {
        Ok(request) => respond(accounts.music_login(session, request.head).await),
        Err(answer) => respond(answer),
    }
}
