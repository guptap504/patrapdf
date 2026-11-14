//! Automatic updates from GitHub releases.
//!
//! About once a day (and when asked) the app fetches the latest release from the
//! GitHub API. If it is newer than the running version, the package matching
//! this install is downloaded in the background and checked before anything is
//! touched:
//!
//! 1. `SHA256SUMS.txt.minisig` must be a valid minisign signature of
//!    `SHA256SUMS.txt` by the release key built into the app ([`PUBLIC_KEY`]),
//!    with a trusted comment naming the release's version;
//! 2. the package's SHA-256 must match its line in `SHA256SUMS.txt`, and its size
//!    the size GitHub reports.
//!
//! A verified update is installed when the user chooses "Restart Now", or
//! otherwise the next time the app quits.
//!
//! Self-updating installs: the macOS app, the Windows installer and portable
//! .exe, and the Linux AppImage. Installs from .deb/.rpm are left to the system
//! package manager, and development builds never update.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{about, signatures};

/// Public half of the release-signing key (minisign, key id ACB329C9C59F44C0).
/// The release workflow signs `SHA256SUMS.txt` with the secret half.
const PUBLIC_KEY: &str = "RWTARJ/FySmzrKbcWQS1MDc3JSBXIFZ35/3IJLRtZAVfjR+80jlr58Zt";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/guptap504/patrapdf/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/guptap504/patrapdf/releases/latest";
const SUMS: &str = "SHA256SUMS.txt";
const SUMS_SIGNATURE: &str = "SHA256SUMS.txt.minisig";

/// Check this often (seconds).
const CHECK_INTERVAL: i64 = 24 * 3600;
/// After a failed check (offline, rate-limited, …), try again this much sooner.
const RETRY_AFTER_FAILURE: i64 = 3600;
/// Don't compete with startup.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_PACKAGE_BYTES: u64 = 300 << 20;
const MAX_METADATA_BYTES: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// What kind of install is this?
// ---------------------------------------------------------------------------

/// A copy of the app that can replace itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// `…/PatraPDF.app` on macOS.
    MacApp { bundle: PathBuf },
    /// Installed with the Windows installer (for this user, or for everyone).
    WindowsInstaller { all_users: bool },
    /// The .exe from the portable zip.
    WindowsPortable { exe: PathBuf },
    /// A Linux AppImage.
    AppImage { path: PathBuf },
}

/// Why this copy of the app doesn't update itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoUpdates {
    Development,
    PackageManager,
    /// macOS: running from the disk image, or quarantined ("translocated").
    NotInApplications,
    Unknown,
}

impl NoUpdates {
    pub fn message(self) -> &'static str {
        match self {
            NoUpdates::Development => "Development builds don't update themselves.",
            NoUpdates::PackageManager => "Updates for this copy come from your system's package manager.",
            NoUpdates::NotInApplications => "Move PatraPDF to your Applications folder to get automatic updates.",
            NoUpdates::Unknown => "This copy of PatraPDF can't update itself. Download new versions from GitHub.",
        }
    }
}

/// How the running app was installed.
pub fn detect() -> Result<Install, NoUpdates> {
    if cfg!(debug_assertions) && std::env::var_os("PATRAPDF_UPDATE_DEV").is_none() {
        return Err(NoUpdates::Development);
    }
    let Ok(exe) = std::env::current_exe() else { return Err(NoUpdates::Unknown) };
    // Resolve symlinks (/usr/bin/x → …), but keep Windows paths free of the \\?\ prefix.
    let exe = if cfg!(windows) { exe } else { exe.canonicalize().unwrap_or(exe) };
    let program_files = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(PathBuf::from))
        .collect::<Vec<_>>();
    detect_from(std::env::consts::OS, &exe, std::env::var_os("APPIMAGE").map(PathBuf::from), &program_files)
}

fn detect_from(os: &str, exe: &Path, appimage: Option<PathBuf>, program_files: &[PathBuf]) -> Result<Install, NoUpdates> {
    match os {
        "macos" => {
            let macos = exe.parent().filter(|p| p.ends_with("Contents/MacOS")).ok_or(NoUpdates::Unknown)?;
            let bundle = macos.parent().and_then(Path::parent).ok_or(NoUpdates::Unknown)?;
            if bundle.extension().is_none_or(|e| e != "app") {
                return Err(NoUpdates::Unknown);
            }
            let text = bundle.to_string_lossy();
            if text.contains("/AppTranslocation/") || bundle.starts_with("/Volumes") {
                return Err(NoUpdates::NotInApplications);
            }
            Ok(Install::MacApp { bundle: bundle.to_path_buf() })
        }
        "windows" => {
            let dir = exe.parent().ok_or(NoUpdates::Unknown)?;
            if dir.join("unins000.exe").exists() {
                let lower = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
                let dir = lower(dir);
                let all_users = program_files
                    .iter()
                    .map(|p| lower(p))
                    .any(|pf| !pf.is_empty() && dir.starts_with(pf.trim_end_matches('\\')));
                Ok(Install::WindowsInstaller { all_users })
            } else {
                Ok(Install::WindowsPortable { exe: exe.to_path_buf() })
            }
        }
        "linux" => match appimage {
            // `has_root`, not `is_absolute`: the tests run this branch on Windows too.
            Some(path) if path.has_root() => Ok(Install::AppImage { path }),
            _ if exe.starts_with("/usr") || exe.starts_with("/opt") => Err(NoUpdates::PackageManager),
            _ => Err(NoUpdates::Unknown),
        },
        _ => Err(NoUpdates::Unknown),
    }
}

/// The release file this install updates from.
fn asset_name(install: &Install, version: &Version) -> String {
    match install {
        Install::MacApp { .. } => {
            let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x86_64" };
            format!("PatraPDF-{version}-macos-{arch}.dmg")
        }
        Install::WindowsInstaller { .. } => format!("PatraPDF-{version}-windows-x64-setup.exe"),
        Install::WindowsPortable { .. } => format!("PatraPDF-{version}-windows-x64-portable.zip"),
        Install::AppImage { .. } => format!("PatraPDF-{version}-linux-x86_64.AppImage"),
    }
}

// ---------------------------------------------------------------------------
// GitHub releases
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

impl Release {
    fn asset(&self, name: &str) -> Result<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| anyhow!("the release has no {name}"))
    }
}

/// The release's version, if it's a final release newer than `current`.
pub fn newer_version(release: &Release, current: &Version) -> Option<Version> {
    if release.draft || release.prerelease {
        return None;
    }
    let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
    (version.pre.is_empty() && version > *current).then_some(version)
}

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        // The OS trust store, so corporate proxies with their own CA keep working.
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .user_agent(format!("patrapdf/{} ({})", about::VERSION, std::env::consts::OS))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// Only HTTPS, except to this machine (tests and local release rehearsals).
fn check_url(url: &str) -> Result<()> {
    let local = ["http://127.0.0.1:", "http://localhost:"].iter().any(|p| url.starts_with(p));
    ensure!(url.starts_with("https://") || local, "refusing to download over an insecure connection: {url}");
    Ok(())
}

fn friendly(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(403 | 429) => anyhow!("GitHub is limiting requests from this network. Try again later."),
        ureq::Error::StatusCode(404) => anyhow!("No release was found on GitHub."),
        ureq::Error::StatusCode(code) => anyhow!("GitHub answered with an error (HTTP {code})."),
        ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Timeout(_) => {
            anyhow!("Couldn't connect to GitHub. Check your internet connection.")
        }
        other => anyhow!(other),
    }
}

fn get_small(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    check_url(url)?;
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(friendly)?;
    Ok(response.body_mut().with_config().limit(MAX_METADATA_BYTES).read_to_vec().map_err(friendly)?)
}

pub fn fetch_latest(agent: &ureq::Agent, url: &str) -> Result<Release> {
    let bytes = get_small(agent, url)?;
    serde_json::from_slice(&bytes).context("GitHub sent an unexpected reply")
}

/// Check that `sums` was signed by `public_key` for release `version`.
fn verify_sums(sums: &[u8], signature: &str, version: &Version, public_key: &str) -> Result<()> {
    use minisign_verify::{PublicKey, Signature};
    let key = PublicKey::from_base64(public_key).map_err(|e| anyhow!("bad public key: {e}"))?;
    let signature = Signature::decode(signature).map_err(|_| anyhow!("the update's signature is malformed"))?;
    key.verify(sums, &signature, false)
        .map_err(|_| anyhow!("the update's signature doesn't match. It was not installed."))?;
    // The trusted comment is covered by the signature: "patrapdf v1.2.3".
    let expected = format!("v{version}");
    ensure!(
        signature.trusted_comment().split_whitespace().any(|w| w == expected),
        "the update's signature is for a different version"
    );
    Ok(())
}

/// The SHA-256 listed for `name` in a `sha256sum` file.
fn checksum_for(sums: &str, name: &str) -> Option<[u8; 32]> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name).then(|| hex32(hash)).flatten()
    })
}

fn hex32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let digit = |b: u8| (b as char).to_digit(16);
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

/// Download and verify the update for `install` from `release` into `dir`.
/// Returns the verified package. Runs on a background thread.
#[allow(clippy::too_many_arguments)]
fn download_update(
    agent: &ureq::Agent,
    release: &Release,
    install: &Install,
    version: &Version,
    dir: &Path,
    public_key: &str,
    progress: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let name = asset_name(install, version);
    let asset = release.asset(&name)?;
    ensure!(asset.size > 0 && asset.size <= MAX_PACKAGE_BYTES, "the update has an unexpected size");

    // Signed checksums first: nothing big is downloaded for a bad release.
    let sums = get_small(agent, &release.asset(SUMS)?.browser_download_url)?;
    let signature = get_small(agent, &release.asset(SUMS_SIGNATURE)?.browser_download_url)?;
    verify_sums(&sums, &String::from_utf8_lossy(&signature), version, public_key)?;
    let expected = checksum_for(&String::from_utf8_lossy(&sums), &name)
        .ok_or_else(|| anyhow!("{name} isn't listed in the release's checksums"))?;

    fs::create_dir_all(dir)?;
    let path = dir.join(&name);
    let partial = dir.join(format!("{name}.part"));
    let result = download_to(agent, &asset.browser_download_url, &partial, asset.size, &expected, progress, cancel);
    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &path)?;
    Ok(path)
}

fn download_to(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    size: u64,
    expected: &[u8; 32],
    progress: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<()> {
    check_url(url)?;
    let response = agent
        .get(url)
        .config()
        .timeout_recv_body(Some(Duration::from_secs(30 * 60)))
        .build()
        .call()
        .map_err(friendly)?;
    let mut reader = response.into_body().into_with_config().limit(size + 1).reader();
    let mut file = fs::File::create(path).with_context(|| format!("couldn't write {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "the download was cancelled");
        let n = reader.read(&mut buf).context("the download was interrupted")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= size, "the download is larger than expected");
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        progress.store(total, Ordering::Relaxed);
    }
    ensure!(total == size, "the download is incomplete");
    file.sync_all()?;
    let digest: [u8; 32] = hasher.finalize().into();
    ensure!(&digest == expected, "the download is damaged (its checksum doesn't match). It was not installed.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

/// Downloads go here (cleared on the next launch).
fn downloads_dir() -> PathBuf {
    signatures::app_data_dir().join("updates")
}

/// Replace the running app with the verified `package`. With `relaunch`, start
/// the new version once this process has exited (the caller quits right after).
fn install_package(install: &Install, package: &Path, relaunch: bool) -> Result<()> {
    match install {
        Install::MacApp { bundle } => install_mac(bundle, package, relaunch),
        Install::WindowsInstaller { all_users } => {
            // Inno Setup: silent upgrade in place; /RELAUNCH is handled by our
            // installer script and starts the app when it's done.
            let mut cmd = Command::new(package);
            cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/CLOSEAPPLICATIONS"])
                .arg(if *all_users { "/ALLUSERS" } else { "/CURRENTUSER" });
            if relaunch {
                cmd.arg("/RELAUNCH");
            }
            cmd.spawn().context("couldn't start the installer")?;
            Ok(())
        }
        Install::WindowsPortable { exe } => {
            let new_exe = package.with_file_name("patrapdf.new.exe");
            extract_exe(package, &new_exe)?;
            self_replace::self_replace(&new_exe).context("couldn't replace patrapdf.exe")?;
            let _ = fs::remove_file(&new_exe);
            if relaunch {
                Command::new(exe).spawn().context("couldn't restart PatraPDF")?;
            }
            Ok(())
        }
        Install::AppImage { path } => {
            replace_file(package, path)?;
            if relaunch {
                relaunch_after_exit(path, &["APPIMAGE", "APPDIR", "ARGV0", "OWD"])?;
            }
            Ok(())
        }
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let output = cmd.stdin(Stdio::null()).output().with_context(|| format!("couldn't run {cmd:?}"))?;
    ensure!(
        output.status.success(),
        "{:?} failed: {}",
        cmd.get_program(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{suffix}"))
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Swap the .app bundle for the one in `dmg`. The old bundle is moved aside
/// (and deleted on the next launch); if the swap fails it is put back.
fn install_mac(bundle: &Path, dmg: &Path, relaunch: bool) -> Result<()> {
    let staged = sibling(bundle, "update");
    let old = sibling(bundle, "old");
    remove_any(&staged)?;

    let mount = dmg.with_file_name(format!("mount-{}", std::process::id()));
    fs::create_dir_all(&mount)?;
    run(Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(&mount)
        .arg(dmg))
    .context("couldn't open the update's disk image")?;
    let copied = (|| {
        let app = fs::read_dir(&mount)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or_else(|| anyhow!("the disk image has no app"))?;
        run(Command::new("ditto").arg(&app).arg(&staged)).context("couldn't copy the new version")
    })();
    let _ = run(Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount));
    let _ = fs::remove_dir(&mount);
    copied?;

    let swapped = (|| {
        run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&staged))
            .context("the new version failed code-signature verification")?;
        remove_any(&old)?;
        fs::rename(bundle, &old).context("couldn't move the old version aside")?;
        if let Err(e) = fs::rename(&staged, bundle) {
            let _ = fs::rename(&old, bundle);
            return Err(e).context("couldn't put the new version in place");
        }
        Ok(())
    })();
    if swapped.is_err() {
        let _ = remove_any(&staged);
    }
    swapped?;
    if relaunch {
        relaunch_after_exit(bundle, &[])?;
    }
    Ok(())
}

/// Atomically replace `target` with a copy of `source` (same directory, then rename).
fn replace_file(source: &Path, target: &Path) -> Result<()> {
    let temp = sibling(target, "new");
    fs::copy(source, &temp).with_context(|| format!("couldn't write next to {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(e).with_context(|| format!("couldn't replace {}", target.display()));
    }
    Ok(())
}

/// Start `program` (an app bundle on macOS) after this process exits.
#[cfg(unix)]
fn relaunch_after_exit(program: &Path, clear_env: &[&str]) -> Result<()> {
    let open = if cfg!(target_os = "macos") { "exec open \"$2\"" } else { "exec \"$2\"" };
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!("while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; {open}"))
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for var in clear_env {
        cmd.env_remove(var);
    }
    cmd.spawn().context("couldn't schedule the restart")?;
    Ok(())
}

#[cfg(not(unix))]
fn relaunch_after_exit(program: &Path, _: &[&str]) -> Result<()> {
    Command::new(program).spawn().context("couldn't restart PatraPDF")?;
    Ok(())
}

/// Pull `patrapdf.exe` out of the portable zip.
fn extract_exe(zip_path: &Path, out: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip_path)?).context("the update isn't a valid zip")?;
    let index = (0..archive.len())
        .find(|&i| {
            archive
                .by_index(i)
                .ok()
                .and_then(|f| f.enclosed_name())
                .is_some_and(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("patrapdf.exe")))
        })
        .ok_or_else(|| anyhow!("the update has no patrapdf.exe"))?;
    let entry = archive.by_index(index)?;
    ensure!(entry.size() <= MAX_PACKAGE_BYTES, "patrapdf.exe in the update is too large");
    let mut file = fs::File::create(out)?;
    std::io::copy(&mut entry.take(MAX_PACKAGE_BYTES), &mut file)?;
    file.sync_all()?;
    Ok(())
}

/// Remove what earlier updates left behind: downloads, and the old app bundle or
/// AppImage that was moved aside while the app was running.
fn clean_up(install: &Install) {
    let _ = remove_any(&downloads_dir());
    match install {
        Install::MacApp { bundle } => {
            let _ = remove_any(&sibling(bundle, "old"));
            let _ = remove_any(&sibling(bundle, "update"));
        }
        Install::AppImage { path } => {
            let _ = remove_any(&sibling(path, "new"));
        }
        Install::WindowsInstaller { .. } | Install::WindowsPortable { .. } => {}
    }
}

/// GitHub's generated release notes, as a few plain lines for a dialog.
pub fn summarize_notes(body: &str) -> String {
    const MAX_LINES: usize = 8;
    let lines: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("**Full Changelog**") && !l.starts_with('#'))
        .map(|l| {
            let l = l.trim_start_matches(['*', '-']).trim();
            // "Fix X by @someone in https://github.com/…/pull/3" → "Fix X"
            let l = l.split(" by @").next().unwrap_or(l);
            let l = l.split(" in https://").next().unwrap_or(l);
            format!("• {l}")
        })
        .collect();
    let mut text = lines.iter().take(MAX_LINES).cloned().collect::<Vec<_>>().join("\n");
    if lines.len() > MAX_LINES {
        text.push_str(&format!("\n…and {} more", lines.len() - MAX_LINES));
    }
    if text.chars().count() > 700 {
        text = text.chars().take(700).collect::<String>() + "…";
    }
    text
}

// ---------------------------------------------------------------------------
// The updater as app state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    Downloading { version: Version, fraction: f32 },
    /// Downloaded and verified; installed on restart, or when the app quits.
    Ready { version: Version },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// After "Check for Updates": already on the newest version.
    UpToDate,
    /// After "Check for Updates": it didn't work (message for the user).
    Failed(String),
    /// A verified update is ready to install. `manual`: the user asked for it.
    Ready { manual: bool },
}

/// What the user should know about a release.
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub version: Version,
    pub notes: String,
}

/// Remembered between launches (`update-state.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Unix time of the last check (successful or not, adjusted for retries).
    last_check: i64,
    /// "Skip This Version": don't offer this one automatically.
    skipped_version: Option<String>,
}

impl State {
    fn path() -> PathBuf {
        signatures::app_data_dir().join("update-state.json")
    }

    fn load() -> Self {
        fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let path = Self::path();
        let result = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| fs::write(&path, serde_json::to_vec_pretty(self).unwrap_or_default()));
        if let Err(e) = result {
            eprintln!("couldn't save update state: {e}");
        }
    }
}

pub struct Updater {
    install: Result<Install, NoUpdates>,
    enabled: bool,
    status: Status,
    release: Option<ReleaseInfo>,
    package: Option<PathBuf>,
    state: State,
    manual: bool,
    feed_url: String,
    public_key: String,
    cancel: Arc<AtomicBool>,
    work: Option<Task<()>>,
    _poll: Task<()>,
    _quit: Subscription,
}

struct GlobalUpdater(Entity<Updater>);
impl Global for GlobalUpdater {}

impl EventEmitter<Event> for Updater {}

/// Create the app's updater and start the daily checks.
pub fn init(enabled: bool, cx: &mut App) -> Entity<Updater> {
    // A release can be rehearsed against a local server; the signature check still applies.
    let feed = std::env::var("PATRAPDF_UPDATE_URL").unwrap_or_else(|_| LATEST_RELEASE_URL.into());
    let updater = cx.new(|cx| Updater::new(detect(), enabled, feed, PUBLIC_KEY.into(), cx));
    cx.set_global(GlobalUpdater(updater.clone()));
    updater
}

pub fn global(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<GlobalUpdater>().map(|g| g.0.clone())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Updater {
    fn new(
        install: Result<Install, NoUpdates>,
        enabled: bool,
        feed_url: String,
        public_key: String,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Ok(install) = install.clone() {
            cx.background_spawn(async move { clean_up(&install) }).detach();
        }
        let poll = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_DELAY).await;
            loop {
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.is_due() {
                        this.check(false, cx);
                    }
                }) else {
                    break;
                };
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        });
        let quit = cx.on_app_quit(|this, _| {
            this.install_on_quit();
            async {}
        });
        Self {
            install,
            enabled,
            status: Status::Idle,
            release: None,
            package: None,
            state: State::load(),
            manual: false,
            feed_url,
            public_key,
            cancel: Arc::default(),
            work: None,
            _poll: poll,
            _quit: quit,
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether the user asked for the current check (show its progress).
    pub fn is_manual(&self) -> bool {
        self.manual
    }

    pub fn release(&self) -> Option<&ReleaseInfo> {
        self.release.as_ref()
    }

    /// `Ok` if this copy of the app updates itself, else why not.
    pub fn support(&self) -> Result<(), NoUpdates> {
        self.install.as_ref().map(|_| ()).map_err(|e| *e)
    }

    /// Turn the automatic checks on or off ("Check for Updates" still works).
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.enabled = enabled;
        if !enabled && !self.manual && matches!(self.status, Status::Checking | Status::Downloading { .. }) {
            self.cancel_work(cx);
        }
        cx.notify();
    }

    fn is_due(&self) -> bool {
        self.enabled && self.install.is_ok() && self.status == Status::Idle && now() - self.state.last_check >= CHECK_INTERVAL
    }

    fn cancel_work(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.work = None;
        self.status = Status::Idle;
        self.manual = false;
        cx.notify();
    }

    /// Look for a newer release and, if there is one, download and verify it.
    /// `manual`: the user asked ("Check for Updates"), so report the outcome and
    /// offer even a version they skipped.
    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        let install = match &self.install {
            Ok(install) => install.clone(),
            Err(reason) => {
                if manual {
                    cx.emit(Event::Failed(reason.message().into()));
                }
                return;
            }
        };
        match self.status {
            Status::Checking | Status::Downloading { .. } => {
                if manual && !self.manual {
                    self.manual = true;
                    cx.notify();
                }
                return;
            }
            Status::Ready { .. } => {
                if manual {
                    cx.emit(Event::Ready { manual: true });
                }
                return;
            }
            Status::Idle => {}
        }

        self.manual = manual;
        self.status = Status::Checking;
        self.state.last_check = now();
        self.state.save();
        cx.notify();

        let feed_url = self.feed_url.clone();
        let public_key = self.public_key.clone();
        let skipped = if manual { None } else { self.state.skipped_version.clone() };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let progress = Arc::new(AtomicU64::new(0));

        self.work = Some(cx.spawn(async move |this, cx| {
            let agent = agent();
            let release = {
                let (agent, url) = (agent.clone(), feed_url.clone());
                cx.background_spawn(async move { fetch_latest(&agent, &url) }).await
            };
            let release = match release {
                Ok(release) => release,
                Err(e) => return Self::finish_with_error(&this, e, cx),
            };
            let current = Version::parse(about::VERSION).expect("the crate version is valid semver");
            let version = newer_version(&release, &current)
                .filter(|v| skipped.as_deref() != Some(v.to_string().as_str()));
            let Some(version) = version else {
                let _ = this.update(cx, |this, cx| {
                    if this.manual {
                        cx.emit(Event::UpToDate);
                    }
                    this.status = Status::Idle;
                    this.manual = false;
                    cx.notify();
                });
                return;
            };
            let info = ReleaseInfo {
                version: version.clone(),
                notes: summarize_notes(release.body.as_deref().unwrap_or_default()),
            };
            let total = release
                .asset(&asset_name(&install, &version))
                .map(|a| a.size.max(1))
                .unwrap_or(1);
            let _ = this.update(cx, |this, cx| {
                this.status = Status::Downloading { version: version.clone(), fraction: 0. };
                cx.notify();
            });

            let job = {
                let (progress, version) = (progress.clone(), version.clone());
                cx.background_spawn(async move {
                    download_update(&agent, &release, &install, &version, &downloads_dir(), &public_key, &progress, &cancel)
                })
            };
            // Report progress while the download runs.
            let mut job = std::pin::pin!(job);
            let result = loop {
                let tick = cx.background_executor().timer(Duration::from_millis(250));
                match futures::future::select(job.as_mut(), tick).await {
                    futures::future::Either::Left((result, _)) => break result,
                    futures::future::Either::Right(_) => {
                        let fraction = progress.load(Ordering::Relaxed) as f32 / total as f32;
                        let _ = this.update(cx, |this, cx| {
                            if let Status::Downloading { fraction: f, .. } = &mut this.status {
                                *f = fraction.min(1.);
                                cx.notify();
                            }
                        });
                    }
                }
            };
            match result {
                Ok(package) => {
                    let _ = this.update(cx, |this, cx| {
                        this.package = Some(package);
                        this.release = Some(info);
                        this.status = Status::Ready { version };
                        cx.emit(Event::Ready { manual: this.manual });
                        this.manual = false;
                        cx.notify();
                    });
                }
                Err(e) => Self::finish_with_error(&this, e, cx),
            }
        }));
    }

    fn finish_with_error(this: &gpui::WeakEntity<Self>, error: anyhow::Error, cx: &mut gpui::AsyncApp) {
        eprintln!("update check failed: {error:#}");
        let _ = this.update(cx, |this, cx| {
            if this.manual {
                cx.emit(Event::Failed(format!("{error:#}")));
            } else {
                // Try again in an hour rather than tomorrow.
                this.state.last_check = now() - CHECK_INTERVAL + RETRY_AFTER_FAILURE;
                this.state.save();
            }
            this.status = Status::Idle;
            this.manual = false;
            cx.notify();
        });
    }

    /// Don't offer the ready update again (until a newer one comes out).
    pub fn skip(&mut self, cx: &mut Context<Self>) {
        if let Status::Ready { version } = &self.status {
            self.state.skipped_version = Some(version.to_string());
            self.state.save();
        }
        if let Some(package) = self.package.take() {
            let _ = fs::remove_file(package);
        }
        self.release = None;
        self.status = Status::Idle;
        cx.notify();
    }

    /// Install the ready update and arrange for the new version to start. The
    /// caller quits the app on success.
    pub fn install_and_restart(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let install = self.install.clone().map_err(|reason| anyhow!(reason.message()))?;
        let package = self.package.take().ok_or_else(|| anyhow!("no update is ready"))?;
        let result = install_package(&install, &package, true);
        if result.is_err() {
            self.release = None;
            self.status = Status::Idle;
            let _ = fs::remove_file(&package);
            cx.notify();
        }
        result
    }

    /// Quitting with an update ready: install it now, so the next launch is new.
    fn install_on_quit(&mut self) {
        if let (Ok(install), Some(package)) = (&self.install, self.package.take()) {
            if let Err(e) = install_package(install, &package, false) {
                eprintln!("couldn't install the update: {e:#}");
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::rc::Rc;

    // A test key and fixtures made with minisign 0.12 (see the release workflow):
    //   minisign -S -s test.key -m SHA256SUMS.txt -t "patrapdf v9.9.9"
    const TEST_KEY: &str = "RWQF6wjw1D9BGAxlSAJYc5+nga2aURBqrV6a8OHwKLS5rC2+aXrTQhDU";
    const TEST_SUMS: &str = "0000000000000000000000000000000000000000000000000000000000000000  PatraPDF-9.9.9-macos-arm64.dmg\n37444e7f2e62969ea08184b0e3ad3dee965bf888a6380fe4087cf231322f1f9f  PatraPDF-9.9.9-linux-x86_64.AppImage\n";
    const TEST_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUQF6wjw1D9BGH0eSbTAUoQGIO3LRAr7JfMaqtjJjnvGZgNb5vUhjbVc7LYQqIPD9rytS/K3gnvNWgHyabpFfGO/byNigk1IXAk=
trusted comment: patrapdf v9.9.9
iGRB6/YzdNLb7YjqOANKbqyVKaED8jM0rwg3dd1X+97zdP4sIQNMBrSBF1jbliwtcAs54pQbrW0+k0cQV67OCQ==
";

    fn test_package() -> Vec<u8> {
        b"PatraPDF test package\n".repeat(4000)
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn release(tag: &str, base: &str, package_size: u64) -> Release {
        let asset = |name: &str, size: u64| Asset {
            name: name.into(),
            browser_download_url: format!("{base}/{name}"),
            size,
        };
        Release {
            tag_name: tag.into(),
            body: Some("## What's Changed\n* Add page reordering by @someone in https://github.com/x/y/pull/1\n\n**Full Changelog**: https://…".into()),
            draft: false,
            prerelease: false,
            assets: vec![
                asset("PatraPDF-9.9.9-linux-x86_64.AppImage", package_size),
                asset(SUMS, TEST_SUMS.len() as u64),
                asset(SUMS_SIGNATURE, TEST_SIGNATURE.len() as u64),
            ],
        }
    }

    /// A tiny HTTP server on 127.0.0.1 answering GETs with `files(base_url)`
    /// (path → body). Returns the base URL; the server lives for the test run.
    pub(crate) fn serve(files: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files = files(&base);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut stream = stream;
                match files.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        base
    }

    #[test]
    fn detects_how_the_app_was_installed() {
        use Install::*;
        let p = PathBuf::from;
        assert_eq!(
            detect_from("macos", &p("/Applications/PatraPDF.app/Contents/MacOS/patrapdf"), None, &[]),
            Ok(MacApp { bundle: p("/Applications/PatraPDF.app") })
        );
        assert_eq!(
            detect_from("macos", &p("/private/var/folders/x/AppTranslocation/ABC/d/PatraPDF.app/Contents/MacOS/patrapdf"), None, &[]),
            Err(NoUpdates::NotInApplications)
        );
        assert_eq!(
            detect_from("macos", &p("/Volumes/PatraPDF/PatraPDF.app/Contents/MacOS/patrapdf"), None, &[]),
            Err(NoUpdates::NotInApplications)
        );
        assert_eq!(detect_from("macos", &p("/Users/me/code/target/release/patrapdf"), None, &[]), Err(NoUpdates::Unknown));

        assert_eq!(
            detect_from("linux", &p("/tmp/.mount_PDFxyz/usr/bin/patrapdf"), Some(p("/home/me/Apps/PatraPDF.AppImage")), &[]),
            Ok(AppImage { path: p("/home/me/Apps/PatraPDF.AppImage") })
        );
        assert_eq!(detect_from("linux", &p("/usr/bin/patrapdf"), None, &[]), Err(NoUpdates::PackageManager));
        assert_eq!(detect_from("linux", &p("/home/me/.cargo/bin/patrapdf"), None, &[]), Err(NoUpdates::Unknown));

        // Windows: the installer leaves an uninstaller next to the exe.
        let dir = std::env::temp_dir().join(format!("patrapdf-detect-{}", std::process::id()));
        let installed = dir.join("Programs/PatraPDF");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("unins000.exe"), b"").unwrap();
        assert_eq!(
            detect_from("windows", &installed.join("patrapdf.exe"), None, &[p("C:\\Program Files")]),
            Ok(WindowsInstaller { all_users: false })
        );
        assert_eq!(
            detect_from("windows", &installed.join("patrapdf.exe"), None, &[dir.join("Programs")]),
            Ok(WindowsInstaller { all_users: true })
        );
        assert_eq!(
            detect_from("windows", &dir.join("patrapdf.exe"), None, &[]),
            Ok(WindowsPortable { exe: dir.join("patrapdf.exe") })
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn package_names_match_the_release_workflow() {
        let version = v("1.2.3");
        let name = |install| asset_name(&install, &version);
        assert!(name(Install::MacApp { bundle: "/A.app".into() }).starts_with("PatraPDF-1.2.3-macos-"));
        assert_eq!(name(Install::WindowsInstaller { all_users: false }), "PatraPDF-1.2.3-windows-x64-setup.exe");
        assert_eq!(name(Install::WindowsPortable { exe: "x".into() }), "PatraPDF-1.2.3-windows-x64-portable.zip");
        assert_eq!(name(Install::AppImage { path: "x".into() }), "PatraPDF-1.2.3-linux-x86_64.AppImage");
    }

    #[test]
    fn only_newer_final_releases_count() {
        let current = v("0.2.0");
        let mut r = release("v0.3.0", "https://x", 1);
        assert_eq!(newer_version(&r, &current), Some(v("0.3.0")));
        r.tag_name = "v0.2.0".into();
        assert_eq!(newer_version(&r, &current), None);
        r.tag_name = "v0.1.9".into();
        assert_eq!(newer_version(&r, &current), None);
        r.tag_name = "v0.10.0".into();
        assert_eq!(newer_version(&r, &current), Some(v("0.10.0")), "compared as versions, not text");
        r.tag_name = "v0.3.0-beta.1".into();
        assert_eq!(newer_version(&r, &current), None);
        r.tag_name = "v0.3.0".into();
        r.prerelease = true;
        assert_eq!(newer_version(&r, &current), None);
        r.prerelease = false;
        r.draft = true;
        assert_eq!(newer_version(&r, &current), None);
        r.draft = false;
        r.tag_name = "nightly".into();
        assert_eq!(newer_version(&r, &current), None);
    }

    #[test]
    fn signatures_and_checksums_are_enforced() {
        let version = v("9.9.9");
        verify_sums(TEST_SUMS.as_bytes(), TEST_SIGNATURE, &version, TEST_KEY).unwrap();
        // Tampered checksums.
        let tampered = TEST_SUMS.replace("3744", "3745");
        assert!(verify_sums(tampered.as_bytes(), TEST_SIGNATURE, &version, TEST_KEY).is_err());
        // A genuine signature for another release can't be replayed.
        assert!(verify_sums(TEST_SUMS.as_bytes(), TEST_SIGNATURE, &v("9.9.10"), TEST_KEY).is_err());
        // Signed by a different key (the real release key).
        assert!(verify_sums(TEST_SUMS.as_bytes(), TEST_SIGNATURE, &version, PUBLIC_KEY).is_err());
        assert!(verify_sums(TEST_SUMS.as_bytes(), "garbage", &version, TEST_KEY).is_err());

        let hash = checksum_for(TEST_SUMS, "PatraPDF-9.9.9-linux-x86_64.AppImage").unwrap();
        assert_eq!(hash, <[u8; 32]>::from(Sha256::digest(test_package())));
        assert_eq!(checksum_for(TEST_SUMS, "PatraPDF-9.9.9-linux-x86_64"), None);
        // `sha256sum -b` marks binary files with '*'.
        assert_eq!(checksum_for(&format!("{}  *a.zip\n", "ab".repeat(32)), "a.zip"), Some([0xab; 32]));
        assert_eq!(checksum_for("xyz  a.zip", "a.zip"), None);
    }

    #[test]
    fn insecure_urls_are_refused() {
        assert!(check_url("https://github.com/x").is_ok());
        assert!(check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(check_url("http://github.com/x").is_err());
        assert!(check_url("http://127.0.0.1.evil.com/x").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
    }

    fn download(base: &str, size: u64) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("patrapdf-dl-{}-{size}", std::process::id()));
        let install = Install::AppImage { path: "/x".into() };
        let r = release("v9.9.9", base, size);
        let (progress, cancel) = (AtomicU64::new(0), AtomicBool::new(false));
        download_update(&agent(), &r, &install, &v("9.9.9"), &dir, TEST_KEY, &progress, &cancel)
    }

    #[test]
    fn downloads_are_verified_end_to_end() {
        let package = test_package();
        let files = |pkg: Vec<u8>, sums: &str| {
            vec![
                ("/PatraPDF-9.9.9-linux-x86_64.AppImage".to_string(), pkg),
                (format!("/{SUMS}"), sums.as_bytes().to_vec()),
                (format!("/{SUMS_SIGNATURE}"), TEST_SIGNATURE.as_bytes().to_vec()),
            ]
        };

        let base = serve(|_| files(package.clone(), TEST_SUMS));
        let path = download(&base, package.len() as u64).unwrap();
        assert_eq!(fs::read(&path).unwrap(), package);
        assert!(!path.with_file_name(format!("{}.part", path.file_name().unwrap().to_string_lossy())).exists());

        // A damaged package is rejected.
        let mut damaged = package.clone();
        damaged[10] ^= 1;
        let base = serve(|_| files(damaged, TEST_SUMS));
        let err = download(&base, package.len() as u64).unwrap_err();
        assert!(format!("{err:#}").contains("checksum"), "{err:#}");

        // Checksums that don't match their signature: the package isn't even fetched.
        let base = serve(|_| files(package.clone(), &TEST_SUMS.replace("3744", "3745")));
        let err = download(&base, package.len() as u64).unwrap_err();
        assert!(format!("{err:#}").contains("signature"), "{err:#}");

        // More data than GitHub said the file has.
        let mut bigger = package.clone();
        bigger.extend_from_slice(b"extra");
        let base = serve(|_| files(bigger, TEST_SUMS));
        let err = download(&base, package.len() as u64).unwrap_err();
        assert!(format!("{err:#}").contains("larger"), "{err:#}");
    }

    #[test]
    fn release_notes_become_a_short_summary() {
        let notes = summarize_notes(
            "## What's Changed\n* Add page reordering by @a in https://github.com/x/y/pull/1\n* Fix zoom by @b in https://github.com/x/y/pull/2\n\n**Full Changelog**: https://github.com/x/y/compare/v0.1.0...v0.2.0",
        );
        assert_eq!(notes, "• Add page reordering\n• Fix zoom");
        let many = (0..12).map(|i| format!("- item {i}")).collect::<Vec<_>>().join("\n");
        assert!(summarize_notes(&many).ends_with("…and 4 more"));
    }

    #[test]
    fn portable_zip_yields_the_exe() {
        let dir = std::env::temp_dir().join(format!("patrapdf-zip-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("portable.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("README.md", options).unwrap();
        writer.write_all(b"readme").unwrap();
        writer.start_file("patrapdf.exe", options).unwrap();
        writer.write_all(b"MZ new version").unwrap();
        writer.finish().unwrap();

        let out = dir.join("patrapdf.new.exe");
        extract_exe(&zip_path, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"MZ new version");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn appimage_is_replaced_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("patrapdf-appimage-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("PatraPDF.AppImage");
        let package = dir.join("download.AppImage");
        fs::write(&current, b"old").unwrap();
        fs::write(&package, b"new").unwrap();
        let install = Install::AppImage { path: current.clone() };
        install_package(&install, &package, false).unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"new");
        assert_eq!(fs::metadata(&current).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!sibling(&current, "new").exists());
        let _ = fs::remove_dir_all(dir);
    }

    /// Builds a real (tiny) .app inside a .dmg and installs it over another one.
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_app_is_swapped_from_the_disk_image() {
        let dir = std::env::temp_dir().join(format!("patrapdf-macswap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let make_app = |root: &Path, marker: &str| {
            let app = root.join("PatraPDF.app");
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::write(app.join("Contents/MacOS/patrapdf"), format!("#!/bin/sh\necho {marker}\n")).unwrap();
            fs::write(
                app.join("Contents/Info.plist"),
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>patrapdf</string><key>CFBundleIdentifier</key><string>test.patrapdf</string></dict></plist>",
            )
            .unwrap();
            run(Command::new("chmod").arg("+x").arg(app.join("Contents/MacOS/patrapdf"))).unwrap();
            run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&app)).unwrap();
            app
        };
        let installed = make_app(&dir.join("Applications"), "old");
        make_app(&dir.join("image"), "new");
        let dmg = dir.join("downloads/update.dmg");
        fs::create_dir_all(dmg.parent().unwrap()).unwrap();
        run(Command::new("hdiutil")
            .args(["create", "-quiet", "-fs", "HFS+", "-format", "UDZO", "-volname", "PatraPDF", "-srcfolder"])
            .arg(dir.join("image"))
            .arg(&dmg))
        .unwrap();

        let install = Install::MacApp { bundle: installed.clone() };
        install_package(&install, &dmg, false).unwrap();
        let script = fs::read_to_string(installed.join("Contents/MacOS/patrapdf")).unwrap();
        assert!(script.contains("new"), "the bundle was replaced");
        assert!(sibling(&installed, "old").exists(), "the old bundle is kept until the next launch");
        assert!(!sibling(&installed, "update").exists());
        assert!(fs::read_dir(dmg.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("mount-")));

        clean_up(&install);
        assert!(!sibling(&installed, "old").exists());
        let _ = fs::remove_dir_all(dir);
    }

    /// The whole flow in the app: check → download → verify → ready, then skip.
    #[gpui::test]
    fn updater_finds_downloads_and_offers_an_update(cx: &mut gpui::TestAppContext) {
        let package = test_package();
        let base = serve(|base| {
            let release = serde_json::json!({
                "tag_name": "v9.9.9",
                "html_url": format!("{base}/release"),
                "body": "* Faster by @a in https://github.com/x/y/pull/1",
                "assets": [
                    {"name": "PatraPDF-9.9.9-linux-x86_64.AppImage", "browser_download_url": format!("{base}/PatraPDF-9.9.9-linux-x86_64.AppImage"), "size": package.len()},
                    {"name": SUMS, "browser_download_url": format!("{base}/{SUMS}"), "size": TEST_SUMS.len()},
                    {"name": SUMS_SIGNATURE, "browser_download_url": format!("{base}/{SUMS_SIGNATURE}"), "size": TEST_SIGNATURE.len()},
                ]
            });
            vec![
                ("/latest".to_string(), release.to_string().into_bytes()),
                ("/PatraPDF-9.9.9-linux-x86_64.AppImage".to_string(), package.clone()),
                (format!("/{SUMS}"), TEST_SUMS.as_bytes().to_vec()),
                (format!("/{SUMS_SIGNATURE}"), TEST_SIGNATURE.as_bytes().to_vec()),
            ]
        });

        let events = Rc::new(RefCell::new(Vec::new()));
        let updater = cx.new(|cx| {
            let install = Ok(Install::AppImage { path: "/nonexistent/PatraPDF.AppImage".into() });
            Updater::new(install, true, format!("{base}/latest"), TEST_KEY.into(), cx)
        });
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&updater, move |_, e: &Event, _| events.borrow_mut().push(e.clone())).detach();
        });
        updater.update(cx, |u, cx| u.check(true, cx));
        cx.run_until_parked();
        updater.read_with(cx, |u, _| {
            assert_eq!(u.status(), &Status::Ready { version: v("9.9.9") });
            assert_eq!(u.release().unwrap().notes, "• Faster");
            assert_eq!(fs::read(u.package.as_ref().unwrap()).unwrap(), package);
        });
        assert_eq!(events.borrow().as_slice(), &[Event::Ready { manual: true }]);

        // Skipping forgets the download, and automatic checks won't offer it again.
        updater.update(cx, |u, cx| u.skip(cx));
        updater.update(cx, |u, cx| u.check(false, cx));
        cx.run_until_parked();
        updater.read_with(cx, |u, _| {
            assert_eq!(u.status(), &Status::Idle);
            assert_eq!(u.state.skipped_version.as_deref(), Some("9.9.9"));
        });
        // Asking explicitly still offers it.
        updater.update(cx, |u, cx| u.check(true, cx));
        cx.run_until_parked();
        updater.read_with(cx, |u, _| assert!(matches!(u.status(), Status::Ready { .. })));
    }
}
