//! Who the proxy is (`NEXUS_PROXY`): one fixed IP address, or the NAME of the proxy's container
//! on a network the backend shares with it, for when the proxy's address is given to it afresh
//! at each start (Nginx Proxy Manager on a shared Docker network, say).
//!
//! With a name, a peer is the proxy if its address is among those the name resolves to now, by
//! the system's resolver (inside a container, Docker's own). Only that one configured name is
//! ever looked up, never anything taken from a request. Its answer is kept for [`FRESH`], one
//! lookup at a time, so that a flood of connections is never a flood of lookups; a connection
//! waits for a lookup [`WAIT`] at most. A name that does not resolve, or a lookup not answered
//! within the wait, makes NO ONE the proxy (closed, not open), and the log says so once each
//! time that changes, not once a connection.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// How long a name's answer is believed: a proxy started again at a new address is followed
/// within this.
pub const FRESH: Duration = Duration::from_secs(5);
/// How long a name that did not resolve is believed not to: shorter, so that a proxy that has
/// just come back is answered soon.
pub const FRESH_REFUSED: Duration = Duration::from_secs(1);
/// The most a connection waits for a lookup; past it, it is not the proxy's.
pub const WAIT: Duration = Duration::from_secs(2);

/// The setting as given: a fixed address, or the name of the proxy's container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySetting {
    Address(IpAddr),
    Name(String),
}

/// Why a value is not a proxy setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAProxy;

impl FromStr for ProxySetting {
    type Err = NotAProxy;

    /// An IP address (written as IPv6 or not; never unspecified or multicast), or a plain host
    /// name: letters, digits, `-`, `_` and `.` between non-empty parts, beginning with a letter
    /// or a digit, at most 253 bytes, and not one the resolver would read as an address
    /// (`172.18.0`, `0x7f000001`). No scheme, path, port or space.
    fn from_str(value: &str) -> Result<Self, NotAProxy> {
        if let Ok(address) = value.parse::<IpAddr>() {
            let address = address.to_canonical();
            return if address.is_unspecified() || address.is_multicast() { Err(NotAProxy) } else { Ok(ProxySetting::Address(address)) };
        }
        let plain = !value.is_empty()
            && value.len() <= 253
            && value.split('.').all(|part| {
                !part.is_empty()
                    && part.len() <= 63
                    && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                    && part.as_bytes()[0].is_ascii_alphanumeric()
            });
        // Every part a number (decimal, octal or 0x…) is an address to the resolver, not a name.
        let numeric = value.split('.').all(|part| {
            part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() >= 2 && part[..2].eq_ignore_ascii_case("0x") && part[2..].bytes().all(|byte| byte.is_ascii_hexdigit()))
        });
        if plain && !numeric { Ok(ProxySetting::Name(value.to_string())) } else { Err(NotAProxy) }
    }
}

impl PartialEq<IpAddr> for ProxySetting {
    fn eq(&self, other: &IpAddr) -> bool {
        matches!(self, ProxySetting::Address(address) if *address == other.to_canonical())
    }
}

impl fmt::Display for ProxySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxySetting::Address(address) => write!(f, "{address}"),
            ProxySetting::Name(name) => write!(f, "the addresses of the name {name:?}, looked up as connections come"),
        }
    }
}

/// A lookup of the one configured name: its addresses, or why there are none.
pub type Lookup = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;
/// What looks a name up: the system's resolver, or a test's.
pub type Resolver = Arc<dyn Fn(&str) -> Lookup + Send + Sync>;

/// The system's resolver (`getaddrinfo`, on a thread of its own).
pub fn system_resolver() -> Resolver {
    Arc::new(|name: &str| {
        let name = name.to_string();
        Box::pin(async move { Ok(tokio::net::lookup_host((name.as_str(), 0)).await?.map(|found| found.ip()).collect()) })
    })
}

/// Who the proxy is, as the listener asks it.
#[derive(Clone)]
pub struct Proxy(Inner);

#[derive(Clone)]
enum Inner {
    Address(IpAddr),
    Name(Arc<Named>),
}

impl From<IpAddr> for Proxy {
    fn from(address: IpAddr) -> Self {
        Proxy(Inner::Address(address.to_canonical()))
    }
}

impl From<ProxySetting> for Proxy {
    fn from(setting: ProxySetting) -> Self {
        match setting {
            ProxySetting::Address(address) => address.into(),
            ProxySetting::Name(name) => Proxy::named(name, system_resolver(), true),
        }
    }
}

impl Proxy {
    /// A proxy known by `name`, looked up by `resolve`; `log`: whether its changes are written.
    pub fn named(name: String, resolve: Resolver, log: bool) -> Self {
        Self::named_with(name, resolve, log, FRESH, FRESH_REFUSED, WAIT)
    }

    /// As [`Proxy::named`], with its times (the tests' own).
    pub fn named_with(name: String, resolve: Resolver, log: bool, fresh: Duration, fresh_refused: Duration, wait: Duration) -> Self {
        let (told, _) = watch::channel(0);
        Proxy(Inner::Name(Arc::new(Named { name, resolve, log, fresh, fresh_refused, wait, state: Mutex::new(State::default()), told })))
    }

    /// Whether `from` is the proxy, if that is known now without waiting: always for a fixed
    /// address; for a name, while its last answer is fresh.
    pub fn now(&self, from: IpAddr) -> Option<bool> {
        let from = from.to_canonical();
        match &self.0 {
            Inner::Address(address) => Some(from == *address),
            Inner::Name(named) => named.fresh_answer(&named.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()), from),
        }
    }

    /// Whether `from` is the proxy, waiting for a lookup if need be ([`WAIT`] at most).
    pub async fn is(&self, from: IpAddr) -> bool {
        match &self.0 {
            Inner::Address(address) => from.to_canonical() == *address,
            Inner::Name(named) => named.clone().is(from.to_canonical()).await,
        }
    }
}

struct Named {
    name: String,
    resolve: Resolver,
    log: bool,
    fresh: Duration,
    fresh_refused: Duration,
    wait: Duration,
    state: Mutex<State>,
    /// Moved on at each answer, for those waiting.
    told: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    /// The last answer and when it came: the addresses (none if the name did not resolve).
    answer: Option<(Instant, Option<Vec<IpAddr>>)>,
    /// A lookup is under way.
    looking: bool,
    /// What the log last said, so that it says a thing once.
    said: Option<Said>,
}

#[derive(Clone, PartialEq, Eq)]
enum Said {
    At(Vec<IpAddr>),
    NotFound,
    TooSlow,
}

impl Named {
    fn fresh_answer(&self, state: &State, from: IpAddr) -> Option<bool> {
        let (at, addresses) = state.answer.as_ref()?;
        let life = if addresses.is_some() { self.fresh } else { self.fresh_refused };
        (at.elapsed() < life).then(|| addresses.as_ref().is_some_and(|addresses| addresses.contains(&from)))
    }

    async fn is(self: Arc<Self>, from: IpAddr) -> bool {
        let deadline = Instant::now() + self.wait;
        let mut told = self.told.subscribe();
        {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(answer) = self.fresh_answer(&state, from) {
                return answer;
            }
            if !state.looking {
                state.looking = true;
                tokio::spawn(self.clone().look());
            }
            told.mark_unchanged();
        }
        loop {
            if tokio::time::timeout_at(deadline, told.changed()).await.is_err() {
                let _ = self.say(Said::TooSlow);
                return false;
            }
            let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(answer) = self.fresh_answer(&state, from) {
                return answer;
            }
        }
    }

    /// One lookup, to its end: never two at once, even when those waiting have given up.
    async fn look(self: Arc<Self>) {
        let found = (self.resolve)(&self.name).await;
        let addresses = match found {
            Ok(found) => {
                let mut addresses: Vec<IpAddr> = found.into_iter().map(|address| address.to_canonical()).filter(|address| !address.is_unspecified() && !address.is_multicast()).collect();
                addresses.sort();
                addresses.dedup();
                (!addresses.is_empty()).then_some(addresses)
            }
            Err(_) => None,
        };
        let _ = self.say(match &addresses {
            Some(addresses) => Said::At(addresses.clone()),
            None => Said::NotFound,
        });
        {
            let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.answer = Some((Instant::now(), addresses));
            state.looking = false;
        }
        self.told.send_modify(|count| *count += 1);
    }

    /// Writes what changed, if it did; whether it was written.
    fn say(&self, said: Said) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.said.as_ref() == Some(&said) {
            return false;
        }
        if self.log {
            match &said {
                Said::At(addresses) => {
                    let addresses: Vec<String> = addresses.iter().map(|address| address.to_string()).collect();
                    println!("NEXUS_PROXY {:?} is at {}: the proxy is answered from there.", self.name, addresses.join(", "));
                }
                Said::NotFound => println!("NEXUS_PROXY {:?} does not resolve now: no one is answered as the proxy until it does.", self.name),
                Said::TooSlow => println!("NEXUS_PROXY {:?} was not looked up in time: no one is answered as the proxy until it is.", self.name),
            }
        }
        state.said = Some(said);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run(test: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(test);
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    /// A resolver whose answer a test sets, counting its lookups, each taking `delay`.
    struct Stand {
        answer: Mutex<io::Result<Vec<IpAddr>>>,
        lookups: AtomicUsize,
        delay: Duration,
    }

    impl Stand {
        fn new(answer: &[&str], delay: Duration) -> Arc<Self> {
            Arc::new(Stand { answer: Mutex::new(Ok(answer.iter().map(|value| ip(value)).collect())), lookups: AtomicUsize::new(0), delay })
        }

        fn set(&self, answer: io::Result<Vec<IpAddr>>) {
            *self.answer.lock().unwrap() = answer;
        }

        fn resolver(self: &Arc<Self>) -> Resolver {
            let stand = self.clone();
            Arc::new(move |name: &str| {
                assert_eq!(name, "Nginx-Proxy-Manager-Official", "only the configured name is looked up");
                let stand = stand.clone();
                Box::pin(async move {
                    stand.lookups.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(stand.delay).await;
                    match &*stand.answer.lock().unwrap() {
                        Ok(found) => Ok(found.clone()),
                        Err(why) => Err(io::Error::new(why.kind(), "no such name")),
                    }
                })
            })
        }
    }

    const NAME: &str = "Nginx-Proxy-Manager-Official";
    const FRESH_T: Duration = Duration::from_millis(300);
    const WAIT_T: Duration = Duration::from_millis(400);

    fn named(stand: &Arc<Stand>) -> Proxy {
        Proxy::named_with(NAME.into(), stand.resolver(), false, FRESH_T, FRESH_T, WAIT_T)
    }

    #[test]
    fn the_setting_is_an_address_or_a_plain_name() {
        assert_eq!("172.18.0.2".parse(), Ok(ProxySetting::Address(ip("172.18.0.2"))));
        assert_eq!("::ffff:172.18.0.2".parse(), Ok(ProxySetting::Address(ip("172.18.0.2"))));
        assert_eq!("fd00::2".parse(), Ok(ProxySetting::Address(ip("fd00::2"))));
        for name in ["Nginx-Proxy-Manager-Official", "npm", "proxy_1", "proxy.internal", "npm-2.lan", "a"] {
            assert_eq!(name.parse(), Ok(ProxySetting::Name(name.into())), "{name}");
        }
        for wrong in [
            "",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "172.18.0.0/16",
            "172.18.0.2:80",
            "npm:81",
            "http://npm",
            "npm/",
            "npm proxy",
            " npm",
            "-npm",
            ".npm",
            "npm.",
            "npm..lan",
            "_npm",
            "np@m",
            "172.18.0",
            "1",
            "0x7f000001",
            "0x7f.1",
            "[fd00::2]",
            &"a".repeat(64),
            &format!("{}.lan", vec!["a".repeat(60); 5].join(".")),
        ] {
            assert_eq!(wrong.parse::<ProxySetting>(), Err(NotAProxy), "{wrong:?}");
        }
    }

    #[test]
    fn a_fixed_address_is_known_at_once() {
        run(async {
            let proxy = Proxy::from(ip("172.18.0.2"));
            assert_eq!(proxy.now(ip("::ffff:172.18.0.2")), Some(true));
            assert_eq!(proxy.now(ip("172.18.0.3")), Some(false));
            assert!(proxy.is(ip("172.18.0.2")).await);
        });
    }

    #[test]
    fn the_peer_at_the_name_is_the_proxy_and_another_on_the_network_is_not() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], Duration::ZERO);
            let proxy = named(&stand);
            assert_eq!(proxy.now(ip("172.18.0.5")), None, "nothing known before the first lookup");
            assert!(proxy.is(ip("172.18.0.5")).await);
            assert!(proxy.is(ip("::ffff:172.18.0.5")).await, "compared in one form");
            assert!(!proxy.is(ip("172.18.0.6")).await);
            assert_eq!(proxy.now(ip("172.18.0.5")), Some(true));
            assert_eq!(proxy.now(ip("172.18.0.6")), Some(false));
            assert_eq!(stand.lookups.load(Ordering::SeqCst), 1);
            // An IPv6 answer as well.
            let stand = Stand::new(&["fd00::5", "172.18.0.5"], Duration::ZERO);
            let proxy = named(&stand);
            assert!(proxy.is(ip("fd00::5")).await);
            assert!(!proxy.is(ip("fd00::6")).await);
        });
    }

    #[test]
    fn a_proxy_started_again_at_another_address_is_followed_within_the_answers_life() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], Duration::ZERO);
            let proxy = named(&stand);
            assert!(proxy.is(ip("172.18.0.5")).await);
            stand.set(Ok(vec![ip("172.18.0.9")]));
            // Within the answer's life, the old one still stands.
            assert!(!proxy.is(ip("172.18.0.9")).await);
            tokio::time::sleep(FRESH_T + Duration::from_millis(50)).await;
            assert!(proxy.is(ip("172.18.0.9")).await, "followed once the answer is old");
            assert!(!proxy.is(ip("172.18.0.5")).await, "and the old address is the proxy's no more");
            assert_eq!(stand.lookups.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn a_name_that_stops_resolving_makes_no_one_the_proxy_until_it_resolves_again() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], Duration::ZERO);
            let proxy = named(&stand);
            assert!(proxy.is(ip("172.18.0.5")).await);
            stand.set(Err(io::Error::new(io::ErrorKind::NotFound, "gone")));
            tokio::time::sleep(FRESH_T + Duration::from_millis(50)).await;
            for anyone in ["172.18.0.5", "172.18.0.6", "127.0.0.1"] {
                assert!(!proxy.is(ip(anyone)).await, "{anyone}");
            }
            // A name that answers with nothing is the same.
            stand.set(Ok(Vec::new()));
            tokio::time::sleep(FRESH_T + Duration::from_millis(50)).await;
            assert!(!proxy.is(ip("172.18.0.5")).await);
            stand.set(Ok(vec![ip("172.18.0.7")]));
            tokio::time::sleep(FRESH_T + Duration::from_millis(50)).await;
            assert!(proxy.is(ip("172.18.0.7")).await, "back once it resolves");
        });
    }

    #[test]
    fn a_lookup_not_answered_in_time_is_no_proxy_and_still_only_one() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], WAIT_T * 3);
            // Its answer, when it comes, believed long enough to be seen here.
            let proxy = Proxy::named_with(NAME.into(), stand.resolver(), false, Duration::from_secs(5), FRESH_T, WAIT_T);
            let began = Instant::now();
            assert!(!proxy.is(ip("172.18.0.5")).await, "closed, not open");
            assert!(began.elapsed() < WAIT_T + Duration::from_millis(200), "waited the bound, no more");
            // The lookup is still under way: no second one is begun.
            assert!(!proxy.is(ip("172.18.0.5")).await);
            assert_eq!(stand.lookups.load(Ordering::SeqCst), 1);
            // When it answers at last, its answer stands.
            tokio::time::sleep(WAIT_T * 2).await;
            assert!(proxy.is(ip("172.18.0.5")).await);
            assert_eq!(stand.lookups.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_burst_of_connections_makes_one_lookup() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], Duration::from_millis(100));
            let proxy = named(&stand);
            let asks: Vec<_> = (0..200)
                .map(|n| {
                    let proxy = proxy.clone();
                    tokio::spawn(async move { proxy.is(if n % 2 == 0 { ip("172.18.0.5") } else { ip("172.18.0.6") }).await })
                })
                .collect();
            for (n, ask) in asks.into_iter().enumerate() {
                assert_eq!(ask.await.unwrap(), n % 2 == 0, "{n}");
            }
            assert_eq!(stand.lookups.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn the_log_says_each_change_once() {
        run(async {
            let stand = Stand::new(&["172.18.0.5"], Duration::ZERO);
            let Proxy(Inner::Name(one)) = named(&stand) else { unreachable!() };
            let said: Vec<bool> = [
                Said::At(vec![ip("172.18.0.5")]),
                Said::At(vec![ip("172.18.0.5")]),
                Said::NotFound,
                Said::NotFound,
                Said::TooSlow,
                Said::At(vec![ip("172.18.0.9")]),
                Said::At(vec![ip("172.18.0.9")]),
            ]
            .into_iter()
            .map(|said| one.say(said))
            .collect();
            assert_eq!(said, [true, false, true, false, true, true, false]);
            // Through lookups: many connections, one line for each change.
            let stand = Stand::new(&["172.18.0.5"], Duration::ZERO);
            let Proxy(Inner::Name(two)) = named(&stand) else { unreachable!() };
            let proxy = Proxy(Inner::Name(two.clone()));
            for _ in 0..20 {
                assert!(!proxy.is(ip("172.18.0.6")).await);
            }
            assert!(!two.say(Said::At(vec![ip("172.18.0.5")])), "already said");
        });
    }
}
