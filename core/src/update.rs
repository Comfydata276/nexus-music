//! The origins the desktop updater may contact. The URL we check is the one the app sends.
use crate::origin::Address;

pub const REPO: &str = "Comfydata276/nexus-music";
pub const DETAILS: Source<'static> = Source::Metadata { origin: "https://api.github.com" };
// GitHub's release-asset origin observed for 0.40.13 and its previous asset origin.
pub const INSTALLER: Source<'static> = Source::Installer {
    repository: "https://github.com",
    assets: &["https://release-assets.githubusercontent.com", "https://objects.githubusercontent.com"],
};

#[derive(Clone, Copy)]
pub enum Source<'a> {
    Metadata { origin: &'a str },
    Installer { repository: &'a str, assets: &'a [&'a str] },
}

impl Source<'_> {
    /// Metadata starts on the API. An installer must start at this project's download link,
    /// even though a later hop may go to the asset store.
    pub fn start(self, address: &str) -> Option<Address> {
        // Address discards fragments. The updater refuses them even on its initial URL.
        if address.contains('#') {
            return None;
        }
        let url = Address::parse(address).ok()?;
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
    pub fn redirect(self, current: &Address, location: &str, followed: usize) -> Option<Address> {
        if followed >= 5 || location.is_empty() || location.contains('#') {
            return None;
        }
        self.checked(current.resolve(location).ok()?)
    }

    fn checked(self, address: Address) -> Option<Address> {
        let origin = address.origin().to_string();
        let allowed = match self {
            Self::Metadata { origin: api } => origin == api,
            Self::Installer { repository, assets } => origin == repository || assets.contains(&origin.as_str()),
        };
        allowed.then_some(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_stays_on_the_api_origin() {
        let api = DETAILS.start("https://api.github.com/repos/example/player/releases/latest").unwrap();
        let next = DETAILS.redirect(&api, "published?after=1", 4).unwrap();
        assert_eq!(next.wire(), "https://api.github.com/repos/example/player/releases/published?after=1");
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
        assert_eq!(INSTALLER.redirect(&first, asset, 0).unwrap().wire(), asset);
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

    #[test]
    fn both_asset_origins_accept_a_signed_query_byte_for_byte() {
        let first = INSTALLER.start(&format!("https://github.com/{REPO}/releases/download/v2/setup.exe")).unwrap();
        let query = format!("sig=a%2Fb%2Bc%3D&jwt={}&response-content-disposition=attachment%3B%20filename%3Dsetup.exe",
            "a".repeat(4096));
        for host in ["release-assets.githubusercontent.com", "objects.githubusercontent.com"] {
            let asset = format!("https://{host}/github-production-release-asset/1234/ab-cd?{query}");
            assert_eq!(INSTALLER.redirect(&first, &asset, 0).unwrap().wire(), asset);
            assert!(INSTALLER.start(&asset).is_none());
        }
        for host in ["objects.githubusercontent.com.evil", "githubusercontent.com", "objects.githubusercontent.com."] {
            assert!(INSTALLER.redirect(&first, &format!("https://{host}/file"), 0).is_none());
        }
    }

}
