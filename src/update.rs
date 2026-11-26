//! Updates from the GitHub releases: whether a newer version is published, and installing
//! it in place of the running program.
//!
//! The check reads the latest release from the GitHub API. Installing downloads this
//! platform's archive, checks it against its published SHA-256, takes the executable out
//! of it and puts it where the running one is: written next to it, then renamed over it.
//! On Unix the rename replaces the file while the old program keeps running from its
//! (now unlinked) copy. Windows can't replace or delete a running executable, but it can
//! rename it: the running one moves aside to `markdone.exe.<n>.old` (deleted by the next
//! launch, see [`cleanup`]) and the new one takes its name. On macOS the bundle's
//! `Info.plist` is replaced too, so Finder shows the new version.
//!
//! Where the program's folder isn't writable (a system-wide install), nothing can be
//! installed: [`install_problem`] says why, and the user gets the download page instead.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

/// The GitHub repository the releases are published in.
const REPO: &str = "bitblaster/markdone";

/// The page listing the latest release (the "download page").
pub const RELEASES_PAGE: &str = "https://github.com/bitblaster/markdone/releases/latest";

/// The running version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// This platform's release archive: its target triple and extension (see the release
/// workflow). `None` where no build is published.
const PLATFORM: Option<(&str, &str)> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some(("x86_64-unknown-linux-gnu", "tar.gz"))
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some(("x86_64-pc-windows-msvc", "zip"))
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some(("aarch64-apple-darwin", "zip"))
} else {
    None
};

/// A published release newer than the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The version, without the tag's `v`.
    pub version: String,
    /// The release's page (notes and downloads).
    pub page: String,
    /// This platform's archive and its checksum file, if the release has them.
    archive: Option<Asset>,
    checksum: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asset {
    name: String,
    url: String,
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
}

fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("markdone/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())
}

/// The latest release, if it is newer than the running version.
pub async fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let release: ApiRelease = client(Duration::from_secs(30))?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(newer_release(release, CURRENT))
}

/// `release` as a [`Release`] when its version is above `current`.
fn newer_release(release: ApiRelease, current: &str) -> Option<Release> {
    let version = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, current) {
        return None;
    }
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| Asset {
                name: a.name.clone(),
                url: a.browser_download_url.clone(),
            })
    };
    let archive_name = PLATFORM.map(|(target, ext)| archive_name(&release.tag_name, target, ext));
    let archive = archive_name.as_deref().and_then(find);
    let checksum = archive_name.and_then(|name| find(&format!("{name}.sha256")));
    Some(Release {
        version,
        page: release.html_url,
        archive,
        checksum,
    })
}

/// The name the release workflow gives an archive.
fn archive_name(tag: &str, target: &str, ext: &str) -> String {
    format!("markdone-{tag}-{target}.{ext}")
}

/// Whether version `a` is above `b` (dotted numbers; a pre-release suffix such as
/// `-rc1` counts as below the plain version).
pub fn is_newer(a: &str, b: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, bool) {
        let (numbers, pre) = match v.split_once('-') {
            Some((n, _)) => (n, true),
            None => (v, false),
        };
        let mut parts: Vec<u64> = numbers.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        while parts.last() == Some(&0) {
            parts.pop();
        }
        (parts, pre)
    }
    let (a_parts, a_pre) = parse(a);
    let (b_parts, b_pre) = parse(b);
    match a_parts.cmp(&b_parts) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => b_pre && !a_pre,
    }
}

/// The running executable, as it was at startup (on Linux, once replaced,
/// `current_exe` would name the deleted file).
pub fn exe_path() -> Option<&'static Path> {
    static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();
    EXE.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        Some(std::fs::canonicalize(&exe).unwrap_or(exe))
    })
    .as_deref()
}

/// The `.app` bundle the executable is in (macOS: `X.app/Contents/MacOS/exe`).
#[cfg(target_os = "macos")]
fn bundle_of(exe: &Path) -> Option<&Path> {
    let contents = exe.parent()?.parent()?;
    let bundle = contents.parent()?;
    (contents.file_name()? == "Contents" && bundle.extension()? == "app").then_some(bundle)
}

/// Why `release` can't be installed here (`None`: it can).
pub fn install_problem(release: &Release) -> Option<String> {
    if PLATFORM.is_none() {
        return Some("No build of Markdone for this platform is published.".into());
    }
    if release.archive.is_none() || release.checksum.is_none() {
        return Some("The release has no package for this platform.".into());
    }
    let Some(exe) = exe_path() else {
        return Some("The location of the program is unknown.".into());
    };
    let Some(dir) = exe.parent() else {
        return Some("The location of the program is unknown.".into());
    };
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut dirs = vec![dir.to_path_buf()];
    // macOS: the bundle's Info.plist is replaced too.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = bundle_of(exe) {
        dirs.push(bundle.join("Contents"));
    }
    dirs.into_iter().find(|d| !writable(d)).map(|dir| {
        format!(
            "Markdone can't update itself: it has no permission to write in {}.",
            dir.display()
        )
    })
}

/// Whether a file can be created in `dir`.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".markdone-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Download `release` and install it over the running executable.
pub async fn install(release: Release) -> Result<(), String> {
    let (Some(archive), Some(checksum)) = (&release.archive, &release.checksum) else {
        return Err("the release has no package for this platform".into());
    };
    let exe = exe_path()
        .ok_or("the location of the program is unknown")?
        .to_path_buf();
    let client = client(Duration::from_secs(600))?;
    let get = async |url: &str| {
        client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())
    };
    let sums = get(&checksum.url).await?;
    let bytes = get(&archive.url).await?;
    let expected = parse_checksum(&String::from_utf8_lossy(&sums))
        .ok_or_else(|| format!("{} is not a SHA-256 checksum", checksum.name))?;
    if sha256_hex(&bytes) != expected {
        return Err(format!(
            "{} does not match its checksum (a damaged download?)",
            archive.name
        ));
    }
    let version = release.version.clone();
    tokio::task::spawn_blocking(move || {
        let files = extract(&bytes)?;
        install_files(&exe, &files, &version)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The hash of a `sha256sum` line (`<hex>  <file name>`).
fn parse_checksum(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?.to_ascii_lowercase();
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What an archive provides: the executable, and (macOS) the bundle's `Info.plist`.
struct Files {
    exe: Vec<u8>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    plist: Option<Vec<u8>>,
}

/// The executable from a Linux release archive: `markdone-<tag>-<target>/markdone`.
#[cfg(target_os = "linux")]
fn extract(bytes: &[u8]) -> Result<Files, String> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if entry.header().entry_type().is_file()
            && path.components().count() == 2
            && path.file_name().is_some_and(|n| n == "markdone")
        {
            let mut exe = Vec::new();
            entry.read_to_end(&mut exe).map_err(|e| e.to_string())?;
            return Ok(Files { exe, plist: None });
        }
    }
    Err("the package has no markdone executable".into())
}

/// The executable (and on macOS the bundle's `Info.plist`) from a zip release archive.
#[cfg(not(target_os = "linux"))]
fn extract(bytes: &[u8]) -> Result<Files, String> {
    let (exe_suffix, plist_suffix) = if cfg!(windows) {
        ("/markdone.exe", None)
    } else {
        (
            "/Markdone.app/Contents/MacOS/markdone",
            Some("/Markdone.app/Contents/Info.plist"),
        )
    };
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut exe = None;
    let mut plist = None;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        if !file.is_file() {
            continue;
        }
        let name = file.name().map_err(|e| e.to_string())?.into_owned();
        let slot = if name.ends_with(exe_suffix) {
            &mut exe
        } else if plist_suffix.is_some_and(|s| name.ends_with(s)) {
            &mut plist
        } else {
            continue;
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data).map_err(|e| e.to_string())?;
        *slot = Some(data);
    }
    let exe = exe.ok_or("the package has no markdone executable")?;
    Ok(Files { exe, plist })
}

/// Put the new files in place of the running program's.
fn install_files(exe: &Path, files: &Files, version: &str) -> Result<(), String> {
    replace_exe(exe, &files.exe).map_err(|e| format!("replacing {}: {e}", exe.display()))?;
    #[cfg(target_os = "macos")]
    if let (Some(plist), Some(bundle)) = (&files.plist, bundle_of(exe)) {
        let path = bundle.join("Contents").join("Info.plist");
        if let Err(e) = write_then_rename(&path, plist) {
            eprintln!("[markdone] update: Info.plist not replaced: {e}");
        }
    }
    #[cfg(windows)]
    record_installed_version(exe, version);
    #[cfg(not(windows))]
    let _ = version;
    Ok(())
}

/// The temporary name the new executable is written under, next to `exe`.
fn staging_path(exe: &Path) -> PathBuf {
    let name = exe
        .file_name()
        .map_or_else(|| "markdone".into(), |n| n.to_string_lossy().into_owned());
    exe.with_file_name(format!(".{name}.update"))
}

/// Write `bytes` beside `path`, then rename the copy over it (never a half-written file).
#[cfg(target_os = "macos")]
fn write_then_rename(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = staging_path(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(unix)]
fn replace_exe(exe: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = staging_path(exe);
    std::fs::write(&tmp, bytes)?;
    let mode = std::fs::metadata(exe).map_or(0o755, |m| m.permissions().mode() | 0o111);
    let result = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .and_then(|()| std::fs::rename(&tmp, exe));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Windows: the running executable can be renamed but not overwritten or deleted, so it
/// moves aside (to a name [`cleanup`] deletes later) and the new one takes its name.
#[cfg(windows)]
fn replace_exe(exe: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = staging_path(exe);
    std::fs::write(&tmp, bytes)?;
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let aside = (0..100)
        .map(|n| exe.with_file_name(format!("{name}.{n}.old")))
        .find(|p| !p.exists() || std::fs::remove_file(p).is_ok())
        .unwrap_or_else(|| exe.with_file_name(format!("{name}.{}.old", std::process::id())));
    if let Err(e) = std::fs::rename(exe, &aside) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, exe) {
        // Put the old one back, so the program is still there.
        let _ = std::fs::rename(&aside, exe);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Windows: when Markdone was installed with its installer (for the current user), update
/// the version "Installed apps" shows. Best effort.
#[cfg(windows)]
fn record_installed_version(exe: &Path, version: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{2967A7B3-51DF-4696-8644-1230F69DE0C6}_is1";
    if !exe.with_file_name("unins000.exe").exists() {
        return;
    }
    let reg = |args: &[&str]| {
        std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if reg(&["query", KEY, "/v", "DisplayVersion"]) {
        reg(&[
            "add",
            KEY,
            "/v",
            "DisplayVersion",
            "/t",
            "REG_SZ",
            "/d",
            version,
            "/f",
        ]);
    }
}

/// At startup: delete what an earlier update left behind (the executable it moved aside
/// on Windows, a staging file of an interrupted update). The old executable may still be
/// running for a moment (it is what started this one), so this retries for a while, on a
/// thread of its own.
pub fn cleanup() {
    let Some(exe) = exe_path() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let leftovers: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let file = p.file_name().unwrap_or_default().to_string_lossy();
            file == format!(".{name}.update")
                || (file.starts_with(&format!("{name}.")) && file.ends_with(".old"))
        })
        .collect();
    if leftovers.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let mut left = leftovers;
        for _ in 0..40 {
            left.retain(|p| std::fs::remove_file(p).is_err() && p.exists());
            if left.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Start the updated program (its path from startup), once this one has stopped
/// handling other launches (see [`crate::single_instance::retire`]).
pub fn restart() {
    let Some(exe) = exe_path() else {
        return;
    };
    crate::single_instance::retire();
    #[cfg(target_os = "macos")]
    if let Some(bundle) = bundle_of(exe) {
        if let Err(e) = std::process::Command::new("open")
            .arg("-n")
            .arg(bundle)
            .spawn()
        {
            eprintln!("[markdone] restart failed: {e}");
        }
        return;
    }
    if let Err(e) = std::process::Command::new(exe).spawn() {
        eprintln!("[markdone] restart failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.5.0", "0.5.0"));
        assert!(!is_newer("0.5", "0.5.0"));
        assert!(!is_newer("0.4.9", "0.5.0"));
        assert!(is_newer("0.6.0", "0.6.0-rc1"));
        assert!(!is_newer("0.6.0-rc1", "0.6.0"));
    }

    fn api_release(tag: &str, assets: &[&str]) -> ApiRelease {
        ApiRelease {
            tag_name: tag.into(),
            html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
            assets: assets
                .iter()
                .map(|n| ApiAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example.invalid/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn newer_release_picks_this_platforms_archive() {
        assert_eq!(newer_release(api_release("v0.5.0", &[]), "0.5.0"), None);
        let Some((target, ext)) = PLATFORM else {
            return;
        };
        let name = archive_name("v9.0.0", target, ext);
        let sums = format!("{name}.sha256");
        let other = "markdone-v9.0.0-riscv64-unknown-linux-gnu.tar.gz";
        let release =
            newer_release(api_release("v9.0.0", &[other, &name, &sums]), "0.5.0").expect("newer");
        assert_eq!(release.version, "9.0.0");
        assert_eq!(release.archive.as_ref().map(|a| &a.name), Some(&name));
        assert_eq!(release.checksum.as_ref().map(|a| &a.name), Some(&sums));
        // Without this platform's package it can be announced, not installed.
        let bare = newer_release(api_release("v9.0.0", &[other]), "0.5.0").expect("newer");
        assert!(install_problem(&bare).is_some());
    }

    #[test]
    fn checksum_lines_parse() {
        let hash = "AB".repeat(32);
        assert_eq!(
            parse_checksum(&format!("{hash}  markdone.tar.gz\n")),
            Some(hash.to_ascii_lowercase())
        );
        assert_eq!(parse_checksum("not a hash  x"), None);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extracts_the_executable_from_a_release_archive() {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, data) in [
            ("markdone-v9.0.0-x/README.md", &b"readme"[..]),
            ("markdone-v9.0.0-x/markdone", &b"new program"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, data).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        let files = extract(&gz.finish().unwrap()).unwrap();
        assert_eq!(files.exe, b"new program");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_keeps_the_executable_bit_and_leaves_no_staging_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("markdone-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("markdone");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        replace_exe(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(!staging_path(&exe).exists());
        assert!(writable(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
