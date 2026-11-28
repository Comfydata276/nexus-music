//! Canonical addresses for signed requests. Parse the original text before an HTTP
//! library has a chance to repair it. Compare its actual request with these components
//! again at dispatch. No name resolution or network classification takes place here.

use std::{fmt, net::Ipv6Addr};

// the design does not say how large an address may be. Bound the original spelling,
// the canonical wire form, and the result of resolving or building a request.
pub const MAX_ADDRESS_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressError {
    TooLong,
    Scheme,
    Host,
    TrailingDot,
    Userinfo,
    EmbeddedIpv4,
    Zone,
    EmptyPort,
    Port,
    Character,
    Path,
    Query,
    Fragment,
    BaseQuery,
    NexusPath,
    HandoverSource,
}

impl AddressError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooLong => "it is too long",
            Self::Scheme => "it needs to start with https://",
            Self::Host => "it has characters a server name can't have",
            Self::TrailingDot => "its name ends in a dot",
            Self::Userinfo => "it has a name or password in it",
            Self::EmbeddedIpv4 => "its IPv6 address contains an IPv4 address",
            Self::Zone => "its IPv6 address has a zone",
            Self::EmptyPort => "its port is missing",
            Self::Port => "its port must be a number from 1 to 65535",
            Self::Character => "it contains a space, backslash or control character",
            Self::Path => "its path is not in plain form",
            Self::Query => "its query is not in encoded form",
            Self::Fragment => "its fragment is not in encoded form",
            Self::BaseQuery => "a server address cannot include a query",
            Self::NexusPath => "its path belongs to the Nexus server",
            Self::HandoverSource => "the address did not come from this device's Nexus sign-in",
        }
    }

    pub fn typed_words(self) -> String {
        if self == Self::Scheme {
            return "Nexus Music needs an https:// address.".into();
        }
        format!(
            "Nexus Music can't use this address: {}. Use the server's plain name.",
            self.reason()
        )
    }

    pub fn handover_words(self) -> String {
        format!(
            "Your Nexus server gave the app a music server address it can't use: {}. Tell whoever runs it.",
            self.reason()
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    scheme: &'static str,
    host: String,
    port: u16,
}

impl Origin {
    pub fn scheme(&self) -> &'static str {
        self.scheme
    }
    /// Lower-case ASCII name, decimal IPv4, or canonical IPv6 including its brackets.
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }

    fn default_port(&self) -> u16 {
        if self.scheme == "https" { 443 } else { 80 }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.host)?;
        if self.port != self.default_port() {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct Address {
    origin: Origin,
    path: String,
    query: Option<String>,
}

// The wire spelling may contain a credential. Obtain it explicitly with wire().
impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Address")
            .field("origin", &self.origin)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn check_length(text: &str) -> Result<(), AddressError> {
    if text.len() > MAX_ADDRESS_BYTES {
        Err(AddressError::TooLong)
    } else {
        Ok(())
    }
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn check_path(path: &str) -> Result<(), AddressError> {
    if path.bytes().any(|b| !unreserved(b) && b != b'/')
        || path.contains("//")
        || path.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err(AddressError::Path);
    }
    Ok(())
}

// the design does not say which bytes an encoded query or fragment may contain.
// RFC 3986 sections 3.4 and 3.5 use the same grammar. Never decode or re-encode it.
fn encoded(text: &str) -> bool {
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            if !bytes.next().is_some_and(|c| c.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|c| c.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !unreserved(b) && !b"!$&'()*+,;=:@/?".contains(&b) {
            return false;
        }
    }
    true
}

// Separate these before reading the path. In particular, a slash, colon, or question
// mark in the query or fragment cannot become part of the authority or path.
fn components(text: &str) -> Result<(&str, Option<&str>), AddressError> {
    check_length(text)?;
    if text.bytes().any(|b| b <= b' ' || matches!(b, b'\\' | 0x7f)) {
        return Err(AddressError::Character);
    }
    let (before, fragment) = text.split_once('#').map_or((text, None), |(u, f)| (u, Some(f)));
    if fragment.is_some_and(|f| !encoded(f)) {
        return Err(AddressError::Fragment);
    }
    let (hier, query) = before.split_once('?').map_or((before, None), |(p, q)| (p, Some(q)));
    if query.is_some_and(|q| !encoded(q)) {
        return Err(AddressError::Query);
    }
    Ok((hier, query))
}

fn read_origin(scheme: &'static str, authority: &str) -> Result<Origin, AddressError> {
    if authority.contains('@') {
        return Err(AddressError::Userinfo);
    }
    let (host, port) = if let Some(ip) = authority.strip_prefix('[') {
        let (ip, tail) = ip.split_once(']').ok_or(AddressError::Host)?;
        if ip.contains('%') {
            return Err(AddressError::Zone);
        }
        if ip.contains('.') {
            return Err(AddressError::EmbeddedIpv4);
        }
        let ip: Ipv6Addr = ip.parse().map_err(|_| AddressError::Host)?;
        // the design does not say whether the obsolete compatible prefix in pure hex
        // counts as embedded IPv4. Refuse it too, except for :: and ::1 (native IPv6).
        if ip.to_ipv4_mapped().is_some() || (ip.to_ipv4().is_some() && !ip.is_loopback() && !ip.is_unspecified()) {
            return Err(AddressError::EmbeddedIpv4);
        }
        let port = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':').ok_or(AddressError::Host)?)
        };
        (format!("[{ip}]"), port)
    } else {
        let (name, port) = authority
            .split_once(':')
            .map_or((authority, None), |(n, p)| (n, Some(p)));
        if name.ends_with('.') {
            return Err(AddressError::TrailingDot);
        }
        if name.len() > 253
            || !name.split('.').all(|l| {
                (1..=63).contains(&l.len())
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            })
        {
            return Err(AddressError::Host);
        }
        // Strict dotted decimal is already in canonical form. Other numeric names stay text.
        (name.to_ascii_lowercase(), port)
    };
    let port = match port {
        None if scheme == "https" => 443,
        None => 80,
        Some("") => return Err(AddressError::EmptyPort),
        // the design does not say how much zero padding to accept. Keep the probe's
        // five-digit bound, then write the number without padding.
        Some(p) if p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
            p.parse::<u16>().ok().filter(|n| *n > 0).ok_or(AddressError::Port)?
        }
        Some(_) => return Err(AddressError::Port),
    };
    Ok(Origin { scheme, host, port })
}

impl Address {
    /// An already-built or handed-over address requires an explicit HTTPS scheme.
    pub fn parse(text: &str) -> Result<Self, AddressError> {
        Self::read(text, false)
    }
    /// Only a typed server address may omit its scheme.
    pub fn typed(text: &str) -> Result<Self, AddressError> {
        Self::read(text, true)
    }

    fn read(text: &str, typed: bool) -> Result<Self, AddressError> {
        let (hier, query) = components(text)?;
        let rest = match hier.split_once("://") {
            Some((s, rest)) if s.eq_ignore_ascii_case("https") => rest,
            None if typed => hier,
            _ => return Err(AddressError::Scheme),
        };
        Self::authority_path("https", rest, query)
    }

    fn authority_path(scheme: &'static str, rest: &str, query: Option<&str>) -> Result<Self, AddressError> {
        let end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        Self::from_parts(read_origin(scheme, authority)?, path, query)
    }

    fn from_parts(origin: Origin, path: &str, query: Option<&str>) -> Result<Self, AddressError> {
        let path = if path.is_empty() { "/" } else { path };
        check_path(path)?;
        let address = Self {
            origin,
            path: path.into(),
            query: query.map(str::to_owned),
        };
        check_length(&address.wire())?;
        Ok(address)
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }
    /// Compare the HTTP stack's parsed request immediately before sending. Supply the
    /// effective port and an IPv6 host with its URI brackets, without re-parsing the
    /// stack's encoded path or query. An absent query differs from an empty one.
    pub fn matches_request(&self, scheme: &str, host: &str, port: u16, path: &str, query: Option<&str>) -> bool {
        scheme == self.origin.scheme
            && host == self.origin.host
            && port == self.origin.port
            && path == self.path
            && query == self.query()
    }

    pub fn wire(&self) -> String {
        let mut s = format!("{}{}", self.origin, self.path);
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        s
    }

    pub(crate) fn under_nexus(&self) -> bool {
        // the design does not say whether the namespace is case-sensitive. Keep the
        // probe's case-insensitive guard; compare complete first segments only.
        self.path
            .split('/')
            .nth(1)
            .is_some_and(|s| s.eq_ignore_ascii_case("nexus"))
    }

    /// Resolve a reference for an HTTPS line. A redirect still needs the music verdict
    /// before any credential can travel with it.
    pub fn resolve(&self, reference: &str) -> Result<Self, AddressError> {
        let target = self.location(reference)?;
        if target.origin.scheme != "https" {
            return Err(AddressError::Scheme);
        }
        Ok(target)
    }

    /// RFC 3986 section 5 on the allowed grammar. Reject a raw dot segment before
    /// merging: resolving it away could make a forbidden path appear to be the request.
    /// HTTP is representable here solely so the judge can report a valid downgrade.
    pub(crate) fn location(&self, text: &str) -> Result<Self, AddressError> {
        let (hier, query) = components(text)?;
        let first = hier.split('/').next().unwrap_or_default();
        if first.contains(':') {
            let (scheme, rest) = hier.split_once("://").ok_or(AddressError::Scheme)?;
            let scheme = if scheme.eq_ignore_ascii_case("https") {
                "https"
            } else if scheme.eq_ignore_ascii_case("http") {
                "http"
            } else {
                return Err(AddressError::Scheme);
            };
            return Self::authority_path(scheme, rest, query);
        }
        if let Some(rest) = hier.strip_prefix("//") {
            return Self::authority_path(self.origin.scheme, rest, query);
        }
        if hier.is_empty() {
            return Self::from_parts(self.origin.clone(), &self.path, query.or(self.query()));
        }
        if hier.starts_with('/') {
            return Self::from_parts(self.origin.clone(), hier, query);
        }
        let (parent, _) = self.path.rsplit_once('/').ok_or(AddressError::Path)?;
        Self::from_parts(self.origin.clone(), &format!("{parent}/{hier}"), query)
    }
}

/// Only the transport opening the answer can attest to the line and device credential.
/// Supply the address on the dispatched request, never a URL supplied by its response.
pub enum HandoverSource<'a> {
    DeviceTokenAnswer(&'a Address),
    Other,
}

/// A server base checked for use on the music line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicBase(Address);

impl MusicBase {
    pub fn typed(text: &str) -> Result<Self, AddressError> {
        Self::checked(Address::typed(text)?, true)
    }

    /// Section 7.2's pure checks. Transport lifetime, pairing the returned base with
    /// the credential in this very answer, and rotating the generation belong to the app.
    pub fn handed_over(nexus: &Origin, source: HandoverSource<'_>, text: &str) -> Result<Self, AddressError> {
        use crate::nexus::proto::plain;
        match source {
            HandoverSource::DeviceTokenAnswer(sent)
                if sent.origin() == nexus && sent.query().is_none() && plain(sent.path()).is_ok() =>
            {
                ()
            }
            _ => return Err(AddressError::HandoverSource),
        }
        let addr = Address::parse(text)?;
        let same_origin = addr.origin() == nexus;
        Self::checked(addr, same_origin)
    }

    fn checked(addr: Address, shared: bool) -> Result<Self, AddressError> {
        // the design does not say how a server base's query would be combined with
        // operation arguments. Refuse even an empty query on a configured music base.
        if addr.query().is_some() {
            return Err(AddressError::BaseQuery);
        }
        if shared && addr.under_nexus() {
            return Err(AddressError::NexusPath);
        }
        Ok(Self(addr))
    }

    pub fn address(&self) -> &Address {
        &self.0
    }

    /// The transport supplies one operation and its already encoded arguments.
    pub fn request(&self, operation: &str, query: Option<&str>) -> Result<MusicRequest, AddressError> {
        check_length(operation)?;
        if operation.is_empty() || matches!(operation, "." | "..") || !operation.bytes().all(unreserved) {
            return Err(AddressError::Path);
        }
        if let Some(q) = query {
            check_length(q)?;
            if !encoded(q) {
                return Err(AddressError::Query);
            }
        }
        let path = format!("{}/rest/{operation}", self.0.path.trim_end_matches('/'));
        Ok(MusicRequest(Address::from_parts(self.0.origin.clone(), &path, query)?))
    }
}

/// The source for a music redirect can only be built from a checked base. The operation
/// is read from this request's own path, so it cannot be mislabeled as a different read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MusicRequest(Address);

impl MusicRequest {
    pub fn address(&self) -> &Address {
        &self.0
    }
    pub fn operation(&self) -> &str {
        self.0.path.trim_end_matches('/').rsplit('/').next().unwrap_or_default()
    }

    /// Section 5.3 checks the path before the query. Only this constructor can turn a
    /// redirect target back into a music request.
    pub(crate) fn with_added_slash(&self, target: Address) -> Option<Self> {
        (self.0.origin == target.origin
            && target.path.strip_suffix('/') == Some(self.0.path())
            && self.0.query == target.query)
            .then_some(Self(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_has_one_wire_spelling() {
        for (input, wire) in [
            ("Music.Example.ORG", "https://music.example.org/"),
            (
                "HTTPS://Mu_Sic.example.org:00443/a.1/~b-c_d/?order=2&order=1#top",
                "https://mu_sic.example.org/a.1/~b-c_d/?order=2&order=1",
            ),
            ("xn--bcher-kva.example", "https://xn--bcher-kva.example/"),
            ("192.0.2.10:1", "https://192.0.2.10:1/"),
            ("192.000.2.10:65535", "https://192.000.2.10:65535/"),
            ("192.2.10", "https://192.2.10/"),
            ("[2001:0DB8:0:0:0:0:0:1]:04444", "https://[2001:db8::1]:4444/"),
            ("[::1]", "https://[::1]/"),
            ("[::]", "https://[::]/"),
            ("[64:ff9b::c000:20a]", "https://[64:ff9b::c000:20a]/"),
            ("m?#f", "https://m/?"),
            ("m#f", "https://m/"),
            ("m?q=%2f%2F%00+/?@&x=1", "https://m/?q=%2f%2F%00+/?@&x=1"),
        ] {
            let a = Address::typed(input).unwrap();
            assert_eq!(a.wire(), wire);
            assert_eq!(Address::parse(wire).unwrap(), a);
        }
        assert_eq!(Address::parse("m"), Err(AddressError::Scheme));
    }

    #[test]
    fn refuses_before_any_repair() {
        use AddressError::*;
        for (text, why) in [
            ("http://m", Scheme),
            ("ftp://m", Scheme),
            ("//m", Host),
            ("", Host),
            ("https://user:p@m", Userinfo),
            ("https://@m", Userinfo),
            ("m.", TrailingDot),
            ("m..org", Host),
            ("-m.org", Host),
            ("m-.org", Host),
            ("bücher.example", Host),
            ("m%2eorg", Host),
            ("m:", EmptyPort),
            ("m:0", Port),
            ("m:65536", Port),
            ("m:000001", Port),
            ("m:+1", Port),
            ("[fe80::1%25eth0]", Zone),
            ("[fe80::1%eth0]", Zone),
            ("[::ffff:192.0.2.10]", EmbeddedIpv4),
            ("[::ffff:c000:20a]", EmbeddedIpv4),
            ("[::c000:20a]", EmbeddedIpv4),
            ("[2001:db8::192.0.2.10]", EmbeddedIpv4),
            ("[abcd:::1234]", Host),
            ("[2001:db8::1", Host),
            ("[::1]x", Host),
            ("m/x/../nexus", Path),
            ("m/nexus/../rest", Path),
            ("m/%2f", Path),
            ("m/./a", Path),
            ("m/a//", Path),
            ("m/é", Path),
            ("m/a;b", Path),
            ("m/a\\@h", Character),
            (" m", Character),
            ("m#a\0b", Character),
            ("m?x\r\n", Character),
            ("m#\u{7f}", Character),
            ("m?x=%", Query),
            ("m?x=%2", Query),
            ("m?x=%xx", Query),
            ("m?é", Query),
            ("m#a#b", Fragment),
            ("m#%", Fragment),
        ] {
            assert_eq!(Address::typed(text), Err(why), "{text:?}");
        }
    }

    #[test]
    fn length_limits_apply_to_inputs_and_results() {
        let label = "a".repeat(63);
        let host = format!("{label}.{label}.{label}.{}", "a".repeat(61));
        assert_eq!(host.len(), 253);
        assert!(Address::typed(&host).is_ok());
        assert_eq!(Address::typed(&(host + "a")), Err(AddressError::Host));
        assert_eq!(Address::typed(&(label + "a")), Err(AddressError::Host));
        let a = format!("https://m/{}", "x".repeat(MAX_ADDRESS_BYTES - 10));
        let full = Address::parse(&a).unwrap();
        assert_eq!(Address::parse(&(a.clone() + "x")), Err(AddressError::TooLong));
        assert_eq!(full.location("?"), Err(AddressError::TooLong));
        assert_eq!(
            Address::typed(&format!("m/{}", "x".repeat(MAX_ADDRESS_BYTES - 2))),
            Err(AddressError::TooLong)
        );
        let fragment = format!("#{}", "a".repeat(MAX_ADDRESS_BYTES - 1));
        assert_eq!(full.location(&fragment).unwrap(), full);
        assert_eq!(full.location(&(fragment + "a")), Err(AddressError::TooLong));
    }

    #[test]
    fn rfc3986_reference_examples_with_the_signed_path_grammar() {
        // The RFC base has HTTP and a semicolon in the path. Use its representable
        // HTTPS equivalent, preserving its directories, final segment and query.
        let base = Address::parse("https://a/b/c/d?q").unwrap();
        assert_eq!(base.resolve("http://a/g"), Err(AddressError::Scheme));
        for (reference, expected) in [
            ("g", "a/b/c/g"),
            ("g/", "a/b/c/g/"),
            ("/g", "a/g"),
            ("//g", "g/"),
            ("?y", "a/b/c/d?y"),
            ("g?y", "a/b/c/g?y"),
            ("#s", "a/b/c/d?q"),
            ("g#s", "a/b/c/g"),
            ("g?y#s", "a/b/c/g?y"),
            ("", "a/b/c/d?q"),
            ("g.", "a/b/c/g."),
            (".g", "a/b/c/.g"),
            ("g..", "a/b/c/g.."),
            ("..g", "a/b/c/..g"),
            ("g?y/./x", "a/b/c/g?y/./x"),
            ("g?y/../x", "a/b/c/g?y/../x"),
            ("g#s/./x", "a/b/c/g"),
            ("g#s/../x", "a/b/c/g"),
            ("?", "a/b/c/d?"),
            ("//g?y", "g/?y"),
        ] {
            assert_eq!(
                base.resolve(reference).unwrap().wire(),
                format!("https://{expected}"),
                "{reference}"
            );
        }
        for reference in [
            "g:h",
            "./g",
            ";x",
            "g;x",
            "g;x?y#s",
            ".",
            "./",
            "..",
            "../",
            "../g",
            "../..",
            "../../",
            "../../g",
            "../../../g",
            "../../../../g",
            "/./g",
            "/../g",
            "./../g",
            "./g/.",
            "g/./h",
            "g/../h",
            "g;x=1/./y",
            "g;x=1/../y",
            "http:g",
            "https:/g",
            "https:g://other",
            "///g",
            "g//h",
            "g/%2e%2e/h",
        ] {
            assert!(base.location(reference).is_err(), "{reference}");
        }
    }
}

#[cfg(test)]
mod music_tests {
    use super::*;
    #[test]
    fn only_a_device_token_answer_from_the_nexus_line_may_hand_over_a_base() {
        let sent = Address::parse("https://NEXUS.example.org:443/nexus/api/account").unwrap();
        let origin = sent.origin();
        let handover = |text| MusicBase::handed_over(origin, HandoverSource::DeviceTokenAnswer(&sent), text);
        for text in [
            "http://music.example.org",
            "music.example.org",
            "https://name:pass@music.example.org",
            "https://nexus.example.org/NEXUS",
            "https://nexus.example.org/nexus/child",
            "https://music.example.org/x/%2e%2e/nexus",
            "https://music.example.org/?",
            "https://music.example.org/?token=private",
        ] {
            assert!(handover(text).is_err(), "{text}");
        }
        for text in [
            "https://music.example.org/nexus",
            "https://nexus.example.org:444/nexus",
            "https://nexus.example.org/nexusmusic",
        ] {
            assert!(handover(text).is_ok(), "{text}");
        }
        assert_eq!(
            MusicBase::handed_over(origin, HandoverSource::Other, "https://m"),
            Err(AddressError::HandoverSource)
        );
        for url in [
            "https://nexus.example.org/rest/getSong",
            "https://other.example.org/nexus/api/account",
            "https://nexus.example.org/nexus/api/account?x=1",
        ] {
            let other = Address::parse(url).unwrap();
            assert_eq!(
                MusicBase::handed_over(origin, HandoverSource::DeviceTokenAnswer(&other), "https://m"),
                Err(AddressError::HandoverSource)
            );
        }
    }

    #[test]
    fn compare_the_components_the_stack_will_send() {
        let a = Address::parse("https://[2001:db8::1]/rest/ping?").unwrap();
        assert!(a.matches_request("https", "[2001:db8::1]", 443, "/rest/ping", Some("")));
        for (s, h, p, path, q) in [
            ("http", "[2001:db8::1]", 443, "/rest/ping", Some("")),
            ("https", "[2001:db8::2]", 443, "/rest/ping", Some("")),
            ("https", "[2001:db8::1]", 8443, "/rest/ping", Some("")),
            ("https", "[2001:db8::1]", 443, "/rest/ping/", Some("")),
            ("https", "[2001:db8::1]", 443, "/rest/ping", None),
        ] {
            assert!(!a.matches_request(s, h, p, path, q));
        }
    }

    #[test]
    fn music_transport_builds_its_own_path() {
        for text in [
            "m/nexus",
            "m/Nexus/sub",
            "m/sub/../nexus",
            "m/nexus/../rest",
            "m/%6eexus",
            "m//nexus",
            "http://m",
            "m?x=private",
        ] {
            assert!(MusicBase::typed(text).is_err());
        }
        for (base, path) in [
            ("m", "/rest/ping"),
            ("m/sub/path", "/sub/path/rest/ping"),
            ("m/sub/path/", "/sub/path/rest/ping"),
        ] {
            let base = MusicBase::typed(base).unwrap();
            let req = base.request("ping", Some("x=%2F")).unwrap();
            assert_eq!(req.operation(), "ping");
            assert_eq!(req.address().path(), path);
            assert_eq!(req.address().query(), Some("x=%2F"));
            for operation in ["", ".", "..", "ping/", "/nexus", "../ping", "%2F", "ping?"] {
                assert!(base.request(operation, None).is_err());
            }
            assert!(base.request("ping", Some("x=#hidden")).is_err());
            assert!(base.request(&"a".repeat(MAX_ADDRESS_BYTES + 1), None).is_err());
            assert!(base.request("ping", Some(&"a".repeat(MAX_ADDRESS_BYTES + 1))).is_err());
        }
    }
}
