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
        // (Only an answer the server gives when all is well is read: a proxy's error page,
        // or a 500 with a body that happens to parse, is no answer.)
        Ok(response) if response.status != 200 => Err(Not::Asked),
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

/// The playlists an answer to "list the playlists" states. `None` unless it **is** such an
/// answer: a `playlists` object, in which `playlist` is either left out (there are none,
/// which is how the server says so) or a list, each entry an object with an id and a name
/// that are text. An answer of any other shape is no answer. It is never taken to say that
/// there are no playlists: that would be leave to make one.
fn playlists(body: &Value) -> Option<Vec<Stated>> {
    let listed = match body.get("playlists")?.as_object()?.get("playlist") {
        None => return Some(Vec::new()),
        Some(listed) => listed.as_array()?,
    };
    listed
        .iter()
        .map(|entry| {
            let fields = entry.as_object()?;
            // (Each of its own kind where it is there: text, not a number that reads as text.)
            let said = |name: &str| fields.get(name).map(|value| value.as_str().map(str::to_string));
            let id = said("id")??;
            if id.trim().is_empty() {
                return None;
            }
            Some(Stated {
                id,
                name: said("name")??,
                owner: match said("owner") {
                    None => None,
                    Some(owner) => Some(owner?),
                },
                comment: match said("comment") {
                    None => None,
                    Some(comment) => Some(comment?),
                },
            })
        })
        .collect()
}

/// One look. `used` is the playlist the secret was last read from under this sign-in;
/// `reset` asks for a new secret. `random` gives thirty-two bytes from the system's source
/// of randomness, or `None` if it has none to give. `log` is told what was written, in
/// words that name no secret.
///
/// **A write that gets no answer is followed by a read.** The server may have written it
/// all the same, so nothing is concluded from the silence: the look goes round again, and
/// what it then reads is what is true. If it cannot read either, the record is not known
/// to be good and what is held is not to be used.
///
/// **The secret used is the one that stands, whoever wrote it.** After a write the look
/// reads again, and what that read shows is used. If it is not the secret this look wrote
/// for a reset (the write never landed and the old one stands, or another device wrote at
/// the same moment and its secret stands), it is taken all the same, and the look does
/// not say it reset anything.
///
/// **Making the record's playlist.** It is made with its own name and nothing in it, and
/// filled in the next request. If the look stops between the two, what it leaves is a
/// playlist of the name with nothing in it, which the next look fills
/// ([`secret::judge`]).
pub fn look(http: &dyn Http, account: &Account, mut used: Option<String>, mut reset: bool, random: &mut dyn FnMut() -> Option<[u8; 32]>, log: &mut dyn FnMut(String)) -> Looked {
    let mut wrote = None;
    // The record has been seen to be no good in this look.
    let mut no_good = false;
    // It has changed nothing on the server yet.
    let mut changed = false;
    // The secret this look last wrote.
    let mut written: Option<Secret> = None;
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
    // Before the first request that changes anything on the server: what is begun is to
    // be seen through.
    let changing = |changed: &mut bool| {
        if !std::mem::replace(changed, true) {
            http.keep_going();
        }
    };
    // A new secret into a playlist, kept private. Whether it was answered or not: no
    // answer is not "not written".
    macro_rules! put {
        ($id:expr, $fresh:expr, $what:expr) => {{
            let comment = $fresh.comment();
            match answer(http, account, "updatePlaylist", &[("playlistId", $id.as_str()), ("comment", comment.as_str()), ("public", "false")]) {
                Ok(_) => log(format!("look: {}", $what)),
                Err(Not::Wanted) => return Looked::Failed,
                Err(Not::Asked) => log(format!("look: no answer to writing a secret into {}: reading what is there", $id)),
            }
            written = Some($fresh);
        }};
    }
    for _ in 0..secret::ROUNDS {
        let listed = ask!(answer(http, account, "getPlaylists", &[]), unasked(no_good));
        let Some(listed) = playlists(&listed) else {
            log("look: the answer to listing the playlists is not a list of playlists".into());
            return unasked(no_good);
        };
        let (id, replace) = match secret::choose(&listed, &account.username, used.as_deref()) {
            Next::Stop(stop) => return Looked::Stopped(stop),
            Next::Fetch { id, replace } => (id, replace),
            Next::Make => {
                // There is none. If one was in use, that one has gone.
                no_good |= used.is_some();
                // (Nothing is made without a secret to put in it.)
                let Some(fresh) = random().map(Secret::new) else { return Looked::NoRandom };
                changing(&mut changed);
                let made = ask!(answer(http, account, "createPlaylist", &[("name", secret::PLAYLIST_NAME)]), unasked(no_good));
                let Some(id) = made["playlist"]["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string) else { return unasked(no_good) };
                put!(id, fresh, format!("made the playlist {id} and wrote a secret into it"));
                wrote = Some(Why::Made);
                // (That the one used before has gone is known now, and has been noted: the
                // playlist just made is not "another one, found in its place".)
                used = None;
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
                changing(&mut changed);
                put!(id, fresh, format!("wrote a new secret into {id} (Reset)"));
                wrote = Some(Why::Reset);
                used = Some(id);
                // The secret that was read may be the record no longer: it is used again
                // only if a read shows it is.
                no_good = true;
            }
            Verdict::Use(secret) => {
                // The read after a reset's write shows a secret that is not the one this
                // look wrote: the write never landed and the old one stands, or another
                // device wrote at the same moment and its secret stands. The one that
                // stands is the one to use, and this look reset nothing.
                if wrote == Some(Why::Reset) && written.as_ref() != Some(&secret) {
                    log("look: the secret that stands is not the one this look wrote: nothing was reset by it".into());
                    wrote = None;
                }
                return Looked::Secret { secret, id, wrote };
            }
            Verdict::Stop(stop) => return Looked::Stopped(stop),
            Verdict::Write(why) => {
                no_good = true;
                // A new secret into it, and it is kept private. The secret used afterwards
                // is the one the next round reads back, not this one.
                let Some(fresh) = random().map(Secret::new) else { return Looked::NoRandom };
                changing(&mut changed);
                put!(id, fresh, format!("wrote a new secret into {id} ({why:?})"));
                wrote = Some(why);
                used = Some(id);
            }
        }
    }
    // It kept being undone, or kept not being answered.
    unasked(no_good)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::Response;
    use crate::link::secret::Stop;

    /// A music server that answers from a script: what each fetch of the record's playlist
    /// gives (`None`: no answer), whether listing and writing work, and one playlist it
    /// can be asked to make, which it then lists and gives as it was last written.
    struct Scripted {
        fetches: Mutex<Vec<Option<String>>>,
        listed: Vec<Value>,
        list_works: bool,
        write_works: bool,
        /// The playlist it was asked to make, or holds from before: its id, name, comment
        /// and how many songs are in it.
        made: Mutex<Option<(String, String, String, u32)>>,
        asked: Mutex<Vec<&'static str>>,
        /// The forms that were sent, to see what was in them and what was not.
        sent: Mutex<Vec<(String, String)>>,
        /// The person signs out once this many requests have been made.
        signed_out_after: Option<usize>,
        kept_going: Mutex<Option<usize>>,
        /// The request that goes wrong (counting from none), and how.
        fails: Option<(usize, Fail)>,
        /// The playlists it lists are there only once this many requests have been made:
        /// another device made them meanwhile.
        listed_from: usize,
        /// What it answers to "list the playlists" in place of its list, if anything: an
        /// answer the server says is ok, and of some other shape.
        lists_as: Option<Value>,
    }

    /// A way one request can go wrong.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Fail {
        /// The server cannot be reached, or the request times out: it never arrives.
        NoAnswer,
        /// It arrives, the server does what was asked, and the answer is lost on the way back.
        Lost,
        /// Something answers that is not the music server: a proxy's error page.
        NotTheServer,
        /// The server's own answer, word for word as when all is well, under a status that
        /// says it is not.
        NotOkStatus,
        /// The server answers that all is not well.
        NotOk,
        /// The person signs out while it is under way.
        SignedOut,
    }

    impl Scripted {
        fn with(fetches: Vec<Option<String>>) -> Scripted {
            let listed = vec![serde_json::json!({"id": "k", "name": secret::PLAYLIST_NAME, "owner": "tester"})];
            Scripted {
                fetches: Mutex::new(fetches),
                listed,
                list_works: true,
                write_works: true,
                made: Mutex::default(),
                asked: Mutex::default(),
                sent: Mutex::default(),
                signed_out_after: None,
                kept_going: Mutex::default(),
                fails: None,
                listed_from: 0,
                lists_as: None,
            }
        }

        /// A server with no playlist of the name on it.
        fn bare() -> Scripted {
            let mut server = Scripted::with(vec![]);
            server.listed.clear();
            server
        }

        fn asked(&self) -> Vec<&'static str> {
            self.asked.lock().unwrap().clone()
        }
    }

    /// A field of a form, as it was meant.
    fn field(form: &str, name: &str) -> Option<String> {
        let value = form.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}=")))?;
        let (mut out, mut bytes) = (Vec::new(), value.bytes());
        while let Some(byte) = bytes.next() {
            match byte {
                b'%' => out.push(u8::from_str_radix(&String::from_utf8(vec![bytes.next()?, bytes.next()?]).ok()?, 16).ok()?),
                b'+' => out.push(b' '),
                other => out.push(other),
            }
        }
        String::from_utf8(out).ok()
    }

    impl Http for Scripted {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let mut asked = self.asked.lock().unwrap();
            if self.signed_out_after.is_some_and(|requests| asked.len() >= requests) {
                return Err(Failure::Stopped);
            }
            let Method::Post(form) = request.method else { panic!("a look asks by POST and no other way") };
            self.sent.lock().unwrap().push((request.url.to_string(), form.to_string()));
            let call = request.url.rsplit('/').next().unwrap_or_default();
            let how = self.fails.filter(|(at, _)| *at == asked.len()).map(|(_, how)| how);
            asked.push(match call {
                "getPlaylists" => "list",
                "getPlaylist" => "fetch",
                "createPlaylist" => "make",
                "updatePlaylist" => "write",
                other => panic!("a look asked for {other}"),
            });
            let says = |status: u16, text: String| Ok(Response::new(status, Vec::new(), move || Ok(text)));
            let unreachable = || Err(Failure::Reach("could not be reached".into()));
            match (how, call) {
                (Some(Fail::NoAnswer), _) => return unreachable(),
                (Some(Fail::SignedOut), _) => return Err(Failure::Stopped),
                (Some(Fail::NotTheServer), _) => return says(502, "<html><body>502 Bad Gateway</body></html>".into()),
                (Some(Fail::NotOk), "getPlaylist") => return says(200, r#"<subsonic-response status="failed"><error code="0" message="not now"/></subsonic-response>"#.into()),
                (Some(Fail::NotOk), _) => return says(200, r#"{"subsonic-response":{"status":"failed","error":{"code":0,"message":"not now"}}}"#.into()),
                _ => {}
            }
            // What the server does, and what it would say of it.
            let ok = |inside: Value| Some(serde_json::json!({"subsonic-response": inside}).to_string());
            let mut made = self.made.lock().unwrap();
            let answer = match call {
                "getPlaylists" if !self.list_works => None,
                "getPlaylists" if self.lists_as.is_some() => {
                    let mut inside = self.lists_as.clone().unwrap_or_default();
                    inside["status"] = "ok".into();
                    ok(inside)
                }
                "getPlaylists" => {
                    let mut listed = if asked.len() > self.listed_from { self.listed.clone() } else { Vec::new() };
                    if let Some((id, name, ..)) = made.as_ref() {
                        listed.push(serde_json::json!({"id": id, "name": name, "owner": "tester"}));
                    }
                    ok(serde_json::json!({"status": "ok", "playlists": {"playlist": listed}}))
                }
                "getPlaylist" => match made.as_ref().filter(|(id, ..)| field(form, "id").as_deref() == Some(id.as_str())) {
                    Some((id, name, comment, songs)) => Some(format!(
                        r#"<subsonic-response status="ok"><playlist id="{id}" name="{name}" comment="{}" songCount="{songs}" public="false" owner="tester"></playlist></subsonic-response>"#,
                        comment.replace('"', "&#34;")
                    )),
                    None => {
                        let mut fetches = self.fetches.lock().unwrap();
                        if fetches.is_empty() { None } else { fetches.remove(0) }
                    }
                },
                "createPlaylist" => {
                    *made = Some(("made".into(), field(form, "name").unwrap_or_default(), String::new(), 0));
                    ok(serde_json::json!({"status": "ok", "playlist": {"id": "made"}}))
                }
                _ if !self.write_works => None,
                _ => {
                    if let Some((_, name, comment, _)) = made.as_mut().filter(|(id, ..)| field(form, "playlistId").as_deref() == Some(id.as_str())) {
                        *comment = field(form, "comment").unwrap_or_default();
                        if let Some(new) = field(form, "name") {
                            *name = new;
                        }
                    }
                    ok(serde_json::json!({"status": "ok"}))
                }
            };
            match (how, answer) {
                (Some(Fail::Lost), _) | (_, None) => unreachable(),
                (Some(Fail::NotOkStatus), Some(text)) => says(500, text),
                (_, Some(text)) => says(200, text),
            }
        }

        fn pause(&self, _: std::time::Duration) -> Result<(), Failure> {
            Ok(())
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

    /// The secret a look in these tests writes, and the record it makes of it.
    fn fresh() -> (Secret, String) {
        let fresh = Secret::new([9; 32]);
        let written = fresh.comment();
        (fresh, written)
    }

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
        let server = Scripted::with(vec![playlist(RECORD, true), playlist(RECORD, false)]);
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
        // The same when the answer is not one about a playlist, and when what was written
        // keeps not being there.
        assert!(found(vec![playlist(RECORD, true), Some("not an answer".into())]).0);
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
        // A record found no good that cannot be written over is no good either, however
        // many times it is tried.
        let mut server = Scripted::with(vec![playlist(RECORD, true); 3]);
        server.write_works = false;
        assert_eq!((looks(&server, Some("k"), false), server.asked().len()), (Looked::Invalid, 9));
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
        let mut server = Scripted::bare();
        server.signed_out_after = Some(2);
        assert_eq!((looks(&server, None, false), server.asked()), (Looked::Failed, vec!["list", "make"]));
    }

    /// "Reset linking": a look that writes a new secret where it would have used the one it
    /// read, and uses only what it then reads back.
    #[test]
    fn a_look_that_resets_writes_a_new_secret_and_reads_it_back() {
        let (fresh, written) = fresh();
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
        // with a later layout, one with a description of the person's own.
        let somebody = r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" comment="" songCount="3" public="false" owner="tester"></playlist></subsonic-response>"#;
        for stops in [Some(somebody.to_string()), playlist(r#"{"nexus_music_link":2}"#, false), playlist("a list of links", false)] {
            let server = Scripted::with(vec![stops]);
            assert!(matches!(looks(&server, Some("k"), true), Looked::Stopped(_)));
            assert!(!server.asked().contains(&"write"), "{:?}", server.asked());
        }
    }

    /// Two devices reset at the same moment, and the other's write lands last. What this
    /// one reads back is the other's secret, not its own and not the old one: it takes the
    /// one that stands, and does not say that it reset anything.
    #[test]
    fn a_look_that_reads_back_another_device_s_secret_takes_it_and_says_it_reset_nothing() {
        let theirs = Secret::new([4; 32]);
        let server = Scripted::with(vec![playlist(RECORD, false), playlist(&theirs.comment(), false)]);
        let mut said = Vec::new();
        let looked = look(&server, &account(), Some("k".into()), true, &mut || Some([9; 32]), &mut |line| said.push(line));
        assert_eq!(server.asked(), ["list", "fetch", "write", "list", "fetch"]);
        assert_eq!(looked, Looked::Secret { secret: theirs.clone(), id: "k".into(), wrote: None });
        assert_eq!(said.last().map(String::as_str), Some("look: the secret that stands is not the one this look wrote: nothing was reset by it"));
        // The same where a record that others could see was replaced by two at once: the
        // one that stands is used, and the person is still told that it had been public.
        let server = Scripted::with(vec![playlist(RECORD, true), playlist(&theirs.comment(), false)]);
        assert_eq!(looks(&server, Some("k"), false), Looked::Secret { secret: theirs, id: "k".into(), wrote: Some(Why::Public) });
    }

    /// A proxy says 200 and then the connection drops: the start of an answer, whose
    /// opening tags read perfectly well. It is no answer.
    #[test]
    fn half_an_answer_is_no_answer_whatever_status_it_came_under() {
        let whole = playlist(RECORD, false).unwrap();
        for cut in [whole.len() - 1, whole.len() - "</subsonic-response>".len(), whole.find("></playlist>").unwrap() + 1] {
            let server = Scripted::with(vec![Some(whole[..cut].to_string())]);
            assert_eq!((looks(&server, Some("k"), false), server.asked()), (Looked::Failed, vec!["list", "fetch"]), "cut at {cut}");
        }
        // After the record has been seen to be no good, it concludes what any failure does.
        let server = Scripted::with(vec![playlist(RECORD, true), Some(whole[..whole.len() - 3].to_string())]);
        assert_eq!(looks(&server, Some("k"), false), Looked::Invalid);
    }

    /// A write that gets no answer may have been written all the same: the look reads, and
    /// goes by what it reads.
    #[test]
    fn a_write_that_is_not_answered_is_followed_by_a_read() {
        let (fresh, written) = fresh();
        let old = match secret::read(RECORD) {
            secret::Record::Is(secret) => secret,
            _ => unreachable!(),
        };
        let unanswered = |fetches: Vec<Option<String>>, how: Fail, reset: bool| {
            let mut server = Scripted::with(fetches);
            server.fails = Some((2, how));
            let looked = looks(&server, Some("k"), reset);
            (looked, server.asked())
        };
        // A reset whose write never arrived, or was refused: the read shows the secret that
        // stood, which is used, and nothing is said to have been reset.
        for how in [Fail::NoAnswer, Fail::NotTheServer, Fail::NotOkStatus, Fail::NotOk] {
            let (looked, asked) = unanswered(vec![playlist(RECORD, false), playlist(RECORD, false)], how, true);
            assert_eq!((looked, asked), (Looked::Secret { secret: old.clone(), id: "k".into(), wrote: None }, vec!["list", "fetch", "write", "list", "fetch"]), "{how:?}");
        }
        // One that was written, with its answer lost: the read shows the new secret, and
        // the reset was done.
        let (looked, _) = unanswered(vec![playlist(RECORD, false), playlist(&written, false)], Fail::Lost, true);
        assert_eq!(looked, Looked::Secret { secret: fresh.clone(), id: "k".into(), wrote: Some(Why::Reset) });
        // Neither the write nor the read is answered: the record is not known to be good,
        // and what is held is not to be used.
        let (looked, asked) = unanswered(vec![playlist(RECORD, false), None], Fail::NoAnswer, true);
        assert_eq!((looked, asked), (Looked::Invalid, vec!["list", "fetch", "write", "list", "fetch"]));
        // A public record whose replacement was not answered: read again, found public
        // still, written again, read back.
        let (looked, asked) = unanswered(vec![playlist(RECORD, true), playlist(RECORD, true), playlist(&written, false)], Fail::NoAnswer, false);
        assert_eq!(asked, ["list", "fetch", "write", "list", "fetch", "write", "list", "fetch"]);
        assert_eq!(looked, Looked::Secret { secret: fresh.clone(), id: "k".into(), wrote: Some(Why::Public) });
        // And one that was written with its answer lost is not written twice.
        let (looked, asked) = unanswered(vec![playlist(RECORD, true), playlist(&written, false)], Fail::Lost, false);
        assert_eq!((looked, asked), (Looked::Secret { secret: fresh.clone(), id: "k".into(), wrote: Some(Why::Public) }, vec!["list", "fetch", "write", "list", "fetch"]));
        // Signed out as a reset began: nothing is written.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.signed_out_after = Some(2);
        assert_eq!((looks(&server, Some("k"), true), server.asked()), (Looked::Failed, vec!["list", "fetch"]));
    }

    /// The record's playlist is made with its name and nothing in it, and filled in the
    /// next request. Whatever stops a look between the two leaves a playlist of the name
    /// with nothing in it, and the next look fills that.
    #[test]
    fn the_playlist_is_made_and_filled_and_one_left_with_nothing_in_it_is_filled() {
        let (fresh, written) = fresh();
        let made = |wrote| Looked::Secret { secret: fresh.clone(), id: "made".into(), wrote };
        let server = Scripted::bare();
        let mut said = Vec::new();
        let looked = look(&server, &account(), None, false, &mut || Some([9; 32]), &mut |line| said.push(line));
        assert_eq!(server.asked(), ["list", "make", "write", "list", "fetch"]);
        assert_eq!(looked, made(Some(Why::Made)));
        let sent = server.sent.lock().unwrap().clone();
        assert_eq!(field(&sent[1].1, "name").as_deref(), Some(secret::PLAYLIST_NAME));
        assert_eq!((field(&sent[2].1, "name"), field(&sent[2].1, "comment"), field(&sent[2].1, "public").as_deref()), (None, Some(written.clone()), Some("false")));
        // It says the look is to be seen through before it makes the playlist, not after;
        // and what it says it wrote names no secret.
        assert_eq!(*server.kept_going.lock().unwrap(), Some(1));
        assert_eq!(said, ["look: made the playlist made and wrote a secret into it"]);
        // The write is not answered and did not arrive: the playlist is there with nothing
        // in it, and is filled. No second one is made.
        let mut server = Scripted::bare();
        server.fails = Some((2, Fail::NoAnswer));
        let looked = looks(&server, None, false);
        assert_eq!(server.asked(), ["list", "make", "write", "list", "fetch", "write", "list", "fetch"]);
        assert_eq!(looked, made(Some(Why::Made)));
        // The write arrived and its answer was lost: the read finds the record.
        let mut server = Scripted::bare();
        server.fails = Some((2, Fail::Lost));
        assert_eq!((looks(&server, None, false), server.asked()), (made(Some(Why::Made)), vec!["list", "make", "write", "list", "fetch"]));
        // The playlist was made and the answer to that was lost: this look concludes
        // nothing, and the next one finds it with nothing in it and fills it.
        let mut server = Scripted::bare();
        server.fails = Some((1, Fail::Lost));
        assert_eq!((looks(&server, None, false), server.asked()), (Looked::Failed, vec!["list", "make"]));
        assert_eq!((looks(&server, None, false), &server.asked()[2..]), (made(Some(Why::Made)), ["list", "fetch", "write", "list", "fetch"].as_slice()));
        // One left with nothing in it by a device that was closed between the two (or made
        // by the person, with no song and no description in it): filled, by whichever
        // device looks next.
        let left = |comment: &str, songs: u32| {
            let server = Scripted::bare();
            *server.made.lock().unwrap() = Some(("left".into(), secret::PLAYLIST_NAME.into(), comment.into(), songs));
            server
        };
        let server = left("", 0);
        assert_eq!((looks(&server, None, false), server.asked()), (Looked::Secret { secret: fresh.clone(), id: "left".into(), wrote: Some(Why::Made) }, vec!["list", "fetch", "write", "list", "fetch"]));
        // With songs in it, or a description, it is the person's: nothing is written,
        // nothing is made beside it, and linking does not start.
        for (comment, songs, stop) in [("", 2, Stop::HasSongs), ("mine", 0, Stop::Taken), ("mine", 2, Stop::HasSongs)] {
            let server = left(comment, songs);
            assert_eq!((looks(&server, None, false), server.asked()), (Looked::Stopped(stop), vec!["list", "fetch"]), "{comment:?}, {songs} songs");
        }
        // Two devices set up at the same moment: by the time this one has made and filled
        // its playlist, another's is there too, with the lower id. This one goes by the
        // lower, takes the secret that stands in it, and leaves its own where it is.
        let theirs = Secret::new([4; 32]);
        let mut server = Scripted::with(vec![playlist(&theirs.comment(), false)]);
        server.listed_from = 1;
        assert_eq!(looks(&server, None, false), Looked::Secret { secret: theirs, id: "k".into(), wrote: Some(Why::Made) });
        assert_eq!(server.asked(), ["list", "make", "write", "list", "fetch"]);
        assert_eq!(server.made.lock().unwrap().as_ref().map(|(id, _, comment, _)| (id.clone(), comment.clone())), Some(("made".into(), written.clone())));
        // Nothing is made if no writing of it could ever be seen through: without a secret
        // to put in it, the server is not touched.
        let server = Scripted::bare();
        assert_eq!((look(&server, &account(), None, false, &mut || None, &mut |_| {}), server.asked()), (Looked::NoRandom, vec!["list"]));
        // With no random bytes a record is not replaced either.
        let server = Scripted::with(vec![playlist(RECORD, true)]);
        assert_eq!((look(&server, &account(), Some("k".into()), false, &mut || None, &mut |_| {}), server.asked()), (Looked::NoRandom, vec!["list", "fetch"]));
    }

    /// The server says all is well and what it lists is not a list of playlists (another
    /// program answering in its place, a version that says it another way). That is not
    /// "there are none": nothing is made, and nothing is concluded.
    #[test]
    fn a_list_of_playlists_that_is_not_one_is_an_error_and_never_none() {
        let entry = |with: Value| serde_json::json!({"playlists": {"playlist": [with]}});
        let not_lists = [
            serde_json::json!({}),
            serde_json::json!({"playlists": []}),
            serde_json::json!({"playlists": "none"}),
            serde_json::json!({"playlists": null}),
            serde_json::json!({"playlists": {"playlist": {}}}),
            serde_json::json!({"playlists": {"playlist": "none"}}),
            serde_json::json!({"playlists": {"playlist": null}}),
            serde_json::json!({"playlists": {"playlist": {"id": "k", "name": secret::PLAYLIST_NAME, "owner": "tester"}}}),
            entry(serde_json::json!(7)),
            entry(serde_json::json!("k")),
            entry(serde_json::json!({"name": secret::PLAYLIST_NAME, "owner": "tester"})),
            entry(serde_json::json!({"id": "", "name": secret::PLAYLIST_NAME, "owner": "tester"})),
            entry(serde_json::json!({"id": 7, "name": secret::PLAYLIST_NAME, "owner": "tester"})),
            entry(serde_json::json!({"id": "k", "owner": "tester"})),
            entry(serde_json::json!({"id": "k", "name": 7, "owner": "tester"})),
            entry(serde_json::json!({"id": "k", "name": secret::PLAYLIST_NAME, "owner": 7})),
            entry(serde_json::json!({"id": "k", "name": secret::PLAYLIST_NAME, "owner": "tester", "comment": ["x"]})),
        ];
        for shape in not_lists {
            for used in [None, Some("k")] {
                let mut server = Scripted::bare();
                server.lists_as = Some(shape.clone());
                let mut said = Vec::new();
                let looked = look(&server, &account(), used.map(str::to_string), false, &mut || Some([9; 32]), &mut |line| said.push(line));
                assert_eq!((looked, server.asked()), (Looked::Failed, vec!["list"]), "{shape}");
                assert_eq!(said, ["look: the answer to listing the playlists is not a list of playlists"], "{shape}");
            }
        }
        // What does say there are none: the list left out, as Navidrome leaves it, or empty.
        for none in [serde_json::json!({"playlists": {}}), serde_json::json!({"playlists": {"playlist": []}})] {
            let mut server = Scripted::bare();
            server.lists_as = Some(none.clone());
            server.signed_out_after = Some(2);
            assert_eq!((looks(&server, None, false), server.asked()), (Looked::Failed, vec!["list", "make"]), "{none}");
        }
        // And a list with other playlists in it, each with what a playlist has, is read:
        // what an entry leaves out (its owner, a comment) is left out, not an error.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.lists_as = Some(serde_json::json!({"playlists": {"playlist": [
            {"id": "a", "name": "Morning mix", "owner": "tester", "songCount": 8, "public": true},
            {"id": "b", "name": "Somebody's", "comment": ""},
            {"id": "k", "name": secret::PLAYLIST_NAME, "owner": "tester"},
        ]}}));
        assert!(matches!(looks(&server, Some("k"), false), Looked::Secret { wrote: None, .. }));
    }

    /// Every request of every kind of look that is not a write, going wrong in every way a
    /// request can; and each write with the sign-in ending under it. The look makes no
    /// other request, and concludes only what it has seen: before the record has been
    /// seen to be no good, a failure concludes nothing (what is held runs out in its own
    /// time); after, what is held is not to be used. (A write that is not answered is
    /// another matter, and has a test of its own.)
    #[test]
    fn every_request_of_a_look_failing_in_every_way() {
        let (_, written) = fresh();
        // Each kind of look: the playlist used before, whether a reset was asked for, what
        // each fetch finds, whether a playlist of the name is listed, the requests it makes
        // when all goes well, and the first of them by which the record has been seen to be
        // no good.
        struct Kind {
            what: &'static str,
            used: Option<&'static str>,
            reset: bool,
            fetches: Vec<Option<String>>,
            listed: bool,
            asks: &'static [&'static str],
            no_good_from: usize,
        }
        let kinds = [
            Kind { what: "the record is read", used: Some("k"), reset: false, fetches: vec![playlist(RECORD, false)], listed: true, asks: &["list", "fetch"], no_good_from: usize::MAX },
            Kind { what: "a public record is replaced", used: Some("k"), reset: false, fetches: vec![playlist(RECORD, true), playlist(RECORD, false)], listed: true, asks: &["list", "fetch", "write", "list", "fetch"], no_good_from: 2 },
            Kind { what: "there is no playlist yet", used: None, reset: false, fetches: vec![], listed: false, asks: &["list", "make", "write", "list", "fetch"], no_good_from: usize::MAX },
            Kind { what: "the playlist that was used has gone", used: Some("gone"), reset: false, fetches: vec![], listed: false, asks: &["list", "make", "write", "list", "fetch"], no_good_from: 1 },
            Kind { what: "the secret is reset", used: Some("k"), reset: true, fetches: vec![playlist(RECORD, false), playlist(&written, false)], listed: true, asks: &["list", "fetch", "write", "list", "fetch"], no_good_from: 3 },
        ];
        for kind in &kinds {
            let server = |fails| {
                let mut server = if kind.listed { Scripted::with(kind.fetches.clone()) } else { Scripted::bare() };
                server.fails = fails;
                server
            };
            // With nothing going wrong it asks what it should.
            let well = server(None);
            assert!(matches!(looks(&well, kind.used, kind.reset), Looked::Secret { .. }), "{}", kind.what);
            assert_eq!(well.asked(), kind.asks, "{}", kind.what);
            for at in 0..kind.asks.len() {
                for how in [Fail::NoAnswer, Fail::Lost, Fail::NotTheServer, Fail::NotOkStatus, Fail::NotOk, Fail::SignedOut] {
                    if kind.asks[at] == "write" && how != Fail::SignedOut {
                        continue;
                    }
                    let server = server(Some((at, how)));
                    let looked = looks(&server, kind.used, kind.reset);
                    let case = format!("{}: its {} (request {at}) {how:?}", kind.what, kind.asks[at]);
                    // That request is the last.
                    assert_eq!(server.asked(), &kind.asks[..=at], "{case}");
                    // A sign-in that has ended concludes nothing, whatever was seen: there
                    // is nobody left to conclude it for.
                    let expected = if how != Fail::SignedOut && at >= kind.no_good_from { Looked::Invalid } else { Looked::Failed };
                    assert_eq!(looked, expected, "{case}");
                    assert_eq!(ends_session(&looked), expected == Looked::Invalid, "{case}");
                }
            }
        }
    }

    /// What a look finds that is not a failure of any request, and is not a secret either.
    #[test]
    fn every_record_a_look_will_not_use_or_write_over() {
        let of = |inside: &str| Some(format!(r#"<subsonic-response status="ok"><playlist {inside}></playlist></subsonic-response>"#));
        let cases = [
            ("the server does not say whether it is public", of(r#"id="k" name="Nexus Music link" comment="" songCount="0" owner="tester""#), Stop::NotStated),
            ("the server does not say whose it is", of(r#"id="k" name="Nexus Music link" comment="" songCount="0" public="false""#), Stop::NotStated),
            ("the server does not say how many songs it has", of(r#"id="k" name="Nexus Music link" comment="" public="false" owner="tester""#), Stop::NotStated),
            ("the answer is about another playlist", of(r#"id="j" name="Nexus Music link" comment="" songCount="0" public="false" owner="tester""#), Stop::Mismatch),
            ("it has been renamed", of(r#"id="k" name="Road trip" comment="" songCount="0" public="false" owner="tester""#), Stop::Mismatch),
            ("it is somebody else's", of(r#"id="k" name="Nexus Music link" comment="" songCount="0" public="false" owner="other""#), Stop::Mismatch),
            ("it has songs in it", of(r#"id="k" name="Nexus Music link" comment="" songCount="2" public="false" owner="tester""#), Stop::HasSongs),
            ("its description is the person's own", playlist("Links to things", false), Stop::Taken),
            ("its description only looks like a record", playlist(r#"{"nexus_music_link":1,"secret":"x"}"#, false), Stop::Taken),
            ("its record is in a later layout", playlist(r#"{"nexus_music_link":2,"secret":"x"}"#, false), Stop::Later),
        ];
        for (what, fetched, stop) in cases {
            for (reset, used) in [(false, Some("k")), (true, Some("k")), (false, Some("gone")), (false, None)] {
                let server = Scripted::with(vec![fetched.clone()]);
                let looked = looks(&server, used, reset);
                assert_eq!((&looked, server.asked()), (&Looked::Stopped(stop), vec!["list", "fetch"]), "{what}, reset {reset}, used {used:?}");
                assert!(ends_session(&looked), "{what}");
            }
        }
        // A list that does not say whose each playlist is: nothing is fetched at all.
        let mut server = Scripted::with(vec![]);
        server.listed = vec![serde_json::json!({"id": "k", "name": secret::PLAYLIST_NAME})];
        assert_eq!((looks(&server, None, false), server.asked()), (Looked::Stopped(Stop::NotStated), vec!["list"]));
        // What was written keeps not being there: after three rounds it gives up, and what
        // is held is not to be used.
        let server = Scripted::with(vec![playlist(RECORD, true); 3]);
        assert_eq!((looks(&server, Some("k"), false), server.asked().len()), (Looked::Invalid, 9));
    }
}
