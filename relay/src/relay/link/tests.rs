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

/// A relay on a port of its own, with short waits; and the relay itself, to count what it holds.
fn relay(change: impl FnOnce(&mut Config)) -> (SocketAddr, Arc<Relay>) {
    let mut config = Config::new(KEY.to_string());
    config.hold = Duration::from_millis(400);
    change(&mut config);
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let address = server.server_addr().to_ip().unwrap();
    let relay = Arc::new(Relay::new(config));
    start(relay.clone(), server, 4);
    (address, relay)
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
    stream.read_to_end(&mut answer).unwrap();
    let split = answer.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
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
    assert_eq!(relay.link.totals(), Totals::default());
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
    let (to, relay) = relay(|_| {});
    let (pc, phone) = (enter(to, ROOM, PC), enter(to, ROOM, PHONE));
    assert_eq!(send(to, ROOM, &pc, PHONE, "c2VhbGVk"), 200);
    let before = relay.link.totals();
    // A jam, its host's secret, and a guest in it.
    let jam_call = |method: &str, path: &str, bearer: &str, body: Value| {
        let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        let bearer = format!("Bearer {bearer}");
        let headers: &[(&str, &str)] = if bearer.len() > 7 { &[("Authorization", bearer.as_str())] } else { &[] };
        let (status, _, bytes) = raw(to, method, path, headers, &sent, Some(sent.len()));
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
    assert_eq!(relay.link.totals(), before);
    assert_eq!(poll(to, ROOM, &phone, 0, 0).1["frames"], json!([{"seq": 1, "from": PC, "frame": "c2VhbGVk"}]));
}

#[test]
fn hello_says_the_route_is_here_and_old_linking_can_be_refused() {
    let old = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (to, _) = relay(|_| {});
    assert_eq!(call(to, "GET", "/api/hello", "", Value::Null).1["link"], 4);
    assert_eq!(call(to, "POST", &format!("/api/room/{old}/enter"), "", json!({"id": "pc"})).0, 200);
    let (to, _) = relay(|config| config.old_link = false);
    assert_eq!(call(to, "GET", "/api/hello", "", Value::Null).1["host"], true);
    for (method, verb) in [("POST", "enter"), ("GET", "poll"), ("POST", "leave")] {
        assert_eq!(call(to, method, &format!("/api/room/{old}/{verb}"), "", json!({"id": "pc"})).0, 410, "{verb}");
    }
    // Jams and the new route are as they were.
    assert_eq!(call(to, "POST", "/api/host/start", "", json!({"name": "Jez"})).0, 200);
    enter(to, ROOM, PC);
}

#[test]
fn a_stalled_body_holds_one_of_the_route_s_own_threads_and_no_jam() {
    let (to, relay) = relay(|config| {
        config.link.threads = 1;
        config.link.queue = 1;
        config.hold = Duration::from_secs(5);
    });
    let pc = enter(to, ROOM, PC);
    // A sender that says 4,000 bytes are coming and sends none: the route's one thread waits.
    // (A body under a kilobyte is read by the HTTP library before the request is handed
    // over, on the connection's own thread.)
    let mut stalled = TcpStream::connect(to).unwrap();
    stalled.write_all(format!("POST /api/link/{ROOM}/send HTTP/1.0\r\nHost: relay\r\nContent-Length: 4000\r\nAuthorization: Bearer {KEY}\r\nX-Nexus-Link: {pc}\r\n\r\n").as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // One more request waits for the thread; the next is turned away at once.
    let (room, token) = (ROOM.to_string(), pc.clone());
    let queued = std::thread::spawn(move || poll(to, &room, &token, 0, 0));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(relay.link.totals().waiting, 1);
    let began = Instant::now();
    assert_eq!(poll(to, ROOM, &pc, 0, 0).0, 503);
    // Jams are answered meanwhile as if nothing were happening.
    assert_eq!(call(to, "POST", "/api/host/start", "", json!({"name": "Jez"})).0, 200);
    assert_eq!(call(to, "GET", "/api/hello", "", Value::Null).0, 200);
    assert!(began.elapsed() < Duration::from_millis(500));
    // When the sender gives up, the thread is free again and the waiting request is answered.
    drop(stalled);
    assert_eq!(queued.join().unwrap().0, 200);
    assert_eq!(relay.link.totals().waiting, 0);
}

/// The whole of what the route can hold, at its shipped limits: every room, every place,
/// every room full, every place polling.
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
    // and every frame still arrives.
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let most = {
        let (relay, watching) = (relay.clone(), watching.clone());
        std::thread::spawn(move || {
            let mut most = 0;
            while watching.load(Ordering::SeqCst) {
                most = most.max(relay.link.totals().writing);
                std::thread::sleep(Duration::from_millis(2));
            }
            most
        })
    };
    let got: usize = std::thread::scope(|scope| {
        let asking: Vec<_> = rooms.iter().zip(&filled).flat_map(|(room, tokens)| tokens.iter().map(move |token| (room, token))).map(|(room, token)| scope.spawn(move || patient_poll(to, room, token, 0, 0)["frames"].as_array().map_or(0, |frames| frames.len()))).collect();
        asking.into_iter().map(|thread| thread.join().unwrap()).sum()
    });
    watching.store(false, Ordering::SeqCst);
    let most = most.join().unwrap();
    assert_eq!(got, 160);
    assert!(most <= limits.writing, "{most}");
    assert!(most > 0);
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
