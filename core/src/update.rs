//! The origins the desktop updater may contact. The URL we check is the one the app sends.
use url::{Position, Url};

pub const REPO: &str = "Comfydata276/nexus-music";
pub const DETAILS: Source<'static> = Source::Metadata { origin: "https://api.github.com" };
// Observed on the project's 0.40.13 installer, from GitHub's download link to the asset.
pub const INSTALLER: Source<'static> = Source::Installer {
    repository: "https://github.com",
    assets: "https://release-assets.githubusercontent.com",
};

#[derive(Clone, Copy)]
pub enum Source<'a> {
    Metadata { origin: &'a str },
    Installer { repository: &'a str, assets: &'a str },
}

impl Source<'_> {
    /// Metadata starts on the API. An installer must start at this project's download link,
    /// even though a later hop may go to the asset store.
    pub fn start(self, address: &str) -> Option<Url> {
        let url = read_url(None, address)?;
        if let Self::Installer { repository, .. } = self {
            let path = format!("/{REPO}/releases/download/");
            if !address.starts_with(&format!("{repository}{path}")) || !url.path().starts_with(&path)
            {
                return None;
            }
        }
        self.checked(url)
    }

    /// Resolve against the actual address that answered, including for a scheme-relative hop.
    pub fn redirect(self, current: &Url, location: &str, followed: usize) -> Option<Url> {
        if followed >= 5 || location.is_empty() {
            return None;
        }
        self.checked(read_url(Some(current), location)?)
    }

    fn checked(self, url: Url) -> Option<Url> {
        // The serialized prefix includes any user information and any non-default port.
        let origin = &url[..Position::BeforePath];
        let allowed = match self {
            Self::Metadata { origin: api } => origin == api,
            Self::Installer { repository, assets } => origin == repository || origin == assets,
        };
        (allowed && url.scheme() == "https" && url.fragment().is_none()).then_some(url)
    }
}

/// Reject spellings a browser-style parser could silently repair before the origin check.
fn read_url(base: Option<&Url>, address: &str) -> Option<Url> {
    let authority_and_path = address.strip_prefix("//").or_else(|| {
        let (scheme, rest) = address.split_once("://")?;
        scheme.eq_ignore_ascii_case("https").then_some(rest)
    });
    if let Some(rest) = authority_and_path {
        let authority = rest.split(['/', '?', '#']).next()?;
        if authority.ends_with(':') || !authority.is_ascii() || authority.contains(['%', '@']) {
            return None;
        }
    }
    let malformed = std::cell::Cell::new(false);
    let url = Url::options()
        .base_url(base)
        .syntax_violation_callback(Some(&|_| malformed.set(true)))
        .parse(address).ok()?;
    (!malformed.get()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_stays_on_the_api_origin() {
        let api = DETAILS.start("https://api.github.com/repos/example/player/releases/latest").unwrap();
        let next = DETAILS.redirect(&api, "published?after=1", 4).unwrap();
        assert_eq!(next.as_str(), "https://api.github.com/repos/example/player/releases/published?after=1");
        for address in [
            "http://api.github.com/", "https://api.github.com.example/",
            "https://api.github.com:8443/", "//example.org/", "https://user@api.github.com/",
            "https://api.github.com@other.example/", "https://github.com/", "#fragment",
            "https://@api.github.com/", "https://%61pi.github.com/", "https://api.github.com:/",
            "https:////api.github.com/", "https://api.github.com/a b",
        ] {
            assert!(DETAILS.redirect(&api, address, 0).is_none(), "{address}");
        }
        assert!(DETAILS.redirect(&api, "/sixth", 5).is_none());
    }

    #[test]
    fn installer_starts_in_this_repository_then_follows_only_the_listed_origins() {
        let url = format!("https://github.com/{REPO}/releases/download/v2/setup.exe");
        let first = INSTALLER.start(&url).unwrap();
        let asset = "https://release-assets.githubusercontent.com/file?signature=example";
        assert_eq!(INSTALLER.redirect(&first, asset, 0).unwrap().as_str(), asset);
        for address in [
            "https://github.com/another/project/releases/download/v2/setup.exe",
            &url.replace("/v2/setup.exe", "/../../elsewhere"),
            &url.replace("/download/", "/downloads/"), asset,
        ] {
            assert!(INSTALLER.start(address).is_none(), "{address}");
        }
        for address in ["http://github.com/file", "https://api.github.com/file",
            "//different.githubusercontent.com/file", "https://github.com:444/file"] {
            assert!(INSTALLER.redirect(&first, address, 0).is_none(), "{address}");
        }
    }
}
