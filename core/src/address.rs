//! A server's address as a person types it: how it's tidied, which addresses are tried for
//! it, and when sending something over plain HTTP would let others on the way read it.
//!
//! The same rules decide how the app signs in to the music server and which jam relay
//! links it will take, on every device.

/// Tidies what the user typed into a base URL: adds a scheme (in small letters), drops
/// trailing slashes and any `/app/...` path copied from the browser's address bar, and the
/// `/rest` the API lives under. Only the path after the server's name and port is tidied, so
/// a server named `rest` or `app` keeps its name.
pub fn normalize_server_url(input: &str) -> String {
    let typed = input.trim();
    if typed.is_empty() {
        return String::new();
    }
    let (scheme, rest) = match typed.split_once("://") {
        // "HTTP://" and "http://" are the same server, and the same account.
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => ("http".to_string(), typed),
    };
    let (host, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let mut path = path.to_string();
    if let Some(pos) = path.find("/app/") {
        path.truncate(pos);
    }
    // Trailing slashes first, so ".../rest/" loses its "/rest" as well, and the first
    // request doesn't go to ".../rest/rest/ping".
    let trim = |path: &mut String| {
        while path.ends_with('/') || path.ends_with('#') {
            path.pop();
        }
    };
    trim(&mut path);
    for suffix in ["/app", "/rest"] {
        if path.ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
        }
    }
    trim(&mut path);
    format!("{scheme}://{host}{path}")
}

/// The addresses to try for what was typed as the server's. One that says "http://" or
/// "https://" is taken at its word. One that doesn't is tried over HTTPS first, and over
/// plain HTTP as well only when it looks like a home-network address (see `https_only`).
pub fn addresses_to_try(typed: &str) -> Vec<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        Vec::new()
    } else if typed.contains("://") {
        vec![typed.to_string()]
    } else if https_only(typed) {
        vec![format!("https://{typed}")]
    } else {
        vec![format!("https://{typed}"), format!("http://{typed}")]
    }
}

/// Whether what was typed as the server's address is tried over HTTPS alone: it names no
/// scheme, and doesn't look like an address on the home network, by the same reading of it
/// as `unencrypted_beyond_home`. Beyond home, a sign-in goes over plain HTTP only if
/// "http://" was typed.
pub fn https_only(typed: &str) -> bool {
    let typed = typed.trim();
    !typed.is_empty() && !typed.contains("://") && unencrypted_beyond_home(&format!("http://{typed}"))
}

/// Whether an address is plain HTTP to somewhere that isn't on the home network (or this
/// PC): the case where what's sent can be read by others on the way.
pub fn unencrypted_beyond_home(address: &str) -> bool {
    let Some(rest) = address.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    // An IPv6 address is in brackets, with the port after them.
    let host = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    }
    .to_lowercase();
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let [a, b, ..] = ip.octets();
        // Private ranges, this PC, link-local, and the range Tailscale and similar use.
        return !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || (a == 100 && (64..128).contains(&b)));
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        return !(ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80);
    }
    let local = ["localhost", ".local", ".lan", ".home", ".internal", ".home.arpa", ".localdomain"];
    !(host.is_empty() || !host.contains('.') || local.iter().any(|ending| host == ending.trim_start_matches('.') || host.ends_with(ending)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tries_https_first_and_knows_when_plain_http_matters() {
        // With no scheme typed, plain HTTP is tried as well only for an address that looks
        // like a home one.
        assert_eq!(addresses_to_try(" music.example.org "), ["https://music.example.org"]);
        assert_eq!(addresses_to_try("music.example.org:4533"), ["https://music.example.org:4533"]);
        assert_eq!(addresses_to_try("8.8.8.8:4533"), ["https://8.8.8.8:4533"]);
        assert_eq!(addresses_to_try("192.168.1.5:4533"), ["https://192.168.1.5:4533", "http://192.168.1.5:4533"]);
        assert_eq!(addresses_to_try("nas:4533"), ["https://nas:4533", "http://nas:4533"]);
        assert_eq!(addresses_to_try("music.lan"), ["https://music.lan", "http://music.lan"]);
        assert_eq!(addresses_to_try("localhost:4533"), ["https://localhost:4533", "http://localhost:4533"]);
        assert!(https_only(" music.example.org ") && https_only("8.8.8.8"));
        assert!(!https_only("nas:4533") && !https_only("192.168.1.5") && !https_only("http://music.example.org") && !https_only(" "));
        // Typed with a scheme it's taken at its word, in whatever case.
        assert_eq!(addresses_to_try("http://192.168.1.5:4533"), ["http://192.168.1.5:4533"]);
        assert_eq!(addresses_to_try("http://music.example.org"), ["http://music.example.org"]);
        assert_eq!(addresses_to_try("HTTP://music.example.org"), ["HTTP://music.example.org"]);
        assert_eq!(addresses_to_try("https://music.example.org/"), ["https://music.example.org/"]);
        assert!(addresses_to_try("  ").is_empty());
        // Plain HTTP only matters where someone else could be listening.
        for home in ["http://192.168.1.5:4533", "http://10.0.0.2", "http://localhost:4533", "http://nas:4533", "http://nas.local", "http://music.lan/", "http://100.96.1.2:4533", "http://127.0.0.1:4533", "http://[::1]:4533", "http://[fd00::5]:4533"] {
            assert!(!unencrypted_beyond_home(home), "{home}");
        }
        for away in ["http://music.example.org", "http://music.example.org:4533/x", "http://8.8.8.8:4533", "http://100.200.1.2", "http://[2001:db8::1]:4533"] {
            assert!(unencrypted_beyond_home(away), "{away}");
        }
        assert!(!unencrypted_beyond_home("https://music.example.org"));
    }

    #[test]
    fn normalizes_typed_addresses() {
        assert_eq!(normalize_server_url("192.168.1.10:4533"), "http://192.168.1.10:4533");
        assert_eq!(normalize_server_url(" https://music.example.com/ "), "https://music.example.com");
        assert_eq!(
            normalize_server_url("http://tower:4533/app/#/album"),
            "http://tower:4533"
        );
        assert_eq!(normalize_server_url("http://tower/music/"), "http://tower/music");
        // A trailing slash doesn't keep "/rest" or "/app" in.
        assert_eq!(normalize_server_url("https://music.example.org/rest/"), "https://music.example.org");
        assert_eq!(normalize_server_url("https://music.example.org/rest"), "https://music.example.org");
        assert_eq!(normalize_server_url("https://music.example.org/navidrome/rest//"), "https://music.example.org/navidrome");
        assert_eq!(normalize_server_url("http://tower:4533/app/"), "http://tower:4533");
        assert_eq!(normalize_server_url("http://tower:4533/app#/"), "http://tower:4533");
        // The scheme in small letters, so it's the same account however it was typed, and
        // the checks on it see it for what it is.
        assert_eq!(normalize_server_url("HTTP://tower:4533"), "http://tower:4533");
        assert_eq!(normalize_server_url("HTTPS://music.example.org/rest/"), "https://music.example.org");
        // Only the path is tidied: a server named "rest" or "app" keeps its name.
        assert_eq!(normalize_server_url("http://rest/"), "http://rest");
        assert_eq!(normalize_server_url("http://rest"), "http://rest");
        assert_eq!(normalize_server_url("http://rest/rest/"), "http://rest");
        assert_eq!(normalize_server_url("http://app/app/#/"), "http://app");
        assert_eq!(normalize_server_url("rest"), "http://rest");
        assert_eq!(normalize_server_url("app:4533/app/"), "http://app:4533");
        assert_eq!(normalize_server_url("  "), "");
    }
}
