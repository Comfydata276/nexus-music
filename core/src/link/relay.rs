//! Keeping a device's place at the relay, and sending through it: the route's calls
//! (`docs/linking-design.md`, 5.3), made through the app's [`Http`].
//!
//! [`keep_place`] is the whole of a device's stay in a room: it checks that the relay has
//! the route, enters under a new place, collects what is put in its mailbox, enters again
//! (under another new place) if the relay has forgotten it, and leaves when it is told to
//! stop. It runs for as long as the session lasts, on a thread the app gives it, and is
//! stopped the way everything here is: the app's `Http` says [`Failure::Stopped`] to a
//! wait. It owns no thread, no clock and no connection.
//!
//! The host key and a place's token go to the relay's own address and nowhere else: the
//! app's `Http` for this must follow no redirect.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Place, VERSION, text, words};
use crate::http::{Failure, Http, Method, Request};

/// The relay, as the app holds it: its address with no `/` at the end, and the host key.
/// Not shown (`{:?}`) with its key.
#[derive(Clone)]
pub struct Relay {
    pub base: String,
    pub key: String,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Relay({})", self.base)
    }
}

/// What keeping a place has to say to the device.
#[derive(Debug, Clone, PartialEq)]
pub enum FromRelay {
    /// Something for the person, under the list: why no place can be had.
    Says(&'static str),
    /// The system gave no random bytes for a place: linking is off.
    NoRandom,
    /// This device has a place in the room, and the token for it.
    Entered { place: Place, token: String },
    /// Who is in the room, and what was in this device's mailbox: who each frame is from,
    /// and the frame. A frame handed over once is not handed over again.
    Heard { places: Vec<String>, frames: Vec<(String, String)> },
    /// The relay has forgotten this place (it restarted, or the device slept). Another
    /// will be entered under.
    Left,
}

const HELLO: Duration = Duration::from_secs(15);
/// A poll is held up to twenty seconds at the relay.
const POLL: Duration = Duration::from_secs(40);
const POST: Duration = Duration::from_secs(20);

/// One call. `Ok(None)`: no answer. `Err`: stop.
fn call(http: &dyn Http, relay: &Relay, method: Method, path: &str, token: &str, patience: Duration) -> Result<Option<(u16, Value)>, ()> {
    let url = format!("{}/{path}", relay.base);
    let bearer = format!("Bearer {}", relay.key);
    let mut headers = vec![("Authorization", bearer.as_str()), ("X-Nexus-Link", token)];
    if !matches!(method, Method::Get) {
        headers.push(("Content-Type", "application/json"));
    }
    match http.send(&Request { method, url: &url, headers: &headers, patience: Some(patience) }) {
        Ok(response) => {
            let status = response.status;
            Ok(Some((status, response.json().unwrap_or(Value::Null))))
        }
        Err(Failure::Reach(_)) => Ok(None),
        Err(Failure::Stopped) => Err(()),
    }
}

/// Sends a sealed frame to a place. The relay's status, if it answered. Sent once:
/// whatever comes of it, it is not sent again by itself.
pub fn send(http: &dyn Http, relay: &Relay, room: &str, token: &str, to: &Place, frame: &str) -> Option<u16> {
    let body = json!({"to": text::place(to), "frame": frame}).to_string();
    call(http, relay, Method::Post(&body), &format!("api/link/{room}/send"), token, POST).ok().flatten().map(|(status, _)| status)
}

/// Keeps this device's place in `room` until the app's `Http` says to stop. `random` gives
/// sixteen bytes from the system's source of randomness for each new place, or `None`.
/// `tell` is told what happens; `log` what is worth writing down, in words that name no
/// key and no token. Trying again is spaced out, up to a minute.
pub fn keep_place(http: &dyn Http, relay: &Relay, room: &str, random: &mut dyn FnMut() -> Option<Place>, tell: &mut dyn FnMut(FromRelay), log: &mut dyn FnMut(String)) {
    // How long to leave it before trying again: longer each time, up to a minute.
    let mut wait = 2u64;
    macro_rules! pause {
        ($seconds:expr) => {
            if http.pause(Duration::from_secs($seconds)).is_err() {
                return;
            }
        };
    }
    macro_rules! again {
        ($entering:lifetime) => {{
            pause!(wait);
            wait = (wait * 2).min(60);
            continue $entering;
        }};
    }
    'entering: loop {
        if http.pause(Duration::ZERO).is_err() {
            return;
        }
        match call(http, relay, Method::Get, "api/hello", "", HELLO) {
            Err(()) => return,
            Ok(Some((200, hello))) if hello["link"].as_u64() == Some(VERSION) && hello["host"].as_bool() == Some(true) => {}
            Ok(Some((200, _))) => {
                tell(FromRelay::Says(words::RELAY_NEEDS_UPDATING));
                pause!(60);
                continue;
            }
            Ok(Some((401, _))) => {
                tell(FromRelay::Says(words::HOST_LINK_WRONG));
                pause!(60);
                continue;
            }
            Ok(_) => again!('entering),
        }
        let Some(place) = random() else { return tell(FromRelay::NoRandom) };
        let name = text::place(&place);
        let (token, mut version) = match call(http, relay, Method::Post(&json!({"place": name}).to_string()), &format!("api/link/{room}/enter"), "", POST) {
            Err(()) => return,
            Ok(Some((200, entered))) => {
                let Some(token) = entered["token"].as_str().map(str::to_string) else { continue };
                (token, entered["v"].as_u64().unwrap_or(0))
            }
            // (A place that is taken: so unlikely that it is simply tried again with another.)
            Ok(Some((409, _))) => continue,
            Ok(_) => again!('entering),
        };
        wait = 2;
        log(format!("room: entered as {name}"));
        tell(FromRelay::Entered { place, token: token.clone() });
        // The last frame handed over. A new token counts from 1 again.
        let mut after = 0u64;
        loop {
            let answer = call(http, relay, Method::Get, &format!("api/link/{room}/poll?v={version}&after={after}"), &token, POLL);
            // Told to stop while the poll was held: the place is given up, not left to lapse.
            if answer.is_err() || http.pause(Duration::ZERO).is_err() {
                let _ = http.send(&Request { method: Method::Post("{}"), url: &format!("{}/api/link/{room}/leave", relay.base), headers: &[("Authorization", &format!("Bearer {}", relay.key)), ("X-Nexus-Link", &token), ("Content-Type", "application/json")], patience: Some(POST) });
                return;
            }
            match answer.ok().flatten() {
                Some((200, heard)) => {
                    version = heard["v"].as_u64().unwrap_or(version);
                    let places: Vec<String> = heard["places"].as_array().into_iter().flatten().filter_map(|place| place.as_str().map(str::to_string)).collect();
                    let mut frames = Vec::new();
                    for frame in heard["frames"].as_array().into_iter().flatten() {
                        // One that was handed over before is sent again until it is said
                        // to have been dealt with: it is skipped.
                        let (Some(seq), Some(from), Some(sealed)) = (frame["seq"].as_u64(), frame["from"].as_str(), frame["frame"].as_str()) else { continue };
                        if seq > after {
                            after = seq;
                            frames.push((from.to_string(), sealed.to_string()));
                        }
                    }
                    tell(FromRelay::Heard { places, frames });
                }
                // The relay has forgotten this place (it restarted, or the device slept).
                Some((404, _)) => {
                    log("room: the relay has forgotten this place".to_string());
                    tell(FromRelay::Left);
                    pause!(1);
                    continue 'entering;
                }
                Some((401, _)) => pause!(30),
                Some((429 | 503, _)) => pause!(2),
                _ => pause!(5),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::Response;

    /// A relay that answers from a script, one answer for each request in turn (`None`: no
    /// answer), and says to stop once the script has run out.
    struct Scripted {
        answers: Mutex<Vec<Option<(u16, Value)>>>,
        asked: Mutex<Vec<String>>,
        waits: Mutex<Vec<u64>>,
    }

    impl Scripted {
        fn with(answers: Vec<Option<(u16, Value)>>) -> Scripted {
            Scripted { answers: Mutex::new(answers), asked: Mutex::default(), waits: Mutex::default() }
        }

        fn over(&self) -> bool {
            self.answers.lock().unwrap().is_empty()
        }
    }

    impl Http for Scripted {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let method = if matches!(request.method, Method::Get) { "GET" } else { "POST" };
            let path = request.url.strip_prefix("http://relay.example/").expect("the relay's own address");
            assert!(request.headers.contains(&("Authorization", "Bearer the-key")), "the host key goes with every call");
            self.asked.lock().unwrap().push(format!("{method} {path} [{}]", request.headers.iter().find(|(name, _)| *name == "X-Nexus-Link").map_or("", |(_, token)| token)));
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                // (The leave, as the stay ends.)
                return Ok(Response::new(200, Vec::new(), || Ok("{\"ok\":true}".into())));
            }
            match answers.remove(0) {
                Some((status, body)) => Ok(Response::new(status, Vec::new(), move || Ok(body.to_string()))),
                None => Err(Failure::Reach("no answer".into())),
            }
        }

        fn pause(&self, wait: Duration) -> Result<(), Failure> {
            if self.over() {
                return Err(Failure::Stopped);
            }
            if !wait.is_zero() {
                self.waits.lock().unwrap().push(wait.as_secs());
            }
            Ok(())
        }
    }

    fn relay() -> Relay {
        Relay { base: "http://relay.example".into(), key: "the-key".into() }
    }

    fn stay(script: Vec<Option<(u16, Value)>>, places: &mut dyn FnMut() -> Option<Place>) -> (Vec<FromRelay>, Vec<String>, Vec<u64>, Vec<String>) {
        let http = Scripted::with(script);
        let (mut told, mut logged) = (Vec::new(), Vec::new());
        keep_place(&http, &relay(), "theroom", places, &mut |from| told.push(from), &mut |line| logged.push(line));
        let (asked, waits) = (http.asked.lock().unwrap().clone(), http.waits.lock().unwrap().clone());
        (told, asked, waits, logged)
    }

    fn hello() -> Option<(u16, Value)> {
        Some((200, json!({"nexus_jam": 1, "link": 4, "host": true})))
    }

    #[test]
    fn a_device_enters_collects_its_mailbox_and_leaves_when_told_to_stop() {
        let mut n = 0u8;
        let mut places = || {
            n += 1;
            Some([n; 16])
        };
        let heard = json!({"v": 7, "places": ["aaaa", "bbbb"], "frames": [{"seq": 1, "from": "bbbb", "frame": "one"}, {"seq": 2, "from": "bbbb", "frame": "two"}]});
        // The relay sends frame 2 again with frame 3: it was not yet said to have been dealt with.
        let again = json!({"v": 7, "places": ["aaaa", "bbbb"], "frames": [{"seq": 2, "from": "bbbb", "frame": "two"}, {"seq": 3, "from": "bbbb", "frame": "three"}, {"from": "bbbb", "frame": "no number"}]});
        // (The last answer is one nothing is made of: the stay is told to stop as it arrives.)
        let (told, asked, waits, logged) = stay(vec![hello(), Some((200, json!({"token": "the-token", "v": 5, "places": []}))), Some((200, heard)), Some((200, again)), Some((503, json!({})))], &mut places);
        let place = text::place(&[1; 16]);
        assert_eq!(told[0], FromRelay::Entered { place: [1; 16], token: "the-token".into() });
        assert_eq!(told[1], FromRelay::Heard { places: vec!["aaaa".into(), "bbbb".into()], frames: vec![("bbbb".into(), "one".into()), ("bbbb".into(), "two".into())] });
        assert_eq!(told[2], FromRelay::Heard { places: vec!["aaaa".into(), "bbbb".into()], frames: vec![("bbbb".into(), "three".into())] });
        assert_eq!(
            asked,
            ["GET api/hello []", "POST api/link/theroom/enter []", "GET api/link/theroom/poll?v=5&after=0 [the-token]", "GET api/link/theroom/poll?v=7&after=2 [the-token]", "GET api/link/theroom/poll?v=7&after=3 [the-token]", "POST api/link/theroom/leave [the-token]"]
        );
        assert_eq!((waits, logged), (vec![], vec![format!("room: entered as {place}")]));
        assert_eq!(format!("{:?}", relay()), "Relay(http://relay.example)");
    }

    #[test]
    fn a_place_the_relay_has_forgotten_is_entered_again_under_another() {
        let mut n = 0u8;
        let mut places = || {
            n += 1;
            Some([n; 16])
        };
        let entered = |token: &str| Some((200, json!({"token": token, "v": 1})));
        let (told, asked, waits, _) = stay(vec![hello(), entered("first"), Some((404, json!({"error": "Enter the room first."}))), hello(), entered("second"), Some((200, json!({"v": 2, "places": [], "frames": []})))], &mut places);
        assert_eq!(told[0], FromRelay::Entered { place: [1; 16], token: "first".into() });
        assert_eq!(told[1], FromRelay::Left);
        assert_eq!(told[2], FromRelay::Entered { place: [2; 16], token: "second".into() });
        assert_eq!(waits, [1]);
        // The second stay counts its frames from the start again.
        assert!(asked.contains(&"GET api/link/theroom/poll?v=1&after=0 [second]".to_string()), "{asked:?}");
    }

    #[test]
    fn a_relay_without_the_route_or_with_another_key_is_said_and_asked_again_in_a_minute() {
        let mut places = || Some([1; 16]);
        let old = Some((200, json!({"nexus_jam": 1, "host": true})));
        let (told, asked, waits, _) = stay(vec![old.clone(), Some((401, json!({}))), Some((200, json!({"nexus_jam": 1, "link": 4})))], &mut places);
        assert_eq!(told, [FromRelay::Says(words::RELAY_NEEDS_UPDATING), FromRelay::Says(words::HOST_LINK_WRONG), FromRelay::Says(words::RELAY_NEEDS_UPDATING)]);
        assert_eq!((asked.len(), waits), (3, vec![60, 60]));
        // One that does not answer is tried again, longer apart each time, up to a minute;
        // and so is an enter that is refused for being busy.
        let (told, _, waits, _) = stay(vec![None, None, hello(), Some((503, json!({}))), None, None, None, None], &mut places);
        assert_eq!((told, waits), (vec![], vec![2, 4, 8, 16, 32, 60]));
        // A place that is taken is tried again at once with another, and a poll that is
        // refused waits its turn.
        let mut n = 0u8;
        let mut places = || {
            n += 1;
            Some([n; 16])
        };
        let (told, _, waits, _) = stay(vec![hello(), Some((409, json!({}))), hello(), Some((200, json!({"token": "t", "v": 0}))), Some((429, json!({}))), Some((401, json!({}))), None], &mut places);
        assert_eq!((told, waits), (vec![FromRelay::Entered { place: [2; 16], token: "t".into() }], vec![2, 30]));
    }

    #[test]
    fn with_no_random_bytes_for_a_place_nothing_is_entered() {
        let (told, asked, _, _) = stay(vec![hello(), None], &mut || None);
        assert_eq!((told, asked), (vec![FromRelay::NoRandom], vec!["GET api/hello []".to_string()]));
    }

    #[test]
    fn a_frame_is_sent_once_to_the_place_it_is_for() {
        let http = Scripted::with(vec![Some((200, json!({"ok": true}))), Some((404, json!({}))), None]);
        assert_eq!(send(&http, &relay(), "theroom", "the-token", &[3; 16], "the-frame"), Some(200));
        assert_eq!(send(&http, &relay(), "theroom", "the-token", &[3; 16], "the-frame"), Some(404));
        assert_eq!(send(&http, &relay(), "theroom", "the-token", &[3; 16], "the-frame"), None);
        assert_eq!(http.asked.lock().unwrap().len(), 3);
        assert_eq!(http.asked.lock().unwrap()[0], "POST api/link/theroom/send [the-token]");
    }
}
