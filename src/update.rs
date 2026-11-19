//! Looks for a newer release on GitHub, and downloads and runs its installer.

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "Comfydata276/nexus-music";

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

fn user_agent() -> String {
    format!("NexusMusic/{}", env!("CARGO_PKG_VERSION"))
}

fn latest_url() -> String {
    // Test builds can point at a stand-in server.
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("NEXUS_UPDATE_URL") {
        return url;
    }
    format!("https://api.github.com/repos/{REPO}/releases/latest")
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

/// The newest published release, if it is newer than this copy.
pub fn check() -> Result<Option<Release>, String> {
    let agent = crate::subsonic::make_agent();
    let response = agent
        .get(&latest_url())
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if status == 404 {
        // Nothing has been published yet.
        return Ok(None);
    }
    if status != 200 {
        return Err(format!("GitHub answered with status {status}."));
    }
    let api: ApiRelease =
        serde_json::from_reader(response.into_body().into_reader()).map_err(|e| e.to_string())?;
    let release = release_from(api);
    Ok(is_newer(&release.version, env!("CARGO_PKG_VERSION")).then_some(release))
}

fn trusted(url: &str) -> bool {
    if cfg!(debug_assertions) && std::env::var_os("NEXUS_UPDATE_URL").is_some() {
        return true;
    }
    url.starts_with(&format!("https://github.com/{REPO}/releases/download/"))
}

const NO_CHECKSUM: &str =
    "GitHub didn't publish a checksum for this update, so it can't be checked and wasn't installed. Download it from the release page instead.";

/// Downloads the installer to the temporary folder, checking it against the size and
/// checksum GitHub published. `progress` is called with the fraction done.
pub fn download(installer: &Installer, mut progress: impl FnMut(f32)) -> Result<PathBuf, String> {
    if !trusted(&installer.url) {
        return Err("The download address isn't on the project's release page.".into());
    }
    if installer.sha256.is_none() {
        return Err(NO_CHECKSUM.into());
    }
    let name: String =
        installer.name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    let path = std::env::temp_dir().join(if name.is_empty() { "NexusMusic-Setup.exe".into() } else { name });
    let fetched = fetch(installer, &path, &mut progress);
    // If the app's own connection to GitHub fails, let Windows fetch the file instead. Either
    // way the result is only trusted once it matches the published checksum.
    #[cfg(windows)]
    let fetched = fetched.or_else(|reason| {
        if std::env::var_os("NEXUS_TRACE").is_some() {
            eprintln!("[trace] update download: trying Windows' downloader after: {reason}");
        }
        fetch_with_windows(&installer.url, &path).map_err(|second| format!("{reason} {second}"))
    });
    let verified = fetched.and_then(|()| verify(installer, &path));
    if verified.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    verified.map(|()| path)
}

/// Fetches the file over the app's own HTTP client.
fn fetch(installer: &Installer, path: &std::path::Path, progress: &mut impl FnMut(f32)) -> Result<(), String> {
    let trace = std::env::var_os("NEXUS_TRACE").is_some();
    // GitHub answers with a redirect to its file store. Each hop is requested separately, so a
    // slow or failed one can be told apart.
    let mut address = installer.url.clone();
    let mut hops = 0;
    let response = loop {
        let started = std::time::Instant::now();
        let agent = crate::subsonic::make_agent();
        let response = agent
            .get(&address)
            .header("User-Agent", user_agent())
            .config()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .timeout_recv_response(Some(Duration::from_secs(12)))
            .build()
            .call()
            .map_err(|e| {
                if trace {
                    eprintln!("[trace] update download: hop {hops} failed after {:?}: {e:?}", started.elapsed());
                }
                format!("The download didn't start ({e}).")
            })?;
        if trace {
            eprintln!("[trace] update download: hop {hops} answered {} in {:?}", response.status(), started.elapsed());
        }
        let next = response
            .status()
            .is_redirection()
            .then(|| response.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string))
            .flatten();
        match next {
            Some(next) if hops < 5 && next.starts_with("https://") => {
                address = next;
                hops += 1;
            }
            _ => break response,
        }
    };
    if !response.status().is_success() {
        return Err(format!("The download failed with status {}.", response.status().as_u16()));
    }
    let mut file = File::create(path).map_err(|e| e.to_string())?;
    let mut body = response.into_body().into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if done > installer.size.max(1) * 2 + (1 << 20) {
            return Err("The download is larger than the release says it should be.".into());
        }
        progress((done as f32 / installer.size.max(1) as f32).min(1.0));
    }
}

/// Has Windows download the file with its own networking, the way a browser would.
#[cfg(windows)]
fn fetch_with_windows(url: &str, path: &std::path::Path) -> Result<(), String> {
    use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
    use windows::core::HSTRING;
    unsafe { URLDownloadToFileW(None, &HSTRING::from(url), &HSTRING::from(path.as_os_str()), 0, None) }
        .map_err(|e| format!("Windows' own downloader failed too (error {:#010X}).", e.code().0))
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
mod network_tests {
    use super::*;

    /// Downloads a real release from GitHub. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the network"]
    fn downloads_a_published_installer() {
        let installer = Installer {
            url: format!("https://github.com/{REPO}/releases/download/v0.4.0/NexusMusic-Setup-0.4.0.exe"),
            name: "NexusMusic-Setup-0.4.0.exe".into(),
            size: 10_071_802,
            sha256: Some("6369d91186f1bb57b8d77b6b05d6b9da2403511f2e285851cc5bc9671425fbc0".into()),
        };
        let started = std::time::Instant::now();
        let path = download(&installer, |_| {}).expect("download failed");
        eprintln!("downloaded in {:?} to {}", started.elapsed(), path.display());
        let _ = std::fs::remove_file(path);
    }
}
