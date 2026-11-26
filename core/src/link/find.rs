//! Asking the music server for songs a command brought that this device's own copy of the
//! library lacks ([`super::device::Do::Find`]).
//!
//! One request for each song (the server has no call for several), by form POST with the
//! sign-in in the form, as every request of a look is.
//!
//! **The whole look-up has what the command has, and no more.** A command is waited on for
//! half a minute **from when it arrived**; after that it is given up, and whatever is
//! found for it is nobody's. So each request is given what is then left of that time as
//! its own limit, **the last one too**: a request to a server that has stopped answering
//! ends when the command does, not when the app's own patience with the server runs out,
//! and no worker is held past it. When no time is left, no further request is made. How
//! long is left is asked of the app before each request ([`find`]'s `left`): the app
//! counts it from the moment the command ends ([`super::device::Do::Find`]'s `until_ms`),
//! so the time a look-up spends waiting to begin is part of it.

use std::time::Duration;

use super::command::id_ok;
use super::look::{self, Account, Not};
use crate::http::Http;
use crate::model::Song;

/// The songs of `ids` that the music server has, in the order asked. `left` says how
/// long the command is still waited on, asked before each request. A song the server
/// does not have, or does not answer for in time, is left out; if the sign-in has ended
/// (the app's `Http` says so), nothing more is asked.
pub fn find(http: &dyn Http, account: &Account, ids: &[String], left: &mut dyn FnMut() -> Duration) -> Vec<Song> {
    let mut found = Vec::new();
    for id in ids.iter().filter(|id| id_ok(id)) {
        let left = left();
        if left.is_zero() {
            break;
        }
        match look::call(http, account, "getSong", &[("id", id)], false, Some(left)) {
            Ok(text) => found.extend(song(&text).filter(|song| song.id == *id)),
            Err(Not::Wanted) => break,
            Err(Not::Asked) => {}
        }
    }
    found
}

/// The song in an answer to "this song", if the server says all is well and it is one.
fn song(text: &str) -> Option<Song> {
    let mut root: serde_json::Value = serde_json::from_str(text).ok()?;
    let body = root.get_mut("subsonic-response")?;
    if body["status"].as_str() != Some("ok") {
        return None;
    }
    serde_json::from_value(body.get_mut("song")?.take()).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::{Failure, Method, Request, Response};

    /// A music server that has some songs, takes a while over each request, and notes how
    /// long each request was given.
    struct Server {
        has: Vec<&'static str>,
        /// How long each request takes, in milliseconds: the clock moves on by it, or by
        /// the request's own limit if that is shorter, when the request gets no answer.
        takes_ms: u64,
        clock_ms: Mutex<u64>,
        given: Mutex<Vec<(String, Option<Duration>)>>,
        signed_out_after: Option<usize>,
        /// Whatever song it is asked for, it answers with this one.
        answers_with: Option<&'static str>,
    }

    impl Server {
        fn with(has: &[&'static str], takes_ms: u64) -> Server {
            Server { has: has.to_vec(), takes_ms, clock_ms: Mutex::new(0), given: Mutex::default(), signed_out_after: None, answers_with: None }
        }

        fn spent(&self) -> Duration {
            Duration::from_millis(*self.clock_ms.lock().unwrap())
        }
    }

    impl Http for Server {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let Method::Post(form) = request.method else { panic!("a look-up asks by POST and no other way") };
            let mut given = self.given.lock().unwrap();
            if self.signed_out_after.is_some_and(|requests| given.len() >= requests) {
                return Err(Failure::Stopped);
            }
            assert!(request.url.ends_with("/rest/getSong") && form.starts_with("u=tester&") && form.contains("&f=json&id="), "{} {form}", request.url);
            let id = form.rsplit("&id=").next().unwrap_or_default().to_string();
            given.push((id.clone(), request.patience));
            let limit = request.patience.map_or(u64::MAX, |limit| limit.as_millis() as u64);
            *self.clock_ms.lock().unwrap() += self.takes_ms.min(limit);
            if self.takes_ms > limit {
                return Err(Failure::Reach("timed out".into()));
            }
            let text = match self.has.contains(&id.as_str()) {
                true => serde_json::json!({"subsonic-response": {"status": "ok", "song": {"id": self.answers_with.unwrap_or(&id), "title": format!("Song {id}"), "duration": 200}}}),
                false => serde_json::json!({"subsonic-response": {"status": "failed", "error": {"code": 70, "message": "Song not found"}}}),
            }
            .to_string();
            Ok(Response::new(200, Vec::new(), move || Ok(text)))
        }

        fn pause(&self, _: Duration) -> Result<(), Failure> {
            Ok(())
        }
    }

    fn account() -> Account {
        Account { rest: "http://music.example/rest".into(), form: "u=tester&t=the-token&s=the-salt&v=1.16.1&c=test".into(), username: "tester".into() }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn found(server: &Server, asked: &[&str], within_ms: u64) -> Vec<String> {
        find(server, &account(), &ids(asked), &mut || Duration::from_millis(within_ms).saturating_sub(server.spent())).into_iter().map(|song| song.id).collect()
    }

    #[test]
    fn the_songs_the_server_has_are_found_in_the_order_asked() {
        let server = Server::with(&["a", "c"], 100);
        assert_eq!(found(&server, &["a", "b", "c"], 30_000), ["a", "c"]);
        // An id no song could have (a radio station's, something that is not an id) is not asked about.
        assert_eq!(found(&server, &["", "c", "not an id"], 30_000), ["c"]);
        assert_eq!(server.given.lock().unwrap().iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "c"]);
    }

    /// Each request is given what is left of the command's time, the last one too: none
    /// can outlive the command it is for.
    #[test]
    fn every_request_is_given_what_is_left_of_the_commands_time_and_none_is_made_after_it() {
        let server = Server::with(&["a", "b", "c", "d"], 9_000);
        assert_eq!(found(&server, &["a", "b", "c", "d", "a"], 30_000), ["a", "b", "c"]);
        let given: Vec<Option<u64>> = server.given.lock().unwrap().iter().map(|(_, limit)| limit.map(|limit| limit.as_millis() as u64)).collect();
        // (The fourth is given the three seconds that are left, and gets no answer in them;
        // the fifth is not asked at all.)
        assert_eq!(given, [Some(30_000), Some(21_000), Some(12_000), Some(3_000)]);
        assert_eq!(server.spent(), Duration::from_millis(30_000));
        // A server that has stopped answering: one request, which ends when the command does.
        let server = Server::with(&["a"], 10 * 60_000);
        assert_eq!(found(&server, &["a", "a", "a"], 30_000), Vec::<String>::new());
        assert_eq!((server.given.lock().unwrap().len(), server.spent()), (1, Duration::from_millis(30_000)));
        // With no time left at all, nothing is asked.
        let server = Server::with(&["a"], 100);
        assert_eq!((found(&server, &["a"], 0), server.given.lock().unwrap().len()), (Vec::new(), 0));
    }

    #[test]
    fn nothing_more_is_asked_once_the_sign_in_has_ended() {
        let mut server = Server::with(&["a", "b", "c"], 100);
        server.signed_out_after = Some(1);
        assert_eq!((found(&server, &["a", "b", "c"], 30_000), server.given.lock().unwrap().len()), (vec!["a".to_string()], 1));
    }

    /// A server that answers about another song than the one it was asked for (a proxy in
    /// front of it, an answer kept from another request): that is not the song asked
    /// for, and it is not handed back as if it were.
    #[test]
    fn an_answer_about_another_song_than_the_one_asked_for_is_not_taken() {
        let mut server = Server::with(&["a", "b", "c"], 100);
        server.answers_with = Some("b");
        assert_eq!(found(&server, &["a", "b", "c"], 30_000), ["b"]);
        assert_eq!(server.given.lock().unwrap().len(), 3);
    }

    /// The answer is the server's word on that song and nothing else: one that is not
    /// well, and one that is not an answer, give nothing.
    #[test]
    fn only_the_servers_own_word_on_the_song_asked_for_counts() {
        assert_eq!(song(r#"{"subsonic-response":{"status":"ok","song":{"id":"a","title":"A"}}}"#).map(|song| song.id), Some("a".into()));
        for not in [r#"{"subsonic-response":{"status":"failed","song":{"id":"a"}}}"#, r#"{"subsonic-response":{"status":"ok"}}"#, r#"{"subsonic-response":{"status":"ok","song":[1]}}"#, r#"{"song":{"id":"a"}}"#, "<html>502</html>", ""] {
            assert_eq!(song(not), None, "{not}");
        }
    }
}
