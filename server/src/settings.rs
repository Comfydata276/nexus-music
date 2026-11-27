//! The container's settings (design 13.2): read once as the program starts. A setting that is
//! wrong, or missing when it must be given, is said by name, and the program stops.
//!
//! None of them is secret, so the log may say what they are.

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The music server as the container reaches it, e.g. `http://navidrome:4533`. Not used
    /// at stage 1's first slice; checked all the same, so that a wrong one is said now.
    pub music_server: String,
    /// The one address the proxy connects to this container from.
    pub proxy: IpAddr,
    pub data: PathBuf,
    pub state: PathBuf,
    pub listen: SocketAddr,
}

pub const DATA_DEFAULT: &str = "/data";
pub const STATE_DEFAULT: &str = "/state";
pub const LISTEN_DEFAULT: &str = "0.0.0.0:8080";

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
            None => return Err("NEXUS_PROXY is missing: give the one address the proxy connects to this container from, for example 172.18.0.2".into()),
            Some(value) => match value.parse::<IpAddr>() {
                Ok(address) if !address.is_unspecified() && !address.is_multicast() => address.to_canonical(),
                _ => return Err("NEXUS_PROXY is wrong: give one IP address (not a name, a range or a port), the one the proxy connects to this container from, for example 172.18.0.2".into()),
            },
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
        let listen = listen
            .parse::<SocketAddr>()
            .map_err(|_| format!("NEXUS_LISTEN is wrong: give an address and a port, for example {LISTEN_DEFAULT}"))?;
        Ok(Settings { music_server, proxy, data, state, listen })
    }

    /// Whether both folders are there. The store kept apart is written to; the data folder is
    /// not used yet.
    pub fn check_folders(&self) -> Result<(), String> {
        for (name, path) in [("NEXUS_DATA", &self.data), ("NEXUS_STATE", &self.state)] {
            if !Path::new(path).is_dir() {
                return Err(format!("{name} is wrong: there is no folder at {}", path.display()));
            }
        }
        Ok(())
    }
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
            ("NEXUS_PROXY", "npm"),
            ("NEXUS_PROXY", "172.18.0.0/16"),
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
            "http://192.168.1.20:4533/music/",
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
}
