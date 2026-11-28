//! Looks for a newer release on GitHub, and downloads and runs its installer.

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nexus_core::update::{DETAILS, INSTALLER, REPO, Source};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release's page on GitHub, for copies that can't update themselves.
    pub page: String,
    pub installer: Option<Installer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Installer {
    pub url: String,
    pub name: String,
    pub size: u64,
    /// Hex SHA-256 of the file, when GitHub supplies one.
    pub sha256: Option<String>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// The updater always sees redirects itself. Its agent carries no account credentials.
fn agent_config(roots: RootCerts) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(roots).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(8)))
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
}

fn agent() -> ureq::Agent {
    agent_config(RootCerts::PlatformVerifier).build().into()
}

/// Splits "v1.2.3" into its numbers.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').splitn(3, '.');
    let mut next = || parts.next()?.trim().parse::<u32>().ok();
    Some((next()?, next()?, next()?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

fn release_from(api: ApiRelease) -> Release {
    let installer = api
        .assets
        .into_iter()
        .find(|asset| {
            let name = asset.name.to_lowercase();
            name.contains("setup") && name.ends_with(".exe")
        })
        .map(|asset| Installer {
            sha256: asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
            url: asset.browser_download_url,
            name: asset.name,
            size: asset.size,
        });
    Release { version: api.tag_name.trim_start_matches('v').to_string(), page: api.html_url, installer }
}

#[derive(Debug, PartialEq)]
pub enum CheckError {
    Refused,
    TimedOut,
    Request,
    Status(u16),
    InvalidRelease,
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused => f.write_str("GitHub supplied an update address the app couldn't use."),
            Self::TimedOut => f.write_str("The request to GitHub timed out."),
            Self::Request => f.write_str("The request to GitHub failed."),
            Self::Status(status) => write!(f, "GitHub answered with status {status}."),
            Self::InvalidRelease => f.write_str("The release details couldn't be read."),
        }
    }
}

/// A manual GET chain. Only the checked, resolved URL reaches the wire. For metadata the
/// remaining wall-clock budget also applies to the final body. Large installers can stream
/// past their existing per-hop response-header timeout.
fn get(
    agent: &ureq::Agent,
    source: Source<'_>,
    start: &str,
    deadline: Option<Instant>,
) -> Result<ureq::http::Response<ureq::Body>, CheckError> {
    let mut address = source.start(start).ok_or(CheckError::Refused)?;
    let mut followed = 0;
    loop {
        let mut request = agent.get(address.as_str());
        if matches!(source, Source::Metadata { .. }) {
            request = request.header("Accept", "application/vnd.github+json");
        }
        let mut config = request.config();
        if let Some(deadline) = deadline {
            let remaining = deadline.checked_duration_since(Instant::now())
                .filter(|time| !time.is_zero()).ok_or(CheckError::TimedOut)?;
            config = config.timeout_global(Some(remaining));
        }
        let response = config.build().call().map_err(|err| match err {
            ureq::Error::Timeout(_) => CheckError::TimedOut,
            _ => CheckError::Request,
        })?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Err(CheckError::Refused);
        }
        let mut locations = response.headers().get_all("location").iter();
        let location = locations.next().and_then(|value| value.to_str().ok()).ok_or(CheckError::Refused)?;
        if locations.next().is_some() {
            return Err(CheckError::Refused);
        }
        address = source.redirect(&address, location, followed).ok_or(CheckError::Refused)?;
        followed += 1;
    }
}

/// The newest published release, if it is newer than this copy.
pub fn check() -> Result<Option<Release>, CheckError> {
    check_at(&agent(), DETAILS, &format!("https://api.github.com/repos/{REPO}/releases/latest"), Duration::from_secs(20))
}

fn check_at(agent: &ureq::Agent, source: Source<'_>, start: &str, budget: Duration) -> Result<Option<Release>, CheckError> {
    let deadline = Instant::now() + budget;
    let response = get(agent, source, start, Some(deadline))?;
    match response.status().as_u16() {
        404 => return Ok(None), // Nothing has been published yet.
        200 => {}
        status => return Err(CheckError::Status(status)),
    }
    let api = serde_json::from_reader(response.into_body().into_reader());
    if Instant::now() >= deadline {
        return Err(CheckError::TimedOut);
    }
    let release = release_from(api.map_err(|_| CheckError::InvalidRelease)?);
    Ok(is_newer(&release.version, env!("CARGO_PKG_VERSION")).then_some(release))
}

const NO_CHECKSUM: &str =
    "GitHub didn't publish a checksum for this update, so it can't be checked and wasn't installed. Download it from the release page instead.";

/// Downloads the installer to the temporary folder, checking it against the size and
/// checksum GitHub published. `progress` is called with the fraction done.
pub fn download(installer: &Installer, progress: impl FnMut(f32)) -> Result<PathBuf, String> {
    let agent = agent_config(RootCerts::PlatformVerifier)
        .timeout_recv_response(Some(Duration::from_secs(12))).build().into();
    download_with(installer, progress, || get(&agent, INSTALLER, &installer.url, None))
}

/// A single fetch supplies the body. Only a verified file can be handed to the install action.
fn download_with(
    installer: &Installer,
    progress: impl FnMut(f32),
    fetch: impl FnOnce() -> Result<ureq::http::Response<ureq::Body>, CheckError>,
) -> Result<PathBuf, String> {
    if installer.sha256.is_none() {
        return Err(NO_CHECKSUM.into());
    }
    let response = fetch().map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("The download failed with status {}.", response.status().as_u16()));
    }
    let name: String =
        installer.name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    let name = if name.is_empty() { "NexusMusic-Setup.exe" } else { &name };
    // Another attempt cannot truncate this one's verified file or delete it on failure.
    let path = std::env::temp_dir().join(format!("{:016x}-{name}", fastrand::u64(..)));
    let file = File::create_new(&path).map_err(|e| e.to_string())?;
    let verified = write_installer(file, response.into_body(), installer.size, progress)
        .and_then(|()| verify(installer, &path));
    if verified.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    verified.map(|()| path)
}

fn write_installer(mut file: File, body: ureq::Body, size: u64, mut progress: impl FnMut(f32)) -> Result<(), String> {
    let mut body = body.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = body.read(&mut buf).map_err(|_| "The download was interrupted.")?;
        if n == 0 {
            return Ok(());
        }
        if n as u64 > size - done {
            return Err("The download is larger than the release says it should be.".into());
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done as f32 / size as f32);
    }
}

/// Checks a downloaded file against the size and checksum the release published.
fn verify(installer: &Installer, path: &std::path::Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    // No published checksum means nothing to check against, and that isn't a pass.
    let intact = size == installer.size && installer.sha256.as_ref().is_some_and(|expected| *expected == digest);
    if intact { Ok(()) } else { Err("The download didn't match the published file, so it wasn't installed.".into()) }
}

/// Whether this copy was put in place by the installer (and so can be replaced by one).
pub fn installed_copy() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let named = exe.file_name().is_some_and(|name| name.eq_ignore_ascii_case("NexusMusic.exe"));
    named && exe.parent().is_some_and(|dir| dir.join("Uninstall.exe").exists())
}

/// Starts the installer quietly; it replaces the program and opens the new version.
pub fn run_installer(path: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new(path).args(["/S", "/RELAUNCH"]).spawn().map(drop)
}

pub fn open_in_browser(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(program).arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("v0.4.0", "0.3.0"));
        assert!(is_newer("0.10.0", "0.9.5"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("nightly", "0.3.0"));
    }

    #[test]
    fn picks_the_installer_from_a_release() {
        let api: ApiRelease = serde_json::from_str(
            r#"{"tag_name":"v0.4.0","html_url":"https://github.com/o/r/releases/tag/v0.4.0","assets":[
                {"name":"NexusMusic-0.4.0-portable.exe","browser_download_url":"https://x/p.exe","size":5},
                {"name":"NexusMusic-Setup-0.4.0.exe","browser_download_url":"https://x/s.exe","size":9,
                 "digest":"sha256:ABCDEF"}]}"#,
        )
        .unwrap();
        let release = release_from(api);
        assert_eq!(release.version, "0.4.0");
        let installer = release.installer.unwrap();
        assert_eq!(installer.name, "NexusMusic-Setup-0.4.0.exe");
        assert_eq!(installer.size, 9);
        assert_eq!(installer.sha256.as_deref(), Some("abcdef"));
    }
}

#[cfg(test)]
mod checksum_tests {
    use super::*;

    fn installer(bytes: &[u8], sha256: Option<String>) -> Installer {
        Installer {
            url: format!("https://github.com/{REPO}/releases/download/v9.9.9/NexusMusic-Setup-9.9.9.exe"),
            name: "NexusMusic-Setup-9.9.9.exe".into(),
            size: bytes.len() as u64,
            sha256,
        }
    }

    #[test]
    fn installs_only_what_matches_a_published_checksum() {
        let bytes = b"not really an installer";
        let path = std::env::temp_dir().join(format!("nexus-update-test-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let digest: String = Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(verify(&installer(bytes, Some(digest.clone())), &path).is_ok());
        // The right size isn't enough.
        assert!(verify(&installer(bytes, Some("0".repeat(64))), &path).is_err());
        assert!(verify(&installer(bytes, None), &path).is_err());
        // The right checksum for a different length isn't either.
        assert!(verify(&Installer { size: 5, ..installer(bytes, Some(digest)) }, &path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn will_not_fetch_an_update_with_no_checksum() {
        assert_eq!(download(&installer(b"", None), |_| {}).unwrap_err(), NO_CHECKSUM);
    }
}

#[cfg(test)]
mod network_tests;
