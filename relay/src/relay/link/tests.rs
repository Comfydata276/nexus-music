use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiny_http::Server;

use super::super::{Config, Relay, start};
use super::*;

const KEY: &str = "a-host-key-for-the-tests";
const ROOM: &str = "gmpsssbzsbysp73rfdjtchkolqpwl2uvsjde7s2hiioejtponbaq";
const PC: &str = "caireeyuculbogazdinryhi6d4";
const PHONE: &str = "6dy7f47u6x3pp6hz7l57z7p674";

/// A relay with short waits: where its route for devices listens, and the relay itself,
/// to count what it holds.
fn relay(change: impl FnOnce(&mut Config)) -> (SocketAddr, Arc<Relay>) {
    let (to, _, relay) = relay_with_jams(change);
    (to, relay)
}

/// The same, and where its jams are served: a different port, and different threads.
fn relay_with_jams(change: impl FnOnce(&mut Config)) -> (SocketAddr, SocketAddr, Arc<Relay>) {
    let mut config = Config::new(KEY.to_string());
    config.hold = Duration::from_millis(400);
    change(&mut config);
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let jams = server.server_addr().to_ip().unwrap();
    let relay = Arc::new(Relay::new(config));
    start(relay.clone(), server, 4);
    let to = listen(relay.clone(), "127.0.0.1:0").unwrap();
    (to, jams, relay)
}

/// One request, as HTTP/1.0 so the answer ends when the connection does. `headers` are
/// written as given; the body's length is stated unless `length` says otherwise.
fn raw(to: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8], length: Option<usize>) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(to).unwrap();
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: relay\r\n");
    if let Some(length) = length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut answer = Vec::new();
    // (A relay with no room for one more connection closes it as it arrives, and its few
    // words may or may not get through first: that is read as the 503 it is.)
    let whole = stream.read_to_end(&mut answer).is_ok();
    let Some(split) = answer.windows(4).position(|window| window == b"\r\n\r\n").filter(|_| whole) else { return (503, String::new(), Vec::new()) };
    let status = std::str::from_utf8(&answer[9..12]).unwrap().parse().unwrap();
    (status, String::from_utf8_lossy(&answer[..split]).to_string(), answer[split + 4..].to_vec())
}

/// A call to the route with the host key and, if given, a place's token.
fn call(to: SocketAddr, method: &str, path: &str, token: &str, body: Value) -> (u16, Value) {
    let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
    let key = format!("Bearer {KEY}");
    let (status, _, bytes) = raw(to, method, path, &[("Authorization", &key), ("X-Nexus-Link", token)], &sent, Some(sent.len()));
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn enter(to: SocketAddr, room: &str, place: &str) -> String {
    let (status, answer) = call(to, "POST", &format!("/api/link/{room}/enter"), "", json!({"place": place}));
    assert_eq!(status, 200, "{answer}");
    answer["token"].as_str().unwrap().to_string()
}

fn poll(to: SocketAddr, room: &str, token: &str, version: u64, after: u64) -> (u16, Value) {
    call(to, "GET", &format!("/api/link/{room}/poll?v={version}&after={after}"), token, Value::Null)
}

/// A poll that asks again when the route's queue is full, as an app does.
fn patient_poll(to: SocketAddr, room: &str, token: &str, version: u64, after: u64) -> Value {
    loop {
        match poll(to, room, token, version, after) {
            (503, _) => std::thread::sleep(Duration::from_millis(40)),
            (status, answer) => {
                assert_eq!(status, 200, "{answer}");
                return answer;
            }
        }
    }
}

fn send(to: SocketAddr, room: &str, token: &str, place: &str, frame: &str) -> u16 {
    call(to, "POST", &format!("/api/link/{room}/send"), token, json!({"to": place, "frame": frame})).0
}

/// Waits, up to three seconds, for something that happens on another thread.
fn until(what: impl Fn() -> bool) {
    let began = Instant::now();
    while !what() && began.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The nth room name, and the nth place, spelled as the route wants them.
fn room_named(n: usize) -> String {
    format!("{}{}{}", BASE32[n % 32] as char, BASE32[n / 32 % 32] as char, "a".repeat(50))
}

fn place_named(n: usize) -> String {
    format!("{}{}{}", BASE32[n % 32] as char, BASE32[n / 32 % 32] as char, "a".repeat(24))
}

#[test]
fn devices_pass_frames_through_their_mailboxes() {
    let (to, _) = relay(|_| {});
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    // Each sees who is in the room.
    let (_, seen) = poll(to, ROOM, &pc, 0, 0);
    assert_eq!(seen["places"], json!([PC, PHONE]));
    let version = seen["v"].as_u64().unwrap();
    // A frame is put in the other's mailbox, marked with where it came from: the place the
    // token belongs to, whatever the sender might claim.
    assert_eq!(send(to, ROOM, &pc, PHONE, "Zmlyc3Q"), 200);
    let (_, got) = poll(to, ROOM, &phone, version, 0);
    assert_eq!(got["frames"], json!([{"seq": 1, "from": PC, "frame": "Zmlyc3Q"}]));
    assert_eq!(got["more"], false);
    // Not said to have been dealt with, it is sent again...
    assert_eq!(poll(to, ROOM, &phone, version, 0).1["frames"][0]["seq"], 1);
    // ...and once it has been, a poll waits, and comes back empty when nothing arrives.
    let began = Instant::now();
    let (_, nothing) = poll(to, ROOM, &phone, version, 1);
    assert_eq!(nothing["frames"], json!([]));
    assert!(began.elapsed() >= Duration::from_millis(350));
    // A frame that arrives wakes a poll that is waiting. Numbers go on from where they were.
    let (room, token) = (ROOM.to_string(), phone.clone());
    let waiting = std::thread::spawn(move || poll(to, &room, &token, version, 1));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(send(to, ROOM, &pc, PHONE, "c2Vjb25k"), 200);
    let (_, woken) = waiting.join().unwrap();
    assert_eq!(woken["frames"], json!([{"seq": 2, "from": PC, "frame": "c2Vjb25k"}]));
    // The sender's own mailbox has nothing in it.
    assert_eq!(poll(to, ROOM, &pc, version, 0).1["frames"], json!([]));
}

#[test]
fn a_place_coming_or_going_wakes_the_others() {
    let (to, relay) = relay(|config| config.idle_after = Duration::from_millis(700));
    let pc = enter(to, ROOM, PC);
    let (_, alone) = poll(to, ROOM, &pc, 0, 0);
    let version = alone["v"].as_u64().unwrap();
    let (room, token) = (ROOM.to_string(), pc.clone());
    let waiting = std::thread::spawn(move || poll(to, &room, &token, version, 0));
    std::thread::sleep(Duration::from_millis(100));
    let phone = enter(to, ROOM, PHONE);
    let (_, joined) = waiting.join().unwrap();
    assert_eq!(joined["places"], json!([PC, PHONE]));
    // One that says it's going is gone at once, and what was waiting for it with it.
    assert_eq!(send(to, ROOM, &pc, PHONE, "Zm9yIHRoZSBwaG9uZQ"), 200);
    assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/leave"), &phone, Value::Null).0, 200);
    assert_eq!(poll(to, ROOM, &pc, joined["v"].as_u64().unwrap(), 0).1["places"], json!([PC]));
    assert_eq!(relay.link.totals().bytes, 0);
    assert_eq!(poll(to, ROOM, &phone, 0, 0).0, 404);
    // One that just stops asking is given up after a while, and the room with it.
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(poll(to, ROOM, &pc, 0, 0).0, 404);
    // (The connection just answered may still be being let go of.)
    assert_eq!(Totals { open: 0, ..relay.link.totals() }, Totals::default());
}

#[test]
fn every_call_needs_the_host_key_and_a_place_needs_its_token() {
    let (to, _) = relay(|_| {});
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    let body = json!({"place": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "to": PHONE, "frame": "eA"}).to_string();
    for (method, verb) in [("POST", "enter"), ("GET", "poll"), ("POST", "send"), ("POST", "leave")] {
        let path = format!("/api/link/{ROOM}/{verb}");
        // No host key, and the wrong one, even with a place's own token.
        assert_eq!(raw(to, method, &path, &[("X-Nexus-Link", &pc)], body.as_bytes(), Some(body.len())).0, 401, "{verb}");
        assert_eq!(raw(to, method, &path, &[("Authorization", "Bearer not-the-host-key-at-all"), ("X-Nexus-Link", &pc)], body.as_bytes(), Some(body.len())).0, 401, "{verb}");
    }
    // The host key alone collects nothing, sends nothing and removes nobody.
    for token in ["", "not-a-token", &pc[..42]] {
        assert_eq!(poll(to, ROOM, token, 0, 0).0, 404);
        assert_eq!(send(to, ROOM, token, PHONE, "eA"), 404);
        assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/leave"), token, Value::Null).0, 404);
    }
    // A token opens its own place in its own room and no other.
    let other = room_named(5);
    let elsewhere = enter(to, &other, PC);
    assert_eq!(poll(to, ROOM, &elsewhere, 0, 0).0, 404);
    assert_eq!(send(to, &other, &pc, PC, "eA"), 404);
    // And a frame can only be sent as the place the token belongs to.
    assert_eq!(send(to, ROOM, &phone, PC, "eA"), 200);
    assert_eq!(poll(to, ROOM, &pc, 0, 0).1["frames"][0]["from"], PHONE);
}

#[test]
fn rooms_places_and_frames_are_spelled_one_way() {
    let (to, relay) = relay(|_| {});
    let enter = |room: &str, place: &str| call(to, "POST", &format!("/api/link/{room}/enter"), "", json!({"place": place})).0;
    assert_eq!(enter(ROOM, PC), 200);
    let upper = ROOM.to_uppercase();
    // (The last letter of each has bits to spare, which must be clear.)
    for room in ["short", &ROOM[..51], &format!("{ROOM}a"), &upper, &format!("{}b", &ROOM[..51]), &format!("{}1", &ROOM[..51])] {
        assert_eq!(enter(room, PHONE), 400, "{room}");
    }
    for place in ["", "pc", &PHONE[..25], &PHONE.to_uppercase(), &format!("{}b", &PHONE[..25]), &format!("{}=", &PHONE[..25])] {
        assert_eq!(enter(ROOM, place), 400, "{place}");
    }
    assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/enter"), "", json!({"place": 7})).0, 400);
    let pc = relay.link.rooms.lock().unwrap().get(ROOM).unwrap().places[0].token.clone();
    assert_eq!(enter(ROOM, PHONE), 200);
    for frame in ["", "not base64!", "with=padding", "a b"] {
        assert_eq!(send(to, ROOM, &pc, PHONE, frame), 400, "{frame}");
    }
    assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/send"), &pc, json!({"to": PHONE, "frame": 7})).0, 400);
    assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/send"), &pc, json!({"to": "pc", "frame": "eA"})).0, 400);
    assert_eq!(send(to, ROOM, &pc, "aaaaaaaaaaaaaaaaaaaaaaaaaa", "eA"), 404);
    assert_eq!(relay.link.totals().frames, 0);
    // A token is 43 characters of the other alphabet.
    assert_eq!(pc.len(), 43);
    assert!(spelled(&pc, BASE64, 32));
    assert!(is_room(ROOM) && is_place(PC) && !is_place(ROOM));
}

#[test]
fn a_place_belongs_to_the_token_it_was_opened_with() {
    let (to, _) = relay(|config| config.idle_after = Duration::from_millis(600));
    let pc = enter(to, ROOM, PC);
    let again = || call(to, "POST", &format!("/api/link/{ROOM}/enter"), "", json!({"place": PC}));
    // Nobody else can take it or replace it, the host key notwithstanding...
    assert_eq!(again().0, 409);
    assert_eq!(raw(to, "POST", &format!("/api/link/{ROOM}/enter"), &[("Authorization", &format!("Bearer {KEY}")), ("X-Nexus-Link", &pc)], br#"{"place":"caireeyuculbogazdinryhi6d4"}"#, Some(38)).0, 409);
    assert_eq!(poll(to, ROOM, &pc, 0, 0).0, 200);
    // ...until it is given up. Then it is free, and the old token opens nothing.
    assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/leave"), &pc, Value::Null).0, 200);
    let (status, entered) = again();
    assert_eq!(status, 200);
    let fresh = entered["token"].as_str().unwrap().to_string();
    assert_ne!(fresh, pc);
    assert_eq!(poll(to, ROOM, &pc, 0, 0).0, 404);
    // A new token counts its frames from 1 again.
    let phone = enter(to, ROOM, PHONE);
    assert_eq!(send(to, ROOM, &phone, PC, "eA"), 200);
    assert_eq!(poll(to, ROOM, &fresh, 0, 0).1["frames"][0]["seq"], 1);
}

#[test]
fn a_second_poll_answers_the_first_and_only_what_was_handed_out_can_be_acknowledged() {
    let (to, relay) = relay(|config| config.hold = Duration::from_secs(5));
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    let version = poll(to, ROOM, &phone, 0, 0).1["v"].as_u64().unwrap();
    let (room, token) = (ROOM.to_string(), phone.clone());
    let first = std::thread::spawn(move || poll(to, &room, &token, version, 0));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(relay.link.totals().held, 1);
    let (room, token) = (ROOM.to_string(), phone.clone());
    let second = std::thread::spawn(move || poll(to, &room, &token, version, 0));
    let (_, replaced) = first.join().unwrap();
    assert_eq!((&replaced["replaced"], &replaced["frames"]), (&json!(true), &json!([])));
    assert_eq!(relay.link.totals().held, 1);
    // A frame arrives for the one still waiting. Saying "I have dealt with frame 9" when
    // only 1 was ever handed out acknowledges nothing...
    assert_eq!(send(to, ROOM, &pc, PHONE, "eA"), 200);
    assert_eq!(second.join().unwrap().1["frames"][0]["seq"], 1);
    assert_eq!(send(to, ROOM, &pc, PHONE, "eQ"), 200);
    let (_, both) = poll(to, ROOM, &phone, version, 9);
    assert_eq!(both["frames"].as_array().unwrap().len(), 2);
    // ...and saying 1 drops that one and no more.
    assert_eq!(poll(to, ROOM, &phone, version, 1).1["frames"], json!([{"seq": 2, "from": PC, "frame": "eQ"}]));
    assert_eq!(relay.link.totals().frames, 1);
}

#[test]
fn each_limit_turns_away_what_is_over_it() {
    let (to, relay) = relay(|config| {
        config.link.rooms = 3;
        config.link.rooms_per_address = 10;
        config.link.places = 3;
        config.link.send_body = 4096;
        config.link.mailbox_frames = 3;
        config.link.mailbox_bytes = 3000;
        config.link.room_bytes = 5000;
        config.link.poll_answer = 6000;
    });
    let entering = |room: &str, place: &str| call(to, "POST", &format!("/api/link/{room}/enter"), "", json!({"place": place})).0;
    // Places in a room, and rooms.
    let tokens: Vec<String> = (0..3).map(|n| enter(to, ROOM, &place_named(n))).collect();
    assert_eq!(entering(ROOM, &place_named(3)), 503);
    assert_eq!((entering(&room_named(1), PC), entering(&room_named(2), PC), entering(&room_named(3), PC)), (200, 200, 503));
    // A whole body.
    let frame = |length: usize| "A".repeat(length);
    let key = format!("Bearer {KEY}");
    let headers = [("Authorization", key.as_str()), ("X-Nexus-Link", tokens[0].as_str())];
    assert_eq!(send(to, ROOM, &tokens[0], &place_named(1), &frame(4096)), 413);
    // One that doesn't say how long it is is not read at all.
    assert_eq!(raw(to, "POST", &format!("/api/link/{ROOM}/send"), &headers, b"", None).0, 411);
    // Frames waiting for one place, by number...
    for _ in 0..3 {
        assert_eq!(send(to, ROOM, &tokens[0], &place_named(1), &frame(100)), 200);
    }
    let path = format!("/api/link/{ROOM}/send");
    let body = json!({"to": place_named(1), "frame": frame(100)}).to_string();
    let (status, head, _) = raw(to, "POST", &path, &headers, body.as_bytes(), Some(body.len()));
    assert_eq!(status, 429);
    assert!(head.to_lowercase().contains("retry-after: 1"), "{head}");
    // ...and by weight; and in the whole room.
    assert_eq!(send(to, ROOM, &tokens[0], &place_named(2), &frame(2900)), 200);
    assert_eq!(send(to, ROOM, &tokens[0], &place_named(2), &frame(101)), 429);
    assert_eq!(send(to, ROOM, &tokens[1], &place_named(0), &frame(1800)), 200);
    assert_eq!(send(to, ROOM, &tokens[1], &place_named(0), &frame(1)), 429);
    assert_eq!(relay.link.totals().bytes, 5000);
    // Dealing with what was sent makes room again.
    assert_eq!(poll(to, ROOM, &tokens[2], 0, 0).1["frames"].as_array().unwrap().len(), 1);
    poll(to, ROOM, &tokens[2], 99, 1);
    assert_eq!(send(to, ROOM, &tokens[1], &place_named(0), &frame(1)), 200);
}

#[test]
fn a_poll_s_answer_has_a_size_and_says_when_there_is_more() {
    let (to, _) = relay(|config| {
        config.link.poll_answer = 8000;
        config.link.sends = (100.0, 100.0);
    });
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    for _ in 0..6 {
        assert_eq!(send(to, ROOM, &pc, PHONE, &"A".repeat(1500)), 200);
    }
    let key = format!("Bearer {KEY}");
    let headers = [("Authorization", key.as_str()), ("X-Nexus-Link", phone.as_str())];
    let (_, _, bytes) = raw(to, "GET", &format!("/api/link/{ROOM}/poll?v=0&after=0"), &headers, b"", Some(0));
    assert!(bytes.len() <= 8000, "{}", bytes.len());
    let first: Value = serde_json::from_slice(&bytes).unwrap();
    let got = first["frames"].as_array().unwrap().len();
    assert!((1..6).contains(&got));
    assert_eq!(first["more"], true);
    // The rest comes once those are dealt with, in order.
    let (_, rest) = poll(to, ROOM, &phone, 0, got as u64);
    assert_eq!(rest["frames"][0]["seq"], got as u64 + 1);
    // One frame bigger than an answer still goes, alone.
    let other = enter(to, ROOM, &place_named(9));
    assert_eq!(send(to, ROOM, &pc, &place_named(9), &"A".repeat(9000)), 200);
    assert_eq!(poll(to, ROOM, &other, 0, 0).1["frames"].as_array().unwrap().len(), 1);
}

#[test]
fn arrivals_and_sends_are_counted_by_place_and_by_address() {
    let (to, _) = relay(|config| {
        config.link.enters = 4.0;
        config.link.sends = (3.0, 0.0);
    });
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    assert_eq!([(); 4].map(|_| send(to, ROOM, &pc, PHONE, "eA")), [200, 200, 200, 429]);
    // Another place has its own allowance...
    assert_eq!(send(to, ROOM, &phone, PC, "eA"), 200);
    // ...and arrivals from one address share one. Naming another address in a header the
    // relay has not been told to believe changes nothing.
    let key = format!("Bearer {KEY}");
    let arrive = |n: usize, forwarded: &str| {
        let body = json!({"place": place_named(n)}).to_string();
        raw(to, "POST", &format!("/api/link/{ROOM}/enter"), &[("Authorization", key.as_str()), ("X-Forwarded-For", forwarded)], body.as_bytes(), Some(body.len())).0
    };
    assert_eq!((arrive(1, "203.0.113.9"), arrive(2, "203.0.113.10"), arrive(3, "203.0.113.11")), (200, 200, 429));
}

#[test]
fn only_the_named_proxy_says_who_is_calling() {
    let (to, _) = relay(|config| {
        config.link.enters = 2.0;
        config.trusted_proxy = Some("127.0.0.1".parse().unwrap());
    });
    let key = format!("Bearer {KEY}");
    let arrive = |n: usize, forwarded: &str| {
        let body = json!({"place": place_named(n)}).to_string();
        raw(to, "POST", &format!("/api/link/{ROOM}/enter"), &[("Authorization", key.as_str()), ("X-Forwarded-For", forwarded)], body.as_bytes(), Some(body.len())).0
    };
    // The last address is the one the proxy wrote; what a client put before it is not.
    assert_eq!((arrive(1, "203.0.113.9"), arrive(2, "198.51.100.1, 203.0.113.9"), arrive(3, "203.0.113.9")), (200, 200, 429));
    assert_eq!(arrive(4, "203.0.113.9, 198.51.100.1"), 200);
    // One /64 of IPv6 is one caller, however it is written; another is another.
    assert_eq!((arrive(5, "2001:db8:1:2::1"), arrive(6, "2001:DB8:1:2:ffff::9"), arrive(7, "2001:db8:1:2:3::4")), (200, 200, 429));
    assert_eq!(arrive(8, "2001:db8:1:3::1"), 200);
    assert_eq!(counted("::ffff:192.0.2.7".parse().unwrap()), "192.0.2.7");
    assert_eq!(counted("2001:db8:1:2:3:4:5:6".parse().unwrap()), "2001:db8:1:2::/64");
}

#[test]
fn nothing_of_a_jam_reaches_a_mailbox() {
    let (to, jams, relay) = relay_with_jams(|_| {});
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    assert_eq!(send(to, ROOM, &pc, PHONE, "c2VhbGVk"), 200);
    let held = |relay: &Relay| {
        let totals = relay.link.totals();
        (totals.rooms, totals.places, totals.frames, totals.bytes)
    };
    let before = held(&relay);
    // A jam, its host's secret, and a guest in it: on the jams' own port.
    let jam_call = |method: &str, path: &str, bearer: &str, body: Value| {
        let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        let bearer = format!("Bearer {bearer}");
        let headers: &[(&str, &str)] = if bearer.len() > 7 { &[("Authorization", bearer.as_str())] } else { &[] };
        let (status, _, bytes) = raw(jams, method, path, headers, &sent, Some(sent.len()));
        (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
    };
    let (_, made) = jam_call("POST", "/api/host/start", KEY, json!({"name": "Jez"}));
    let (jam, secret) = (made["jam"].as_str().unwrap().to_string(), made["secret"].as_str().unwrap().to_string());
    let (_, joined) = jam_call("POST", &format!("/api/jam/{jam}/join"), "", json!({"name": "Guest"}));
    let guest = joined["token"].as_str().unwrap().to_string();
    // A jam's id is not a room, with or without the host key.
    for verb in ["enter", "send", "leave"] {
        assert_eq!(call(to, "POST", &format!("/api/link/{jam}/{verb}"), &guest, json!({"place": PC, "to": PHONE, "frame": "eA"})).0, if verb == "enter" { 400 } else { 404 }, "{verb}");
    }
    assert_eq!(poll(to, &jam, &guest, 0, 0).0, 404);
    // A guest's token and a host's secret open no place, even with the host key beside them...
    for token in [guest.as_str(), secret.as_str(), jam.as_str()] {
        assert_eq!(poll(to, ROOM, token, 0, 0).0, 404);
        assert_eq!(send(to, ROOM, token, PHONE, "eA"), 404);
        assert_eq!(call(to, "POST", &format!("/api/link/{ROOM}/leave"), token, Value::Null).0, 404);
    }
    // ...and without it, as a guest has them, nothing on the route answers at all.
    for bearer in ["", guest.as_str(), secret.as_str(), pc.as_str()] {
        let headers = [("Authorization", format!("Bearer {bearer}")), ("X-Nexus-Link", pc.clone())];
        let headers: Vec<(&str, &str)> = headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
        assert_eq!(raw(to, "GET", &format!("/api/link/{ROOM}/poll?v=0&after=0"), &headers, b"", Some(0)).0, 401, "{bearer}");
    }
    // The route is not there at all where jams are served, with any key or token; and where
    // the route is served there is no jam, no page, and nothing else.
    for (method, verb) in [("POST", "enter"), ("GET", "poll"), ("POST", "send"), ("POST", "leave")] {
        let key = format!("Bearer {KEY}");
        assert_eq!(raw(jams, method, &format!("/api/link/{ROOM}/{verb}"), &[("Authorization", &key), ("X-Nexus-Link", &pc)], b"{}", Some(2)).0, 404, "{verb}");
    }
    for path in ["/".to_string(), "/api/hello".to_string(), "/healthz".to_string(), format!("/api/jam/{jam}"), format!("/j/{jam}"), format!("/api/jam/{jam}/state?v=0"), format!("/api/host/{jam}/poll"), format!("/api/link/{ROOM}"), format!("/api/link/{ROOM}/poll/extra")] {
        for bearer in [KEY, guest.as_str(), secret.as_str()] {
            assert_eq!(raw(to, "GET", &path, &[("Authorization", &format!("Bearer {bearer}"))], b"", Some(0)).0, 404, "{path}");
        }
    }
    assert_eq!(raw(to, "POST", "/api/host/start", &[("Authorization", &format!("Bearer {KEY}"))], b"{}", Some(2)).0, 404);
    // A room's name and a place's token get nothing from the paths a guest or a host uses.
    for path in [format!("/api/jam/{ROOM}"), format!("/api/jam/{ROOM}/state"), format!("/api/jam/{ROOM}/cover/300/al-1"), format!("/api/jam/{PC}")] {
        assert_eq!(jam_call("GET", &path, &pc, Value::Null).0, 404, "{path}");
    }
    assert_eq!(jam_call("POST", &format!("/api/jam/{ROOM}/join"), "", json!({"name": "Guest"})).0, 404);
    assert_eq!(jam_call("POST", &format!("/api/jam/{ROOM}/ask"), &pc, json!({"action": "pause"})).0, 404);
    assert_eq!(jam_call("POST", &format!("/api/jam/{jam}/ask"), &pc, json!({"action": "pause"})).0, 401);
    assert_eq!(jam_call("GET", &format!("/api/host/{ROOM}/poll"), &pc, Value::Null).0, 404);
    assert_eq!(jam_call("GET", &format!("/api/host/{jam}/poll"), &pc, Value::Null).0, 401);
    // The jam's own state says nothing of rooms, places or frames.
    let (_, state) = jam_call("GET", &format!("/api/jam/{jam}/state?v=0"), &guest, Value::Null);
    assert!(!state.to_string().contains(PC) && !state.to_string().contains("c2VhbGVk"));
    // After all of it the mailbox is as it was, and its owner collects the frame.
    assert_eq!(held(&relay), before);
    assert_eq!(poll(to, ROOM, &phone, 0, 0).1["frames"], json!([{"seq": 1, "from": PC, "frame": "c2VhbGVk"}]));
}

#[test]
fn hello_offers_the_route_only_when_it_is_served_and_old_linking_can_be_refused() {
    let old = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let jam_call = |jams: SocketAddr, method: &str, path: &str, body: Value| call(jams, method, path, "", body);
    // A relay that serves only jams says nothing of the route.
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let alone = server.server_addr().to_ip().unwrap();
    start(Arc::new(Relay::new(Config::new(KEY.to_string()))), server, 2);
    let (_, hello) = jam_call(alone, "GET", "/api/hello", Value::Null);
    assert_eq!((&hello["nexus_jam"], &hello["host"], &hello["link"]), (&json!(1), &json!(true), &Value::Null));
    // One that serves it says so, and old linking still works beside it.
    let (to, jams, _) = relay_with_jams(|_| {});
    assert_eq!(jam_call(jams, "GET", "/api/hello", Value::Null).1["link"], 4);
    assert_eq!(jam_call(jams, "POST", &format!("/api/room/{old}/enter"), json!({"id": "pc"})).0, 200);
    enter(to, ROOM, PC);
    let (to, jams, _) = relay_with_jams(|config| config.old_link = false);
    assert_eq!(jam_call(jams, "GET", "/api/hello", Value::Null).1["host"], true);
    for (method, verb) in [("POST", "enter"), ("GET", "poll"), ("POST", "leave")] {
        assert_eq!(jam_call(jams, method, &format!("/api/room/{old}/{verb}"), json!({"id": "pc"})).0, 410, "{verb}");
    }
    // Jams and the new route are as they were.
    assert_eq!(jam_call(jams, "POST", "/api/host/start", json!({"name": "Jez"})).0, 200);
    enter(to, ROOM, PC);
}

/// The reviewer's request, and its neighbours: a body that is JSON and not what its call
/// takes. The first of these ended the whole program, and every jam in it with it.
#[test]
fn a_body_of_the_wrong_shape_is_refused_and_ends_nothing() {
    let (to, jams, relay) = relay_with_jams(|config| config.link.enters = 1000.0);
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    let key = format!("Bearer {KEY}");
    let headers = [("Authorization", key.as_str()), ("X-Nexus-Link", pc.as_str())];
    let good = format!(r#"{{"to":"{PHONE}","frame":"eA"}}"#);
    let wrong = ["7", "-1", "1e9", "true", "null", r#""text""#, "[]", r#"[1,"two"]"#, r#"[{"to":"x"}]"#, "{", "nonsense", r#"{"to":7,"frame":"eA"}"#, r#"{"to":"PHONE"}"#, r#"{"frame":"eA"}"#, r#"{"to":{},"frame":[]}"#, r#"{"to":null,"frame":null}"#];
    for verb in ["send", "enter", "leave"] {
        for body in wrong {
            let body = body.replace("PHONE", PHONE);
            // (`leave` takes nothing: for it, only what is not an object at all is wrong. And
            // `enter` asks only for a place.)
            if verb != "send" && body.starts_with('{') && body.ends_with('}') {
                continue;
            }
            let (status, _, _) = raw(to, "POST", &format!("/api/link/{ROOM}/{verb}"), &headers, body.as_bytes(), Some(body.len()));
            assert_eq!(status, 400, "{verb} with {body}");
        }
    }
    // Nothing was done by any of them: both places are where they were, nothing waits, and
    // the route and the jams are served as before.
    let held = relay.link.totals();
    assert_eq!((held.places, held.frames), (2, 0));
    assert_eq!(raw(to, "POST", &format!("/api/link/{ROOM}/send"), &headers, good.as_bytes(), Some(good.len())).0, 200);
    assert_eq!(poll(to, ROOM, &phone, 0, 0).1["frames"][0]["from"], PC);
    assert_eq!(call(jams, "POST", "/api/host/start", "", json!({"name": "Jez"})).0, 200);
}

/// A fault is outlived only on the route's own threads. On any other it ends the program,
/// as it did before the route existed.
#[test]
fn only_the_route_s_own_threads_outlive_a_fault() {
    for name in ["link-0", "link-5", "link-sweeper", "link-listener"] {
        assert!(outlives_a_fault(Some(name)), "{name}");
    }
    for name in ["worker-0", "worker-7", "sweeper", "main", "", "relink-0", "LINK-0"] {
        assert!(!outlives_a_fault(Some(name)), "{name}");
    }
    assert!(!outlives_a_fault(None));
}

/// Whatever goes wrong in a request, the route goes on: the thread that was serving it
/// answers 500 and takes the next, and a jam on the other port never knows. (The faults
/// here are made by the test: nothing a sender can send is known to cause one.)
#[test]
fn a_fault_in_one_request_ends_neither_the_route_nor_a_jam() {
    let (to, jams, relay) = relay_with_jams(|config| config.link.threads = 2);
    let pc = enter(to, ROOM, PC);
    let key = format!("Bearer {KEY}");
    // More faults than the route has threads, some of them while its rooms are locked.
    for n in 0..6 {
        let token = if n % 2 == 0 { FAULT } else { FAULT_HOLDING_THE_ROOMS };
        let (status, _, body) = raw(to, "GET", &format!("/api/link/{ROOM}/poll?v=0&after=0"), &[("Authorization", &key), ("X-Nexus-Link", token)], b"", Some(0));
        assert_eq!((status, String::from_utf8_lossy(&body).contains("went wrong")), (500, true));
    }
    // The route still serves, with what it held intact; nothing is left open or charged.
    assert_eq!(send(to, ROOM, &pc, PC, "eA"), 200);
    assert_eq!(poll(to, ROOM, &pc, 0, 0).1["frames"][0]["frame"], "eA");
    let began = Instant::now();
    while relay.link.totals().open > 0 && began.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = relay.link.totals();
    assert_eq!((after.open, after.waiting, after.writing, after.places), (0, 0, 0, 1));
    // And a jam is started, joined and shown as if nothing had happened.
    let (_, made) = call(jams, "POST", "/api/host/start", "", json!({"name": "Jez"}));
    let jam = made["jam"].as_str().unwrap();
    assert_eq!(call(jams, "POST", &format!("/api/jam/{jam}/join"), "", json!({"name": "Guest"})).0, 200);
    assert_eq!(call(jams, "GET", &format!("/api/jam/{jam}"), "", Value::Null).0, 200);
}

/// A request is refused before any of its body is read if the body is not one the route
/// will take, and the connection is closed, not read to its end.
#[test]
fn a_body_the_route_will_not_take_is_never_read() {
    let (to, relay) = relay(|config| config.link.body_wait = Duration::from_secs(30));
    let pc = enter(to, ROOM, PC);
    let key = format!("Bearer {KEY}");
    // A sender that says far more is coming than a `send` may be, and sends none of it,
    // is answered at once and let go of: nobody waits for what it promised.
    for (verb, promised) in [("send", 288 * 1024 + 1), ("send", usize::MAX), ("enter", 8 * 1024 + 1), ("leave", 1 << 30)] {
        let began = Instant::now();
        let mut stream = TcpStream::connect(to).unwrap();
        stream.write_all(format!("POST /api/link/{ROOM}/{verb} HTTP/1.1\r\nHost: relay\r\nContent-Length: {promised}\r\nAuthorization: {key}\r\nX-Nexus-Link: {pc}\r\n\r\n").as_bytes()).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 413 "), "{verb}: {}", String::from_utf8_lossy(&answer));
        assert!(began.elapsed() < Duration::from_secs(2), "{verb}");
    }
    // The same for one that doesn't say how long it is, in either of the ways there are.
    let unsaid = format!("POST /api/link/{ROOM}/send HTTP/1.1\r\nHost: relay\r\nAuthorization: {key}\r\nX-Nexus-Link: {pc}\r\n");
    for head in [format!("{unsaid}\r\n"), format!("{unsaid}Transfer-Encoding: chunked\r\n\r\n5\r\nhello")] {
        let mut stream = TcpStream::connect(to).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 411 "), "{}", String::from_utf8_lossy(&answer));
    }
    // And for one without the host key, or without a place: nothing it sends is waited for.
    for (bearer, token) in [("Bearer not-the-host-key-at-all", pc.as_str()), (key.as_str(), "no-such-token")] {
        let began = Instant::now();
        let mut stream = TcpStream::connect(to).unwrap();
        stream.write_all(format!("POST /api/link/{ROOM}/send HTTP/1.1\r\nHost: relay\r\nContent-Length: 4000\r\nAuthorization: {bearer}\r\nX-Nexus-Link: {token}\r\n\r\n").as_bytes()).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        assert!(answer.starts_with(b"HTTP/1.1 401 ") || answer.starts_with(b"HTTP/1.1 404 "));
        assert!(began.elapsed() < Duration::from_secs(2));
    }
    // A head that never ends is cut off at its size, and one that isn't a request at all
    // is simply closed.
    let mut stream = TcpStream::connect(to).unwrap();
    let _ = stream.write_all(format!("GET /api/link/{ROOM}/poll HTTP/1.1\r\nX-Pad: {}\r\n", "a".repeat(9000)).as_bytes());
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    assert!(answer.starts_with(b"HTTP/1.1 431 "), "{}", String::from_utf8_lossy(&answer));
    let mut stream = TcpStream::connect(to).unwrap();
    stream.write_all(&[0xff, 0xfe, b'\r', b'\n', b'\r', b'\n']).unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    assert!(answer.is_empty());
    // Each was let go of within a moment of being answered.
    let began = Instant::now();
    while relay.link.totals().open > 0 && began.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(relay.link.totals().open, 0);
}

/// Reading has a deadline, a head's and a body's, and the route is closed to more than it
/// can hold: so however it is leant on, it comes back, and it never grows.
#[test]
fn a_stalled_head_or_body_is_given_up_and_too_many_connections_are_closed_unread() {
    let (to, relay) = relay(|config| {
        config.link.threads = 2;
        config.link.queue = 4;
        config.link.connections = 12;
        config.link.head_wait = Duration::from_millis(500);
        config.link.body_wait = Duration::from_millis(500);
        config.hold = Duration::from_secs(4);
    });
    // Six devices, each waiting for something to be put in its mailbox.
    let tokens: Vec<String> = (0..6).map(|n| enter(to, ROOM, &place_named(n))).collect();
    let pc = enter(to, ROOM, PC);
    let version = poll(to, ROOM, &pc, 0, 0).1["v"].as_u64().unwrap();
    // (One after another: all at once, six would be more than the queue of four takes.)
    let waiting: Vec<_> = tokens.into_iter().enumerate().map(|(n, token)| {
        let device = std::thread::spawn(move || poll(to, ROOM, &token, version, 0));
        until(|| relay.link.totals().held == n + 1);
        device
    }).collect();
    assert_eq!((relay.link.totals().held, relay.link.totals().open), (6, 6));
    // Two senders that promise a body and stall: both of the route's threads are waiting.
    // Four more that say nothing at all wait for a thread.
    let stalled = format!("POST /api/link/{ROOM}/send HTTP/1.1\r\nHost: relay\r\nContent-Length: 4000\r\nAuthorization: Bearer {KEY}\r\nX-Nexus-Link: {pc}\r\n\r\n{{\"to\":");
    let mut held: Vec<TcpStream> = (0..2).map(|_| {
        let mut stream = TcpStream::connect(to).unwrap();
        stream.write_all(stalled.as_bytes()).unwrap();
        stream
    }).collect();
    until(|| relay.link.totals().open == 8 && relay.link.totals().waiting == 0);
    held.extend((0..4).map(|_| TcpStream::connect(to).unwrap()));
    until(|| relay.link.totals().open == 12);
    assert_eq!((relay.link.totals().open, relay.link.totals().waiting), (12, 4));
    // That is as many connections as the route takes: one more is closed as it arrives,
    // with nothing read from it and nothing said to it.
    let began = Instant::now();
    let mut closed = TcpStream::connect(to).unwrap();
    let _ = closed.write_all(stalled.as_bytes());
    let mut answer = Vec::new();
    let _ = closed.read_to_end(&mut answer);
    assert!(answer.is_empty() && began.elapsed() < Duration::from_millis(300), "{}", String::from_utf8_lossy(&answer));
    assert_eq!(relay.link.totals().open, 12);
    // Every one of the twelve is given up at its deadline or answered, whether or not its
    // sender ever lets go, and the route is back: only the waiting devices are left, nothing
    // waits for a thread, and a device is served.
    let began = Instant::now();
    while relay.link.totals().open > 6 && began.elapsed() < Duration::from_secs(8) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = relay.link.totals();
    assert_eq!((after.open, after.waiting, after.held, after.frames), (6, 0, 6, 0));
    assert!(began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
    for stream in &mut held {
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        assert!(answer.is_empty(), "{}", String::from_utf8_lossy(&answer));
    }
    assert_eq!(send(to, ROOM, &pc, &place_named(0), "eA"), 200);
    for device in waiting {
        assert_eq!(device.join().unwrap().0, 200);
    }
}

/// With the queue for a thread full, a connection is told the relay is busy as it arrives,
/// and nothing of it is read.
#[test]
fn with_every_thread_busy_and_the_queue_full_the_next_is_turned_away_unread() {
    let (to, relay) = relay(|config| {
        config.link.threads = 1;
        config.link.queue = 2;
        config.link.head_wait = Duration::from_millis(800);
    });
    let mut held: Vec<TcpStream> = vec![TcpStream::connect(to).unwrap()];
    std::thread::sleep(Duration::from_millis(100));
    held.extend((0..2).map(|_| TcpStream::connect(to).unwrap()));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!((relay.link.totals().open, relay.link.totals().waiting), (3, 2));
    let began = Instant::now();
    let mut over = TcpStream::connect(to).unwrap();
    let mut answer = Vec::new();
    let _ = over.read_to_end(&mut answer);
    assert!(answer.starts_with(b"HTTP/1.1 503 "), "{}", String::from_utf8_lossy(&answer));
    assert!(began.elapsed() < Duration::from_millis(300));
    assert_eq!(relay.link.totals().open, 3);
}

/// The reviewer's case, and its neighbours: the route leant on every way there is, while a
/// jam is hosted, joined, steered and polled through the same relay. No answer to the jam
/// is slowed, because nothing of the route runs where jams are served.
#[test]
fn the_route_saturated_costs_a_jam_nothing() {
    let (to, jams, relay) = relay_with_jams(|config| {
        config.link.head_wait = Duration::from_secs(20);
        config.link.body_wait = Duration::from_secs(20);
        config.link.write_wait = Duration::from_secs(20);
        config.link.sends = (1000.0, 1000.0);
        config.link.sends_per_address = 10000.0;
    });
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    let key = format!("Bearer {KEY}");
    // Every thread of the route waiting on a body that is not coming; its queue full of
    // more of the same; connections past that turned away; and a device that asked for a
    // great deal and reads none of it.
    let stalled = format!("POST /api/link/{ROOM}/send HTTP/1.1\r\nHost: relay\r\nContent-Length: 290000\r\nAuthorization: {key}\r\nX-Nexus-Link: {pc}\r\n\r\n{{\"to\":");
    let leaning: Vec<TcpStream> = (0..120).filter_map(|_| {
        let mut stream = TcpStream::connect(to).ok()?;
        let _ = stream.write_all(stalled.as_bytes());
        Some(stream)
    }).collect();
    std::thread::sleep(Duration::from_millis(300));
    let totals = relay.link.totals();
    assert_eq!((totals.waiting, totals.open), (64, 70), "six being read, sixty-four waiting, the rest told the relay is busy");
    // A jam, meanwhile: started, joined, shown, asked of, answered, and watched.
    let timed = |method: &str, path: &str, bearer: &str, body: Value| {
        let began = Instant::now();
        let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        let bearer = format!("Bearer {bearer}");
        let headers: &[(&str, &str)] = if bearer.len() > 7 { &[("Authorization", bearer.as_str())] } else { &[] };
        let (status, _, bytes) = raw(jams, method, path, headers, &sent, Some(sent.len()));
        assert_eq!(status, 200, "{method} {path}: {}", String::from_utf8_lossy(&bytes));
        assert!(began.elapsed() < Duration::from_millis(250), "{method} {path} took {:?}", began.elapsed());
        serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)
    };
    for _ in 0..3 {
        let made = timed("POST", "/api/host/start", KEY, json!({"name": "Jez", "public": true}));
        let (jam, secret) = (made["jam"].as_str().unwrap().to_string(), made["secret"].as_str().unwrap().to_string());
        timed("POST", &format!("/api/host/{jam}/state"), &secret, json!({"state": {"playing": true, "queue": []}}));
        timed("GET", &format!("/api/jam/{jam}"), "", Value::Null);
        let guest = timed("POST", &format!("/api/jam/{jam}/join"), "", json!({"name": "Guest"}))["token"].as_str().unwrap().to_string();
        timed("GET", &format!("/api/jam/{jam}/state?v=0"), &guest, Value::Null);
        // The guest steers; the host collects the question and answers it.
        let asking = {
            let (jam, guest) = (jam.clone(), guest.clone());
            std::thread::spawn(move || call_as(jams, "POST", &format!("/api/jam/{jam}/ask"), &guest, json!({"action": "pause", "data": {}})))
        };
        std::thread::sleep(Duration::from_millis(150));
        let asked = timed("GET", &format!("/api/host/{jam}/poll?after=0"), &secret, Value::Null);
        let ask = asked["events"].as_array().unwrap().iter().find(|event| event["kind"] == "ask").unwrap()["id"].as_u64().unwrap();
        timed("POST", &format!("/api/host/{jam}/reply"), &secret, json!({"id": ask, "ok": true, "data": {}}));
        assert_eq!(asking.join().unwrap().0, 200);
        timed("GET", "/api/hello", KEY, Value::Null);
        timed("POST", &format!("/api/host/{jam}/end"), &secret, json!({}));
    }
    // The devices themselves are another matter: the route is full, and says so.
    assert_eq!(poll(to, ROOM, &phone, 0, 0).0, 503);
    drop(leaning);
}

/// A call as a jam's guest or host makes it: the bearer is theirs, not the host key.
fn call_as(to: SocketAddr, method: &str, path: &str, bearer: &str, body: Value) -> (u16, Value) {
    let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
    let bearer = format!("Bearer {bearer}");
    let (status, _, bytes) = raw(to, method, path, &[("Authorization", &bearer)], &sent, Some(sent.len()));
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The whole of what the route can hold, at its shipped limits: every room, every place,
/// every room full, every place polling. And what it is writing at once, counted as what
/// is written: every byte of every reply.
#[test]
fn the_totals_under_load() {
    let (to, relay) = relay(|config| {
        // (Everything comes from one address in a test, and all at once: the limits on how
        // often are lifted, and the limits on how much are the ones being tested.)
        config.link.rooms_per_address = 20;
        config.link.enters = 1000.0;
        config.link.sends = (1000.0, 1000.0);
        config.link.sends_per_address = 10000.0;
        config.hold = Duration::from_secs(4);
    });
    let limits = Limits::default();
    let rooms: Vec<String> = (0..limits.rooms).map(room_named).collect();
    let filled: Vec<Vec<String>> = std::thread::scope(|scope| {
        let filling: Vec<_> = rooms.iter().map(|room| scope.spawn(move || (0..limits.places).map(|n| enter(to, room, &place_named(n))).collect::<Vec<String>>())).collect();
        filling.into_iter().map(|thread| thread.join().unwrap()).collect()
    });
    assert_eq!(call(to, "POST", &format!("/api/link/{}/enter", room_named(limits.rooms)), "", json!({"place": PC})).0, 503);
    assert_eq!(call(to, "POST", &format!("/api/link/{}/enter", rooms[0]), "", json!({"place": place_named(limits.places)})).0, 503);
    // Every room is filled to the brim: eight frames of a quarter of a megabyte, one to
    // each of eight places, and then nothing more is taken for anyone in it.
    let frame = "A".repeat(256 * 1024);
    std::thread::scope(|scope| {
        for (room, tokens) in rooms.iter().zip(&filled) {
            let frame = &frame;
            scope.spawn(move || {
                for n in 1..=8 {
                    assert_eq!(send(to, room, &tokens[0], &place_named(n), frame), 200);
                }
                assert_eq!(send(to, room, &tokens[0], &place_named(9), "eA"), 429);
                assert_eq!(send(to, room, &tokens[0], &place_named(1), &frame[..1024]), 429);
            });
        }
    });
    let stored = relay.link.totals();
    assert_eq!((stored.rooms, stored.places, stored.frames, stored.bytes), (20, 320, 160, 40 * 1024 * 1024));
    // Every place asks at once. What is being written is never more than its allowance,
    // every frame still arrives, and what was counted is what was sent: the bytes the
    // route says it wrote are the bytes the callers received, heads and all.
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let most = {
        let (relay, watching) = (relay.clone(), watching.clone());
        std::thread::spawn(move || {
            let mut most = 0;
            while watching.load(Ordering::SeqCst) {
                most = most.max(relay.link.totals().writing);
                std::thread::sleep(Duration::from_millis(1));
            }
            most
        })
    };
    let wrote_before = relay.link.written.load(Ordering::SeqCst);
    let key = format!("Bearer {KEY}");
    let (got, received): (usize, usize) = std::thread::scope(|scope| {
        let asking: Vec<_> = rooms.iter().zip(&filled).flat_map(|(room, tokens)| tokens.iter().map(move |token| (room, token))).map(|(room, token)| {
            let key = &key;
            scope.spawn(move || loop {
                // (Told the relay is busy, a device asks again, as the app does. Those few
                // bytes are written as the connection arrives and are not part of the count.)
                let (status, head, body) = raw(to, "GET", &format!("/api/link/{room}/poll?v=0&after=0"), &[("Authorization", key), ("X-Nexus-Link", token)], b"", Some(0));
                if status == 503 {
                    std::thread::sleep(Duration::from_millis(40));
                    continue;
                }
                assert_eq!(status, 200);
                let frames = serde_json::from_slice::<Value>(&body).unwrap()["frames"].as_array().map_or(0, |frames| frames.len());
                break (frames, head.len() + 4 + body.len());
            })
        }).collect();
        asking.into_iter().map(|thread| thread.join().unwrap()).fold((0, 0), |sum, one| (sum.0 + one.0, sum.1 + one.1))
    });
    watching.store(false, Ordering::SeqCst);
    let most = most.join().unwrap();
    assert_eq!(got, 160);
    assert_eq!(relay.link.written.load(Ordering::SeqCst) - wrote_before, received);
    assert!(received > 160 * 256 * 1024 + 320 * 150, "{received}");
    // (On one machine an answer is written as fast as it is made, so the allowance is
    // seldom near full here. That it refuses what does not fit is tested by itself, below.)
    assert!(most > 0 && most <= limits.writing, "{most}");
    assert_eq!(relay.link.totals().writing, 0);
    // Dealt with, they are gone; and then every place waits, and that is as many as wait.
    std::thread::scope(|scope| {
        for (room, tokens) in rooms.iter().zip(&filled) {
            for token in tokens {
                scope.spawn(move || {
                    let now = patient_poll(to, room, token, 0, 1);
                    patient_poll(to, room, token, now["v"].as_u64().unwrap(), 1)
                });
            }
        }
        let began = Instant::now();
        while relay.link.totals().held < 320 && began.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let held = relay.link.totals();
        assert_eq!((held.held, held.frames, held.bytes, held.writing), (320, 0, 0, 0));
    });
}

/// What an answer to a poll is charged is its length, to the byte: measured before a
/// single frame is copied.
#[test]
fn a_poll_s_answer_is_measured_before_it_is_built() {
    let link = Link::default();
    let limits = Limits::default();
    let frames = [Frame { seq: 7, from: PC.to_string(), frame: "A".repeat(1000) }, Frame { seq: 12345, from: PHONE.to_string(), frame: "eA".to_string() }];
    let chosen: Vec<&Frame> = frames.iter().collect();
    for (places, frames, more, replaced) in [(vec![], vec![], false, false), (vec![PC, PHONE], chosen.clone(), true, false), (vec![PC], chosen[..1].to_vec(), false, true)] {
        let before = link.writing.load(Ordering::SeqCst);
        let bytes = link.admit_poll(41, &places, &frames, more, replaced, &limits).unwrap();
        assert_eq!(link.writing.load(Ordering::SeqCst) - before, bytes.len());
        let split = bytes.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let (head, body) = (String::from_utf8_lossy(&bytes[..split]).to_string(), &bytes[split + 4..]);
        assert!(head.contains(&format!("Content-Length: {}", body.len())) && head.contains("Connection: close") && head.contains("Cache-Control: no-store"));
        let read: Value = serde_json::from_slice(body).unwrap();
        assert_eq!((read["v"].as_u64(), read["places"].as_array().map(Vec::len), read["frames"].as_array().map(Vec::len), read["more"].as_bool(), read.get("replaced").is_some()), (Some(41), Some(places.len()), Some(frames.len()), Some(more), replaced));
    }
    assert_eq!(link.admit_poll(1, &[PC], &chosen, false, false, &Limits { writing: 1000, ..Limits::default() }).map(|bytes| bytes.len()), None);
}

/// A connection that stops reading is given up on, not waited for. (The deadline is the
/// listening socket's, which a connection taken from it starts with: so on Linux.)
#[cfg(target_os = "linux")]
#[test]
fn a_write_that_makes_no_progress_is_given_up() {
    assert!(stuck_writes_are_given_up(Duration::from_millis(10), Duration::from_secs(90)), "nobody else was answered");
    // And it is the deadline that does it: with none to speak of, the thread stays stuck.
    assert!(!stuck_writes_are_given_up(Duration::from_secs(3600), Duration::from_secs(6)), "the thread was never stuck");
}

/// Whether, with one thread for such requests and that thread writing to a connection
/// that has stopped reading, somebody else is answered within `patience`.
#[cfg(target_os = "linux")]
fn stuck_writes_are_given_up(deadline: Duration, patience: Duration) -> bool {
    let server = Arc::new(super::super::listen("127.0.0.1:0", deadline).unwrap());
    let to = server.server_addr().to_ip().unwrap();
    start(Arc::new(Relay::new(Config::new(KEY.to_string()))), server, 1);
    // More answers asked for on one connection than the system will hold on the way, and
    // none of them read. A moment later the thread is in the middle of writing them.
    let asked = 24 * 1024 * 1024 / super::super::SCRIPT.len();
    let mut flood = TcpStream::connect(to).unwrap();
    flood.write_all(&b"GET /app.js HTTP/1.1\r\nHost: relay\r\n\r\n".repeat(asked)).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let mut other = TcpStream::connect(to).unwrap();
    other.set_read_timeout(Some(patience)).unwrap();
    other.write_all(b"GET /healthz HTTP/1.0\r\nHost: relay\r\n\r\n").unwrap();
    let mut answer = Vec::new();
    let read = other.read_to_end(&mut answer);
    drop(flood);
    read.is_ok() && answer.windows(3).any(|window| window == b"200")
}
