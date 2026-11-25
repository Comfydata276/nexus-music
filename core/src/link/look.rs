//! The look: reading the secret from the music server, as a sequence of requests.
//!
//! What a look decides is in [`super::secret`] (which playlist, what its answer means).
//! This is the asking: list the playlists, fetch the one chosen, and where the answer
//! says so make the playlist, or write a new secret into it and read that back. It asks
//! through the app's [`Http`], so the app's own connections, timeouts and certificates are
//! used, and it holds no connection of its own.
//!
//! **Every request is a form POST** with the sign-in in the form, so that neither the
//! sign-in nor the secret is ever in an address (and so never in a log of addresses).
//! Navidrome takes a form POST for every call. The app's `Http` for this must follow no
//! redirect: a redirect would carry the form to wherever it pointed.
//!
//! **Evidence is not forgotten by a retry.** Once the record has been seen to be no good
//! (public, not a record, gone, or the playlist that was used no longer there), the look
//! can end only two ways: a later read shows a good record, which is then the one to use;
//! or the look says the record is no good ([`Looked::Invalid`]), which ends the session.
//! It never ends "the server could not be asked" ([`Looked::Failed`]), which would leave
//! the old secret in use until its time ran out on the strength of a request that
//! happened to fail afterwards.
//!
//! **A look that resets** ("Reset linking") does everything a look does, and where it
//! would have used the secret it read, writes a new one in its place and reads that back.
//! It is not a different path to the playlist: the same answer is checked the same way
//! before anything is written, so a reset cannot write into a playlist a look would not
//! use. Once it has written, the secret that was read is the record no longer.
//!
//! **It stops with the sign-in.** When the sign-in the look is for ends, the app's `Http`
//! says [`Failure::Stopped`] to the next request and makes none; the look ends there, and
//! what it says is of no account (its answer is the answer to a look nobody is waiting on).
//! That is so even after it has written: signing out is not leaving a page. For an app
//! that can otherwise be told to stop (a phone whose screen was left), the look says
//! [`Http::keep_going`] before its first write, so that a secret it writes is read back.

use serde_json::Value;

use super::secret::{self, Next, Secret, Stop, Verdict, Why};
use crate::http::{Failure, Http, Method, Request};
use crate::step::Stated;

/// The sign-in to the music server, as a look uses it. Not shown (`{:?}`): the form is
/// the sign-in itself.
#[derive(Clone)]
pub struct Account {
    /// Where the server's calls are: its address with `/rest` after it, and no `/` at the end.
    pub rest: String,
    /// The sign-in as the fields of a form, ready to send: `u=…&t=…&s=…&v=…&c=…`, each
    /// value encoded. Whatever the call adds comes after it.
    pub form: String,
    /// The account's name, as the server states an owner.
    pub username: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Account({})", self.username)
    }
}

/// What a look at the secret came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Looked {
    /// The secret a look read, the playlist it is in, and why a new one was written on the
    /// way, if one was.
    Secret { secret: Secret, id: String, wrote: Option<Why> },
    /// The record is not one to use, and not one to write over.
    Stopped(Stop),
    /// The server could not be asked, or kept undoing what was written. Nothing is
    /// concluded: what is held is neither renewed nor replaced.
    Failed,
    /// The record was found to be no good (public, another playlist's, not a record,
    /// reset) and a good one could not be read in its place: what is held is not to be used.
    Invalid,
    /// The system gave no random bytes: nothing is written, and linking is off.
    NoRandom,
}

/// Whether what a look found means the secret in use must not go on being used. That the
/// server could not be asked means nothing of the kind: the secret then runs out in its own
/// time. (A look that read a different secret ends the old session by beginning a new one.)
pub fn ends_session(looked: &Looked) -> bool {
    matches!(looked, Looked::Stopped(_) | Looked::Invalid)
}

/// Why a request came to nothing.
enum Not {
    /// No answer, or not one the server gives when all is well.
    Asked,
    /// The sign-in has ended: ask nothing more.
    Wanted,
}

/// One call: its answer's text. `with` are the call's own fields, as they are (encoded here).
fn call(http: &dyn Http, account: &Account, endpoint: &str, with: &[(&str, &str)], xml: bool) -> Result<String, Not> {
    let mut form = format!("{}&f={}", account.form, if xml { "xml" } else { "json" });
    for (name, value) in with {
        form.push_str(&format!("&{name}={}", crate::text::encode(value)));
    }
    let url = format!("{}/{endpoint}", account.rest);
    let request = Request { method: Method::Post(&form), url: &url, headers: &[("Content-Type", "application/x-www-form-urlencoded")], patience: None };
    match http.send(&request) {
        Ok(response) => response.body().map_err(|_| Not::Asked),
        Err(Failure::Stopped) => Err(Not::Wanted),
        Err(Failure::Reach(_)) => Err(Not::Asked),
    }
}

/// The same, for a call answered in JSON: what is inside its `subsonic-response`, if the
/// server says all is well.
fn answer(http: &dyn Http, account: &Account, endpoint: &str, with: &[(&str, &str)]) -> Result<Value, Not> {
    let text = call(http, account, endpoint, with, false)?;
    let mut root: Value = serde_json::from_str(&text).map_err(|_| Not::Asked)?;
    let body = root.get_mut("subsonic-response").map(Value::take).ok_or(Not::Asked)?;
    if body["status"].as_str() == Some("ok") { Ok(body) } else { Err(Not::Asked) }
}

/// One look. `used` is the playlist the secret was last read from under this sign-in;
/// `reset` asks for a new secret. `random` gives thirty-two bytes from the system's source
/// of randomness, or `None` if it has none to give. `log` is told what was written, in
/// words that name no secret.
pub fn look(http: &dyn Http, account: &Account, mut used: Option<String>, mut reset: bool, random: &mut dyn FnMut() -> Option<[u8; 32]>, log: &mut dyn FnMut(String)) -> Looked {
    let mut wrote = None;
    // The record has been seen to be no good in this look.
    let mut no_good = false;
    // It has written nothing yet.
    let mut written = false;
    let unasked = |no_good: bool| if no_good { Looked::Invalid } else { Looked::Failed };
    macro_rules! ask {
        ($asked:expr, $otherwise:expr) => {
            match $asked {
                Ok(answer) => answer,
                Err(Not::Wanted) => return Looked::Failed,
                Err(Not::Asked) => return $otherwise,
            }
        };
    }
    let write = |id: &str, fresh: &Secret, written: &mut bool| {
        if !std::mem::replace(written, true) {
            http.keep_going();
        }
        answer(http, account, "updatePlaylist", &[("playlistId", id), ("comment", &fresh.comment()), ("public", "false")]).map(drop)
    };
    for _ in 0..secret::ROUNDS {
        let listed = ask!(answer(http, account, "getPlaylists", &[]), unasked(no_good));
        let listed: Vec<Stated> = serde_json::from_value(listed["playlists"]["playlist"].clone()).unwrap_or_default();
        let (id, replace) = match secret::choose(&listed, &account.username, used.as_deref()) {
            Next::Stop(stop) => return Looked::Stopped(stop),
            Next::Fetch { id, replace } => (id, replace),
            Next::Make => {
                // There is none. If one was in use, that one has gone.
                no_good |= used.is_some();
                let made = ask!(answer(http, account, "createPlaylist", &[("name", secret::PLAYLIST_NAME)]), unasked(no_good));
                let Some(id) = made["playlist"]["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string) else { return unasked(no_good) };
                let Some(fresh) = random().map(Secret::new) else { return Looked::NoRandom };
                ask!(write(&id, &fresh, &mut written), unasked(no_good));
                log(format!("look: made the playlist {id} and wrote a secret into it"));
                used = Some(id);
                continue;
            }
        };
        // One answer, and the XML one: only it states whether the playlist is public, and
        // what it is, whose, and its comment are taken from the same answer.
        let fetched = ask!(call(http, account, "getPlaylist", &[("id", &id)], true), unasked(no_good));
        let Some(fetched) = secret::fetched(&fetched) else { return unasked(no_good) };
        match secret::judge(&id, replace, &fetched, &account.username) {
            Verdict::Use(_) if reset => {
                reset = false;
                let Some(fresh) = random().map(Secret::new) else { return Looked::NoRandom };
                // (If it cannot be written, the secret that is there stands, and the look
                // says only that the server could not be asked.)
                ask!(write(&id, &fresh, &mut written), unasked(no_good));
                log(format!("look: wrote a new secret into {id} (Reset)"));
                wrote = Some(Why::Reset);
                used = Some(id);
                // The secret that was read is the record no longer: if the new one cannot
                // be read back, the look says so, and the old one is not used meanwhile.
                no_good = true;
            }
            Verdict::Use(secret) => return Looked::Secret { secret, id, wrote },
            Verdict::Stop(stop) => return Looked::Stopped(stop),
            Verdict::Write(why) => {
                no_good = true;
                // A new secret into it, and it is kept private. The secret used afterwards
                // is the one the next round reads back, not this one.
                let Some(fresh) = random().map(Secret::new) else { return Looked::NoRandom };
                ask!(write(&id, &fresh, &mut written), Looked::Invalid);
                log(format!("look: wrote a new secret into {id} ({why:?})"));
                wrote = Some(why);
                used = Some(id);
            }
        }
    }
    // It kept being undone.
    unasked(no_good)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::Response;

    /// A music server that answers from a script: what each fetch of the playlist gives
    /// (`None`: no answer), and whether listing and writing work.
    struct Scripted {
        fetches: Mutex<Vec<Option<String>>>,
        listed: Vec<Value>,
        list_works: bool,
        write_works: bool,
        asked: Mutex<Vec<&'static str>>,
        /// The forms that were sent, to see what was in them and what was not.
        sent: Mutex<Vec<(String, String)>>,
        /// The person signs out once this many requests have been made.
        signed_out_after: Option<usize>,
        kept_going: Mutex<Option<usize>>,
    }

    impl Scripted {
        fn with(fetches: Vec<Option<String>>) -> Scripted {
            let listed = vec![serde_json::json!({"id": "k", "name": secret::PLAYLIST_NAME, "owner": "tester"})];
            Scripted { fetches: Mutex::new(fetches), listed, list_works: true, write_works: true, asked: Mutex::default(), sent: Mutex::default(), signed_out_after: None, kept_going: Mutex::default() }
        }

        fn asked(&self) -> Vec<&'static str> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Http for Scripted {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let mut asked = self.asked.lock().unwrap();
            if self.signed_out_after.is_some_and(|requests| asked.len() >= requests) {
                return Err(Failure::Stopped);
            }
            let Method::Post(form) = request.method else { panic!("a look asks by POST and no other way") };
            self.sent.lock().unwrap().push((request.url.to_string(), form.to_string()));
            let ok = |inside: Value| Ok(Response::new(200, Vec::new(), move || Ok(serde_json::json!({"subsonic-response": inside}).to_string())));
            let unreachable = || Err(Failure::Reach("could not be reached".into()));
            match request.url.rsplit('/').next().unwrap_or_default() {
                "getPlaylists" => {
                    asked.push("list");
                    if self.list_works { ok(serde_json::json!({"status": "ok", "playlists": {"playlist": self.listed}})) } else { unreachable() }
                }
                "getPlaylist" => {
                    asked.push("fetch");
                    let mut fetches = self.fetches.lock().unwrap();
                    match if fetches.is_empty() { None } else { fetches.remove(0) } {
                        Some(text) => Ok(Response::new(200, Vec::new(), move || Ok(text))),
                        None => unreachable(),
                    }
                }
                "createPlaylist" => {
                    asked.push("make");
                    ok(serde_json::json!({"status": "ok", "playlist": {"id": "made"}}))
                }
                "updatePlaylist" => {
                    asked.push("write");
                    if self.write_works { ok(serde_json::json!({"status": "ok"})) } else { unreachable() }
                }
                other => panic!("a look asked for {other}"),
            }
        }

        fn keep_going(&self) {
            self.kept_going.lock().unwrap().get_or_insert(self.asked.lock().unwrap().len());
        }
    }

    fn account() -> Account {
        Account { rest: "http://music.example/rest".into(), form: "u=tester&t=the-token&s=the-salt&v=1.16.1&c=test".into(), username: "tester".into() }
    }

    fn looks(server: &Scripted, used: Option<&str>, reset: bool) -> Looked {
        look(server, &account(), used.map(str::to_string), reset, &mut || Some([9; 32]), &mut |_| {})
    }

    /// The playlist as Navidrome answers for it: the record, or somebody's note; private or not.
    fn playlist(comment: &str, public: bool) -> Option<String> {
        let comment = comment.replace('"', "&#34;");
        Some(format!(r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" comment="{comment}" songCount="0" public="{public}" owner="tester"></playlist></subsonic-response>"#))
    }

    const RECORD: &str = r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#;

    #[test]
    fn a_look_reads_the_record_by_form_post_and_puts_nothing_in_an_address() {
        let server = Scripted::with(vec![playlist(RECORD, false)]);
        assert!(matches!(looks(&server, Some("k"), false), Looked::Secret { wrote: None, .. }));
        assert_eq!(server.asked(), ["list", "fetch"]);
        let sent = server.sent.lock().unwrap().clone();
        // The sign-in is in the form, and the address is the call and nothing else.
        assert_eq!(sent[0], ("http://music.example/rest/getPlaylists".to_string(), "u=tester&t=the-token&s=the-salt&v=1.16.1&c=test&f=json".to_string()));
        assert_eq!(sent[1], ("http://music.example/rest/getPlaylist".to_string(), "u=tester&t=the-token&s=the-salt&v=1.16.1&c=test&f=xml&id=k".to_string()));
        // A look that reads and writes nothing asks nobody to keep going.
        assert_eq!(*server.kept_going.lock().unwrap(), None);
        // What is written goes in the form too, encoded, and the playlist is kept private.
        let server = Scripted::with(vec![playlist("", false), playlist(RECORD, false)]);
        looks(&server, Some("k"), false);
        let sent = server.sent.lock().unwrap().clone();
        assert_eq!(sent[2].0, "http://music.example/rest/updatePlaylist");
        assert!(sent[2].1.contains("&playlistId=k&comment=%7B%22nexus_music_link%22") && sent[2].1.ends_with("&public=false"), "{}", sent[2].1);
        // Before its first write it says that what it has begun is to be seen through.
        assert_eq!(*server.kept_going.lock().unwrap(), Some(2));
        assert_eq!(format!("{:?}", account()), "Account(tester)");
    }

    /// The reviewer's case: the record is found public, a new secret is written, and the
    /// read that should show it fails. The old secret must not live on.
    #[test]
    fn a_record_seen_to_be_no_good_is_not_forgotten_when_a_later_request_fails() {
        let found = |fetches: Vec<Option<String>>| {
            let server = Scripted::with(fetches);
            let looked = looks(&server, Some("k"), false);
            (ends_session(&looked), matches!(looked, Looked::Secret { .. }), server.asked())
        };
        // Public; written; the read-back gets no answer.
        let (ends, _, asked) = found(vec![playlist(RECORD, true), None]);
        assert!(ends, "the old secret would live on until its time ran out");
        assert_eq!(asked, ["list", "fetch", "write", "list", "fetch"]);
        // The same when the playlist that was used has gone, when the answer is not one
        // about a playlist, and when what was written keeps not being there.
        let mut server = Scripted::with(vec![playlist(RECORD, true)]);
        server.listed.clear();
        assert!(ends_session(&looks(&server, Some("gone"), false)) || server.asked().contains(&"make"));
        assert!(found(vec![playlist("somebody's note", false), Some("not an answer".into())]).0);
        assert!(found(vec![playlist(RECORD, true), playlist(RECORD, true), playlist(RECORD, true)]).0);
        // Unless a later read shows it good: then that is the secret to use.
        let (ends, good, _) = found(vec![playlist(RECORD, true), playlist(RECORD, false)]);
        assert!(!ends && good);
        // And with nothing seen to be wrong, not being able to ask concludes nothing.
        let (ends, good, asked) = found(vec![None]);
        assert!(!ends && !good && asked == ["list", "fetch"]);
        let mut server = Scripted::with(vec![]);
        server.list_works = false;
        assert!(!ends_session(&looks(&server, Some("k"), false)));
        // A record found no good that cannot be written over is no good either.
        let mut server = Scripted::with(vec![playlist(RECORD, true)]);
        server.write_works = false;
        assert!(ends_session(&looks(&server, Some("k"), false)));
        // An answer the server gives when all is not well is no answer.
        let server = Scripted::with(vec![Some(r#"<subsonic-response status="failed"><error code="70"/></subsonic-response>"#.into())]);
        assert_eq!(looks(&server, Some("k"), false), Looked::Failed);
    }

    /// The person signs out while a refresh is under way: the request in flight finishes,
    /// and the refresh makes no other.
    #[test]
    fn a_look_makes_no_request_after_the_sign_in_it_is_for_has_ended() {
        // A look that would take five requests: list, fetch (public), write, list, fetch.
        let whole = || Scripted::with(vec![playlist(RECORD, true), playlist(RECORD, false)]);
        let server = whole();
        looks(&server, Some("k"), false);
        assert_eq!(server.asked(), ["list", "fetch", "write", "list", "fetch"]);
        // Signed out during each of them in turn: that one is the last, written or not.
        for during in 1..=5 {
            let mut server = whole();
            server.signed_out_after = Some(during);
            let looked = looks(&server, Some("k"), false);
            assert_eq!(server.asked().len(), during.min(5), "signed out during request {during}: {:?}", server.asked());
            if during < 5 {
                assert_eq!(looked, Looked::Failed);
            }
        }
        // Signed out before it began, it asks nothing at all. And with a playlist to make.
        let mut server = whole();
        server.signed_out_after = Some(0);
        looks(&server, None, false);
        assert!(server.asked().is_empty());
        let mut server = Scripted::with(vec![]);
        server.listed.clear();
        server.signed_out_after = Some(2);
        looks(&server, None, false);
        assert_eq!(server.asked(), ["list", "make"]);
    }

    /// "Reset linking": a look that writes a new secret where it would have used the one it
    /// read, and uses only what it then reads back.
    #[test]
    fn a_look_that_resets_writes_a_new_secret_and_reads_it_back() {
        let fresh = Secret::new([9; 32]);
        let written = fresh.comment();
        let server = Scripted::with(vec![playlist(RECORD, false), playlist(&written, false)]);
        let looked = looks(&server, Some("k"), true);
        assert_eq!(server.asked(), ["list", "fetch", "write", "list", "fetch"]);
        assert!(matches!(&looked, Looked::Secret { secret, wrote: Some(Why::Reset), .. } if *secret == fresh), "the reset did not come to the new secret");
        // The same look without the reset reads the record and writes nothing.
        let server = Scripted::with(vec![playlist(RECORD, false)]);
        assert!(matches!(looks(&server, Some("k"), false), Looked::Secret { wrote: None, .. }));
        assert_eq!(server.asked(), ["list", "fetch"]);
        // It writes once: reading the new one back is not a reason to write another.
        let server = Scripted::with(vec![playlist(RECORD, false), playlist(&written, false), playlist(&written, false)]);
        looks(&server, Some("k"), true);
        assert_eq!(server.asked().iter().filter(|asked| **asked == "write").count(), 1);
        // Nothing a look would not use is written into: a playlist that is somebody's, one
        // with a later layout.
        let somebody = r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" comment="" songCount="3" public="false" owner="tester"></playlist></subsonic-response>"#;
        for stops in [Some(somebody.to_string()), playlist(r#"{"nexus_music_link":2}"#, false)] {
            let server = Scripted::with(vec![stops]);
            assert!(matches!(looks(&server, Some("k"), true), Looked::Stopped(_)));
            assert!(!server.asked().contains(&"write"), "{:?}", server.asked());
        }
    }

    #[test]
    fn a_reset_that_cannot_be_written_leaves_the_secret_and_one_that_cannot_be_read_back_ends_it() {
        // The write gets no answer: nothing has changed, and nothing is concluded.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.write_works = false;
        let looked = looks(&server, Some("k"), true);
        assert!(looked == Looked::Failed && !ends_session(&looked));
        assert_eq!(server.asked(), ["list", "fetch", "write"]);
        // Written, and the read-back gets no answer: the secret that was in use is the
        // record no longer, and is not used on the strength of a request that failed.
        let server = Scripted::with(vec![playlist(RECORD, false), None]);
        assert!(ends_session(&looks(&server, Some("k"), true)), "the old secret would live on after a reset");
        // Signed out as it began: nothing is written.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.signed_out_after = Some(2);
        looks(&server, Some("k"), true);
        assert_eq!(server.asked(), ["list", "fetch"]);
    }

    #[test]
    fn with_no_random_bytes_to_be_had_nothing_is_written() {
        let server = Scripted::with(vec![playlist("", false)]);
        let looked = look(&server, &account(), Some("k".into()), false, &mut || None, &mut |_| {});
        assert_eq!(looked, Looked::NoRandom);
        assert_eq!(server.asked(), ["list", "fetch"]);
        // Nor is a playlist that was just made written into; and what was written is said
        // without the secret.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.listed.clear();
        let mut said = Vec::new();
        look(&server, &account(), None, false, &mut || Some([9; 32]), &mut |line| said.push(line));
        // (This server never lists what was made, so it is made again each round.)
        assert!(!said.is_empty() && said.iter().all(|line| line == "look: made the playlist made and wrote a secret into it"), "{said:?}");
    }
}
