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

use super::{Place, Token, VERSION, text, words};
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
    Entered { place: Place, token: Token },
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

/// The most places taken from one answer of the relay's: this device's own, and one more
/// than the fifteen others a device keeps (`super::device`), so that its own place being
/// among them never costs it one of those.
pub const MOST_PLACES: usize = 17;
/// The most frames taken from one answer of the relay's. (A device answers twenty asks a
/// second; what is over is the relay's to hand over again, or is lost with its sender told.)
pub const MOST_FRAMES: usize = 64;
/// The longest a place's token can be, in characters.
pub const MOST_TOKEN: usize = 256;
/// The most text one hand-over holds, the places its frames are from and the frames: half
/// of what may wait for the player's thread (`super::turn::MOST_KEPT_BYTES`), so that a
/// hand-over fits there whenever little else is waiting. The longest frame there can be
/// is less.
pub const MOST_HEARD_BYTES: usize = super::turn::MOST_KEPT_BYTES / 2;
const _: () = assert!(super::frame::MOST_FRAME_TEXT + 64 <= MOST_HEARD_BYTES);

/// What is taken from one answer of the relay's: chosen where it lies in the answer,
/// before any of it is copied.
struct Chosen<'a> {
    places: Vec<&'a str>,
    /// Who each frame is from, and the frame.
    frames: Vec<(&'a str, &'a str)>,
    /// The number of the last frame this deals with: asked for from there next, once
    /// what was chosen has been taken.
    upto: u64,
}

/// Chooses from an answer to a poll. `after` is the number of the last frame dealt with.
fn chosen(heard: &Value, after: u64) -> Chosen<'_> {
    let places = heard["places"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|place| text::read_place(place).is_some()).take(MOST_PLACES).collect();
    let (mut frames, mut upto, mut bytes) = (Vec::new(), after, 0usize);
    for frame in heard["frames"].as_array().into_iter().flatten() {
        let (Some(seq), Some(from), Some(sealed)) = (frame["seq"].as_u64(), frame["from"].as_str(), frame["frame"].as_str()) else { continue };
        // One that was handed over before is sent again until it is said to have been
        // dealt with: it is skipped.
        if seq <= upto {
            continue;
        }
        // Only frames from a place, and no longer than a frame can be. (One that is not
        // is dealt with by being passed over: it is not asked for again.)
        if text::read_place(from).is_some() && !sealed.is_empty() && sealed.len() <= super::frame::MOST_FRAME_TEXT {
            // No more of them than one hand-over holds, by number or by size: this one
            // and what follows it are left with the relay, and asked for from here.
            let size = from.len() + sealed.len();
            if frames.len() >= MOST_FRAMES || bytes + size > MOST_HEARD_BYTES {
                break;
            }
            bytes += size;
            frames.push((from, sealed));
        }
        upto = seq;
    }
    Chosen { places, frames, upto }
}

/// Sends a sealed frame to a place. The relay's status, if it answered. Sent once:
/// whatever comes of it, it is not sent again by itself.
pub fn send(http: &dyn Http, relay: &Relay, room: &str, token: &str, to: &Place, frame: &str) -> Option<u16> {
    let body = json!({"to": text::place(to), "frame": frame}).to_string();
    call(http, relay, Method::Post(&body), &format!("api/link/{room}/send"), token, POST).ok().flatten().map(|(status, _)| status)
}

/// Keeps this device's place in `room` until the app's `Http` says to stop. `random` gives
/// sixteen bytes from the system's source of randomness for each new place, or `None`.
/// `tell` is told what happens, **and says whether it took what it was told**: whoever
/// is told may keep only so much waiting, and what it does not take is not lost. A place
/// entered or forgotten is said again until it is taken, and frames not taken are not
/// counted as handed over: the relay is asked for the same ones again. `log` is told what
/// is worth writing down, in words that name no key and no token. Trying again is spaced
/// out, up to a minute.
pub fn keep_place(http: &dyn Http, relay: &Relay, room: &str, random: &mut dyn FnMut() -> Option<Place>, tell: &mut dyn FnMut(FromRelay) -> bool, log: &mut dyn FnMut(String)) {
    // How long to leave it before trying again: longer each time, up to a minute.
    let mut wait = 2u64;
    macro_rules! pause {
        ($seconds:expr) => {
            if http.pause(Duration::from_secs($seconds)).is_err() {
                return;
            }
        };
    }
    // What must be heard is said until it is taken: a second is left between. (What is
    // only for the person, and is said again in a minute anyway, is said once.)
    macro_rules! told {
        ($from:expr) => {
            while !tell($from) {
                pause!(1);
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
        let Some(place) = random() else {
            told!(FromRelay::NoRandom);
            return;
        };
        let name = text::place(&place);
        let (token, mut version) = match call(http, relay, Method::Post(&json!({"place": name}).to_string()), &format!("api/link/{room}/enter"), "", POST) {
            Err(()) => return,
            Ok(Some((200, entered))) => {
                // (A token is a short thing: what is not is not one.)
                let Some(token) = entered["token"].as_str().filter(|token| token.len() <= MOST_TOKEN).map(str::to_string) else { continue };
                (token, entered["v"].as_u64().unwrap_or(0))
            }
            // (A place that is taken: so unlikely that it is simply tried again with another.)
            Ok(Some((409, _))) => continue,
            Ok(_) => again!('entering),
        };
        wait = 2;
        log(format!("room: entered as {name}"));
        told!(FromRelay::Entered { place, token: token.as_str().into() });
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
                    // What the relay says is looked at before any of it is taken: only
                    // what reads as a place, and no more places than a room is given
                    // here; only frames from a place, no longer than a frame can be, and
                    // no more of them than one hand-over holds.
                    let chosen = chosen(&heard, after);
                    let heard = FromRelay::Heard { places: chosen.places.iter().map(|place| place.to_string()).collect(), frames: chosen.frames.iter().map(|(from, frame)| (from.to_string(), frame.to_string())).collect() };
                    // **Counted as handed over only once it has been taken.** If there is
                    // no room for it just now, the same frames are asked for again.
                    if tell(heard) {
                        after = chosen.upto;
                    } else {
                        pause!(1);
                    }
                }
                // The relay has forgotten this place (it restarted, or the device slept).
                Some((404, _)) => {
                    log("room: the relay has forgotten this place".to_string());
                    told!(FromRelay::Left);
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
        stay_taking(script, places, &mut |_| true)
    }

    /// The same, where whoever is told takes what it is told or does not (`takes`): what
    /// it did not take is not among what was told.
    fn stay_taking(script: Vec<Option<(u16, Value)>>, places: &mut dyn FnMut() -> Option<Place>, takes: &mut dyn FnMut(&FromRelay) -> bool) -> (Vec<FromRelay>, Vec<String>, Vec<u64>, Vec<String>) {
        let http = Scripted::with(script);
        let (mut told, mut logged) = (Vec::new(), Vec::new());
        let mut tell = |from: FromRelay| {
            let taken = takes(&from);
            if taken {
                told.push(from);
            }
            taken
        };
        keep_place(&http, &relay(), "theroom", places, &mut tell, &mut |line| logged.push(line));
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
        let (a, b) = (text::place(&[0xa; 16]), text::place(&[0xb; 16]));
        let heard = json!({"v": 7, "places": [a, b], "frames": [{"seq": 1, "from": b, "frame": "one"}, {"seq": 2, "from": b, "frame": "two"}]});
        // The relay sends frame 2 again with frame 3: it was not yet said to have been dealt with.
        let again = json!({"v": 7, "places": [a, b], "frames": [{"seq": 2, "from": b, "frame": "two"}, {"seq": 3, "from": b, "frame": "three"}, {"from": b, "frame": "no number"}]});
        // (The last answer is one nothing is made of: the stay is told to stop as it arrives.)
        let (told, asked, waits, logged) = stay(vec![hello(), Some((200, json!({"token": "the-token", "v": 5, "places": []}))), Some((200, heard)), Some((200, again)), Some((503, json!({})))], &mut places);
        let place = text::place(&[1; 16]);
        assert_eq!(told[0], FromRelay::Entered { place: [1; 16], token: "the-token".into() });
        assert_eq!(told[1], FromRelay::Heard { places: vec![a.clone(), b.clone()], frames: vec![(b.clone(), "one".into()), (b.clone(), "two".into())] });
        assert_eq!(told[2], FromRelay::Heard { places: vec![a.clone(), b.clone()], frames: vec![(b.clone(), "three".into())] });
        assert_eq!(
            asked,
            ["GET api/hello []", "POST api/link/theroom/enter []", "GET api/link/theroom/poll?v=5&after=0 [the-token]", "GET api/link/theroom/poll?v=7&after=2 [the-token]", "GET api/link/theroom/poll?v=7&after=3 [the-token]", "POST api/link/theroom/leave [the-token]"]
        );
        assert_eq!((waits, logged), (vec![], vec![format!("room: entered as {place}")]));
        assert_eq!(format!("{:?}", relay()), "Relay(http://relay.example)");
    }

    /// Whatever the relay answers, only what could be a room and its frames is taken from
    /// it, and no more than a room and a hand-over are given: nothing else is copied.
    #[test]
    fn what_the_relay_hands_over_is_checked_and_bounded_before_any_of_it_is_taken() {
        let place = |n: usize| text::place(&[n as u8; 16]);
        let mut places: Vec<Value> = vec![json!(7), json!("not a place"), json!(""), json!("x".repeat(1_000_000)), json!(null)];
        places.extend((0..40).map(|n| json!(place(n))));
        let mut frames = vec![
            json!({"seq": 1, "from": "nobody", "frame": "from no place"}),
            json!({"seq": 2, "from": place(1), "frame": ""}),
            json!({"seq": 3, "from": place(1), "frame": "x".repeat(super::super::frame::MOST_FRAME_TEXT + 1)}),
            json!({"seq": 4, "from": place(1), "frame": 7}),
            json!({"seq": 5, "from": place(1), "frame": "x".repeat(super::super::frame::MOST_FRAME_TEXT)}),
        ];
        frames.extend((6..206).map(|seq| json!({"seq": seq, "from": place(2), "frame": "f"})));
        let mut n = 0u8;
        let mut places_given = || {
            n += 1;
            Some([n; 16])
        };
        let (told, asked, ..) = stay(vec![hello(), Some((200, json!({"token": "the-token", "v": 5}))), Some((200, json!({"v": 7, "places": places, "frames": frames}))), Some((503, json!({})))], &mut places_given);
        let FromRelay::Heard { places, frames } = &told[1] else { panic!("{told:?}") };
        assert_eq!((places.len(), places[0].as_str(), places.iter().all(|place| text::read_place(place).is_some())), (MOST_PLACES, place(0).as_str(), true));
        assert_eq!((frames.len(), frames[0].1.len(), frames[1].clone()), (MOST_FRAMES, super::super::frame::MOST_FRAME_TEXT, (place(2), "f".to_string())));
        // What one hand-over holds counts as handed over, with what was passed over on
        // the way; the rest is asked for from there.
        assert!(asked.iter().any(|asked| asked.starts_with(&format!("GET api/link/theroom/poll?v=7&after={} ", 5 + MOST_FRAMES - 1))), "{asked:?}");
        // And an answer that is not of that shape at all gives an empty room, not an error.
        let (told, ..) = stay(vec![hello(), Some((200, json!({"token": "the-token", "v": 5}))), Some((200, json!({"v": "x", "places": "all of them", "frames": {"seq": 1}}))), Some((503, json!({})))], &mut places_given);
        assert_eq!(told[1], FromRelay::Heard { places: vec![], frames: vec![] });
    }

    /// Whoever is told may have no room for what it is told just now (the player's thread
    /// is busy, and what waits for it is bounded). **Nothing is lost by that**: frames not
    /// taken are not counted as handed over, and are asked for again; a place that was
    /// entered is said again until it is heard.
    #[test]
    fn what_was_not_taken_is_offered_again_and_frames_are_not_counted_as_handed_over() {
        let mut n = 0u8;
        let mut places = || {
            n += 1;
            Some([n; 16])
        };
        let b = text::place(&[0xb; 16]);
        let heard = || Some((200, json!({"v": 7, "places": [b], "frames": [{"seq": 1, "from": b, "frame": "one"}, {"seq": 2, "from": b, "frame": "two"}]})));
        // The place is not taken the first time it is said, and the frames not the first twice.
        let mut refusals = vec![("entered", 1), ("heard", 2)];
        let mut takes = |from: &FromRelay| {
            let kind = match from {
                FromRelay::Entered { .. } => "entered",
                FromRelay::Heard { .. } => "heard",
                _ => "other",
            };
            match refusals.iter_mut().find(|(of, left)| *of == kind && *left > 0) {
                Some((_, left)) => {
                    *left -= 1;
                    false
                }
                None => true,
            }
        };
        let script = vec![hello(), Some((200, json!({"token": "the-token", "v": 5}))), heard(), heard(), heard(), Some((200, json!({"v": 7, "places": [b], "frames": []}))), Some((503, json!({})))];
        let (told, asked, waits, _) = stay_taking(script, &mut places, &mut takes);
        assert_eq!(told[0], FromRelay::Entered { place: [1; 16], token: "the-token".into() });
        assert_eq!(told[1], FromRelay::Heard { places: vec![b.clone()], frames: vec![(b.clone(), "one".into()), (b.clone(), "two".into())] });
        assert_eq!(told[2], FromRelay::Heard { places: vec![b.clone()], frames: vec![] });
        let polls: Vec<&str> = asked.iter().filter(|asked| asked.contains("/poll?")).map(|asked| asked.split("&after=").nth(1).unwrap_or_default().split(' ').next().unwrap_or_default()).collect();
        assert_eq!(polls, ["0", "0", "0", "2", "2"], "{asked:?}");
        // (A second is left before each is offered again.)
        assert_eq!(waits, [1, 1, 1]);
    }

    /// One hand-over holds so many frames and so much text, and no more: what is over is
    /// left with the relay, not counted as handed over, and comes at the next asking.
    /// And it is chosen from the relay's answer where it lies, before any of it is copied.
    #[test]
    fn a_hand_over_is_cut_to_size_and_what_is_over_is_left_with_the_relay() {
        let place = |n: usize| text::place(&[n as u8; 16]);
        let longest = "x".repeat(super::super::frame::MOST_FRAME_TEXT);
        let frames: Vec<Value> = (1..=6).map(|seq| json!({"seq": seq, "from": place(2), "frame": longest})).collect();
        let heard = json!({"v": 7, "places": [place(1), place(2)], "frames": frames});
        let chosen = chosen(&heard, 0);
        let bytes: usize = chosen.frames.iter().map(|(from, frame)| from.len() + frame.len()).sum();
        assert!(!chosen.frames.is_empty() && chosen.frames.len() < 6 && bytes <= MOST_HEARD_BYTES, "{} frames, {bytes} bytes", chosen.frames.len());
        assert_eq!(chosen.upto, chosen.frames.len() as u64);
        // (Chosen where it lies: what is chosen is the relay's own text, not a copy of it.)
        assert!(std::ptr::eq(chosen.frames[0].1.as_ptr(), heard["frames"][0]["frame"].as_str().unwrap().as_ptr()));
        // The rest comes when asked for from there; and a frame that is not one is passed
        // over and counted, so that it is not asked for again for ever.
        let rest = super::chosen(&heard, chosen.upto);
        assert_eq!(rest.frames.len(), chosen.frames.len().min(6 - chosen.frames.len()));
        let odd = json!({"frames": [{"seq": 1, "from": "nobody", "frame": "x"}, {"seq": 2, "from": place(2), "frame": ""}, {"seq": 3, "from": place(2), "frame": "f"}, {"seq": 2, "from": place(2), "frame": "again"}]});
        let chosen = super::chosen(&odd, 0);
        assert_eq!((chosen.frames.len(), chosen.frames[0].1, chosen.upto, chosen.places.len()), (1, "f", 3, 0));
        // More frames than one hand-over holds: the first so many, and the cursor at the last of them.
        let many: Vec<Value> = (1..=200).map(|seq| json!({"seq": seq, "from": place(2), "frame": "f"})).collect();
        let many = json!({"frames": many});
        let chosen = super::chosen(&many, 0);
        assert_eq!((chosen.frames.len(), chosen.upto), (MOST_FRAMES, MOST_FRAMES as u64));
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
