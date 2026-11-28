//! The container's settings (design 13.2): read once as the program starts. A setting that is
//! wrong, or missing when it must be given, is said by name, and the program stops.
//!
//! None of them is secret, so the log may say what they are.

use std::net::SocketAddr;

pub use crate::proxy::ProxySetting;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The music server as the container reaches it, e.g. `http://navidrome:4533`. Not used
    /// at stage 1's first slice; checked all the same, so that a wrong one is said now.
    pub music_server: String,
    /// Who the proxy is: the one address it connects to this container from, or the name of
    /// its container on a network they share, looked up as connections come ([`crate::proxy`]).
    pub proxy: ProxySetting,
    pub data: PathBuf,
    pub state: PathBuf,
    pub listen: SocketAddr,
    /// The admin's page's own origin, exactly as a browser sends it in `Origin`
    /// (`NEXUS_PUBLIC_ORIGIN`, the lead's ruling of 13:46; design 7.3, 19.2): `https://` and a
    /// name, and a port if not 443, nothing else. The page's two calls with no session (its
    /// sign-in and setting up) are taken only from it. Unset, they are refused.
    pub public_origin: Option<String>,
    /// How large the data may grow, in bytes, before new events wait (`NEXUS_DATA_BUDGET`,
    /// design 19.2; S12's intake). 4 GB unless given.
    pub data_budget: u64,
}

pub const DATA_DEFAULT: &str = "/data";
pub const STATE_DEFAULT: &str = "/state";
pub const LISTEN_DEFAULT: &str = "0.0.0.0:8080";
pub const DATA_BUDGET_DEFAULT: u64 = 4_000_000_000;
/// Less than this is surely a slip (a number of gigabytes written without its unit, say).
pub const DATA_BUDGET_LEAST: u64 = 1_000_000;

impl Settings {
    /// Reads the settings through `get` (the environment, or a test's table). A value that is
    /// empty or only spaces counts as not given.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let setting = |name: &str| get(name).map(|value| value.trim().to_string()).filter(|value| !value.is_empty());

        let music_server = match setting("NEXUS_MUSIC_SERVER") {
            None => return Err("NEXUS_MUSIC_SERVER is missing: give the music server as this container reaches it, for example http://navidrome:4533".into()),
            Some(value) => music_server(&value).ok_or_else(|| {
                "NEXUS_MUSIC_SERVER is wrong: give http:// or https://, a host, and if need be a port and a path, with nothing else (no name, password, query or #), for example http://navidrome:4533".to_string()
            })?,
        };
        let proxy = match setting("NEXUS_PROXY") {
            None => return Err("NEXUS_PROXY is missing: give the one address the proxy connects to this container from, for example 172.18.0.2, or the name of the proxy's container on a network it shares with this one, for example Nginx-Proxy-Manager-Official".into()),
            Some(value) => value.parse::<ProxySetting>().map_err(|_| {
                "NEXUS_PROXY is wrong: give one IP address (not a range or a port), the one the proxy connects to this container from, for example 172.18.0.2, or the plain name of the proxy's container (no scheme, port, path or space), for example Nginx-Proxy-Manager-Official".to_string()
            })?,
        };
        let folder = |name: &str, default: &str| -> Result<PathBuf, String> {
            let value = setting(name).unwrap_or_else(|| default.to_string());
            let path = PathBuf::from(&value);
            // Read as written: a `.` or `..` in it, or a doubled slash, is not tidied away.
            let plain = value.starts_with('/')
                && !value.contains("//")
                && !value.split('/').any(|segment| segment == "." || segment == "..")
                && path.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
            if plain { Ok(path) } else { Err(format!("{name} is wrong: give a folder as a whole path from /, with no . or .. in it, for example {default}")) }
        };
        let data = folder("NEXUS_DATA", DATA_DEFAULT)?;
        let state = folder("NEXUS_STATE", STATE_DEFAULT)?;
        if state.starts_with(&data) {
            return Err("NEXUS_STATE is wrong: it must not be the data folder (NEXUS_DATA) or inside it, since a copy of the data must not hold it".into());
        }
        if data.starts_with(&state) {
            return Err("NEXUS_DATA is wrong: it must not be inside the folder of the store kept apart (NEXUS_STATE)".into());
        }
        let listen = setting("NEXUS_LISTEN").unwrap_or_else(|| LISTEN_DEFAULT.to_string());
        let listen = match listen.parse::<SocketAddr>() {
            // Port 0 would be a port chosen afresh at each start, which the proxy and the
            // health check could not know.
            Ok(listen) if listen.port() != 0 => listen,
            _ => return Err(format!("NEXUS_LISTEN is wrong: give an address and a port other than 0, for example {LISTEN_DEFAULT}")),
        };
        let public_origin = match setting("NEXUS_PUBLIC_ORIGIN") {
            None => None,
            Some(value) => Some(public_origin(&value).ok_or_else(|| {
                "NEXUS_PUBLIC_ORIGIN is wrong: give the admin's page's address as a browser has it, https:// and the name (and :port if it is not 443), with nothing after it, for example https://nexus.example.org".to_string()
            })?),
        };
        let data_budget = match setting("NEXUS_DATA_BUDGET") {
            None => DATA_BUDGET_DEFAULT,
            Some(value) => data_budget(&value).ok_or_else(|| {
                format!("NEXUS_DATA_BUDGET is wrong: give how large the data may grow, as a whole number of bytes or with MB, GB or TB after it, at least 1 MB, for example 4GB (the default is {DATA_BUDGET_DEFAULT} bytes)")
            })?,
        };
        Ok(Settings { music_server, proxy, data, state, listen, public_origin, data_budget })
    }

    /// Whether both folders are there, and apart on the disk as well as in their names: not
    /// the same folder through a link or a second mount, and neither inside the other once
    /// links are followed. The store kept apart is written to; the data folder is not used yet.
    pub fn check_folders(&self) -> Result<(), String> {
        let mut found = Vec::new();
        for (name, path) in [("NEXUS_DATA", &self.data), ("NEXUS_STATE", &self.state)] {
            match std::fs::canonicalize(path) {
                Ok(real) if real.is_dir() => found.push(real),
                _ => return Err(format!("{name} is wrong: there is no folder there")),
            }
        }
        let (data, state) = (&found[0], &found[1]);
        if state.starts_with(data) || data.starts_with(state) || same_folder(data, state) {
            return Err("NEXUS_STATE is wrong: on the disk it is the data folder (NEXUS_DATA), inside it, or around it, through a link or a mount; a copy of the data must not hold it".into());
        }
        Ok(())
    }
}

/// An origin as a browser writes it in `Origin`: `https://`, a host in its one plain form (as
/// the shared reading of an address gives it, 7.1), and a port only if not 443. Anything else,
/// or anything not already in that form, is not one.
fn public_origin(value: &str) -> Option<String> {
    let rest = value.strip_prefix("https://")?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return None;
    }
    let origin = nexus_core::nexus::account::NexusOrigin::typed(value).ok()?;
    let plain = origin.origin().to_string();
    (plain == value).then_some(plain)
}

/// A size: a whole number of bytes, or of MB, GB or TB (a thousand to the next; the unit in any
/// case, a space before it or not), at least [`DATA_BUDGET_LEAST`].
fn data_budget(value: &str) -> Option<u64> {
    let digits = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    let (number, unit) = value.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let times: u64 = match unit.trim_start().to_ascii_uppercase().as_str() {
        "" => 1,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return None,
    };
    number.checked_mul(times).filter(|bytes| *bytes >= DATA_BUDGET_LEAST)
}

/// Whether two paths are one folder on the disk (two mounts of it, say).
#[cfg(unix)]
fn same_folder(one: &Path, other: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(one), std::fs::metadata(other)) {
        (Ok(one), Ok(other)) => one.dev() == other.dev() && one.ino() == other.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_folder(_: &Path, _: &Path) -> bool {
    false
}

/// The music server's address if it is one: `http` or `https`, a host (a name, an IPv4
/// address or an IPv6 one in brackets), a port if given, a path if given, and nothing else.
/// A slash at the end is dropped.
fn music_server(value: &str) -> Option<String> {
    let rest = value.strip_prefix("http://").or_else(|| value.strip_prefix("https://"))?;
    let (place, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let (host, port) = match place.rfind(':') {
        Some(at) if !place[at..].contains(']') => (&place[..at], Some(&place[at + 1..])),
        _ => (place, None),
    };
    let host_right = match host.strip_prefix('[') {
        Some(inside) => inside.strip_suffix(']').is_some_and(|v6| v6.parse::<std::net::Ipv6Addr>().is_ok()),
        None => {
            !host.is_empty()
                && host.len() <= 253
                && !host.starts_with(['.', '-'])
                && host.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        }
    };
    let port_right = port.is_none_or(|port| {
        !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|port| port != 0)
    });
    let path_right = path.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~'))
        && !path.contains("//")
        && !path.split('/').any(|segment| segment == "." || segment == "..");
    (host_right && port_right && path_right).then(|| value.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::IpAddr;

    #[cfg(unix)]
    #[test]
    fn the_public_origin_is_taken_only_as_a_browser_writes_one() {
        for good in ["https://nexus.example.org", "https://nexus.example.org:8443"] {
            assert_eq!(public_origin(good).as_deref(), Some(good), "{good}");
        }
        for bad in ["http://nexus.example.org", "nexus.example.org", "https://nexus.example.org/", "https://nexus.example.org/nexus/", "https://Nexus.example.org", "https://nexus.example.org:443", "https://a@nexus.example.org", "https://nexus.example.org?x", "https://"] {
            assert_eq!(public_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn folders_apart_in_name_but_not_on_the_disk_are_refused() {
        let root = std::env::temp_dir().join(format!("nexus-server-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::os::unix::fs::symlink(root.join("data"), root.join("state-link")).unwrap();
        std::fs::create_dir_all(root.join("data/inner")).unwrap();
        std::os::unix::fs::symlink(root.join("data/inner"), root.join("inner-link")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let settings = |state: &str| Settings {
            music_server: "http://navidrome:4533".into(),
            proxy: "172.18.0.2".parse().unwrap(),
            data: root.join("data"),
            state: root.join(state),
            listen: "0.0.0.0:8080".parse().unwrap(),
            public_origin: None,
            data_budget: DATA_BUDGET_DEFAULT,
        };
        assert_eq!(settings("state").check_folders(), Ok(()));
        for state in ["state-link", "inner-link"] {
            assert!(settings(state).check_folders().unwrap_err().starts_with("NEXUS_STATE is wrong"), "{state}");
        }
        assert!(settings("nowhere").check_folders().unwrap_err().starts_with("NEXUS_STATE is wrong: there is no folder"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn read(pairs: &[(&str, &str)]) -> Result<Settings, String> {
        let table: HashMap<String, String> = pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
        Settings::read(|name| table.get(name).cloned())
    }

    const BOTH: &[(&str, &str)] = &[("NEXUS_MUSIC_SERVER", "http://navidrome:4533"), ("NEXUS_PROXY", "172.18.0.2")];

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut pairs: Vec<_> = BOTH.iter().copied().filter(|(name, _)| !extra.iter().any(|(other, _)| other == name)).collect();
        pairs.extend_from_slice(extra);
        pairs
    }

    #[test]
    fn two_settings_given_and_the_rest_by_default() {
        let settings = read(BOTH).unwrap();
        assert_eq!(settings.music_server, "http://navidrome:4533");
        assert_eq!(settings.proxy, "172.18.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(settings.data, PathBuf::from("/data"));
        assert_eq!(settings.state, PathBuf::from("/state"));
        assert_eq!(settings.listen, "0.0.0.0:8080".parse().unwrap());
    }

    #[test]
    fn a_missing_setting_is_named() {
        for (missing, value) in [("NEXUS_MUSIC_SERVER", ""), ("NEXUS_PROXY", "   ")] {
            let pairs = with(&[(missing, value)]);
            let why = read(&pairs).unwrap_err();
            assert!(why.starts_with(&format!("{missing} is missing")), "{why}");
        }
        let why = read(&[("NEXUS_PROXY", "172.18.0.2")]).unwrap_err();
        assert!(why.starts_with("NEXUS_MUSIC_SERVER is missing"), "{why}");
    }

    #[test]
    fn a_wrong_setting_is_named() {
        let cases: &[(&str, &str)] = &[
            ("NEXUS_MUSIC_SERVER", "navidrome:4533"),
            ("NEXUS_MUSIC_SERVER", "ftp://navidrome"),
            ("NEXUS_MUSIC_SERVER", "http://"),
            ("NEXUS_MUSIC_SERVER", "http://user:secret@navidrome:4533"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:4533/?u=me"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:4533/#x"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:port"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:0"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:65536"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome:"),
            ("NEXUS_MUSIC_SERVER", "http://navi drome"),
            ("NEXUS_MUSIC_SERVER", "http://-navidrome"),
            ("NEXUS_MUSIC_SERVER", "http://[::1"),
            ("NEXUS_MUSIC_SERVER", "http://[nope]:4533"),
            ("NEXUS_MUSIC_SERVER", "http://[::1]x"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome/../rest"),
            ("NEXUS_MUSIC_SERVER", "http://navidrome//music"),
            ("NEXUS_PROXY", "172.18.0.0/16"),
            ("NEXUS_PROXY", "http://npm"),
            ("NEXUS_PROXY", "npm:81"),
            ("NEXUS_PROXY", "npm/"),
            ("NEXUS_PROXY", "npm proxy"),
            ("NEXUS_PROXY", "172.18.0"),
            ("NEXUS_PROXY", "172.18.0.2:80"),
            ("NEXUS_PROXY", "0.0.0.0"),
            ("NEXUS_PROXY", "::"),
            ("NEXUS_PROXY", "224.0.0.1"),
            ("NEXUS_DATA", "data"),
            ("NEXUS_DATA", "/data/../state"),
            ("NEXUS_DATA", "/data/./x"),
            ("NEXUS_STATE", "./state"),
            ("NEXUS_STATE", "/data"),
            ("NEXUS_STATE", "/data/state"),
            ("NEXUS_LISTEN", "8080"),
            ("NEXUS_LISTEN", "localhost:8080"),
            ("NEXUS_LISTEN", "0.0.0.0:99999"),
            ("NEXUS_LISTEN", "0.0.0.0:0"),
            ("NEXUS_PROXY", "::ffff:0.0.0.0"),
            ("NEXUS_PROXY", "::ffff:224.0.0.1"),
        ];
        for &(name, value) in cases {
            let pairs = with(&[(name, value)]);
            let why = read(&pairs).unwrap_err();
            assert!(why.starts_with(&format!("{name} is wrong")), "{name}={value}: {why}");
        }
        let pairs = with(&[("NEXUS_DATA", "/srv/nexus"), ("NEXUS_STATE", "/srv")]);
        assert!(read(&pairs).unwrap_err().starts_with("NEXUS_DATA is wrong"));
    }

    #[test]
    fn right_settings_are_taken() {
        for value in [
            "http://navidrome:4533",
            "https://music.example.org",
            "http://192.0.2.20:4533/music/",
            "http://[fd00::20]:4533",
            "http://[fd00::20]",
            "http://navidrome.local:4533/a-b_c.d~e",
        ] {
            let pairs = with(&[("NEXUS_MUSIC_SERVER", value)]);
            assert_eq!(read(&pairs).unwrap().music_server, value.trim_end_matches('/'), "{value}");
        }
        let pairs = with(&[("NEXUS_PROXY", "::ffff:172.18.0.2"), ("NEXUS_DATA", "/srv/data"), ("NEXUS_STATE", "/srv/state"), ("NEXUS_LISTEN", "[::]:9000")]);
        let settings = read(&pairs).unwrap();
        assert_eq!(settings.proxy, "172.18.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(settings.listen, "[::]:9000".parse().unwrap());
        // Folders that only begin alike are apart.
        let pairs = with(&[("NEXUS_DATA", "/srv/data"), ("NEXUS_STATE", "/srv/data-state")]);
        assert!(read(&pairs).is_ok());
    }

    /// The lead's ruling of 28 Nov: the proxy by its container's name, when its address is not fixed.
    #[test]
    fn the_proxy_may_be_named_by_its_containers_name() {
        for name in ["Nginx-Proxy-Manager-Official", "npm", "proxy_1.lan"] {
            let pairs = with(&[("NEXUS_PROXY", name)]);
            assert_eq!(read(&pairs).unwrap().proxy, ProxySetting::Name(name.into()), "{name}");
        }
        let pairs = with(&[("NEXUS_PROXY", "fd00::2")]);
        assert_eq!(read(&pairs).unwrap().proxy, ProxySetting::Address("fd00::2".parse().unwrap()));
    }

    /// S12's audit, S1: a data budget mistyped stops the start, by name; a right one is taken.
    #[test]
    fn the_data_budget_is_a_size_or_the_start_stops() {
        let base = [("NEXUS_MUSIC_SERVER", "http://navidrome:4533"), ("NEXUS_PROXY", "172.18.0.2")];
        let with_budget = |value: &'static str| {
            let mut pairs = base.to_vec();
            pairs.push(("NEXUS_DATA_BUDGET", value));
            read(&pairs)
        };
        assert_eq!(read(&base).unwrap().data_budget, 4_000_000_000);
        for (given, bytes) in [("4GB", 4_000_000_000), ("4 gb", 4_000_000_000), ("500MB", 500_000_000), ("2000000000", 2_000_000_000), ("1TB", 1_000_000_000_000)] {
            assert_eq!(with_budget(given).unwrap().data_budget, bytes, "{given}");
        }
        for wrong in ["4G", "4.5GB", "-1", "4", "0", "lots", "4GiB", "99999999999999999999"] {
            assert!(with_budget(wrong).unwrap_err().starts_with("NEXUS_DATA_BUDGET is wrong"), "{wrong}");
        }
    }
}
