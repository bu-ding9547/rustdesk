use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use hbb_common::{
    anyhow::{anyhow, bail},
    config::Config,
    get_version_number,
    log,
    tls::{get_cached_tls_type, upsert_tls_cache, TlsType},
    tokio,
    ResultType,
};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hbbs_http::{create_http_client_async, get_url_for_tls};

/// The fork whose releases are the update source: the tag of the newest release is the
/// newest version, and its installer plus hash list are what the upgrade uses.
const REPO: &str = "bu-ding9547/rustdesk";
const API: &str = "https://api.github.com";

/// Event the home page listens for, carrying the version and the installer to download.
pub const EVENT_UPDATE_AVAILABLE: &str = "rustdesk_update_available";

/// Staging area for an upgrade in progress: the installer, the hash list, the backup path and,
/// after the install, the report. UAC keeps the same user, so the elevated installer sees the
/// same directory the update card wrote.
const PENDING_FILE: &str = "pending.json";
const MANIFEST_FILE: &str = "files.sha256";
const REPORT_FILE: &str = "report.json";

#[derive(Clone, Default)]
pub struct Update {
    pub version: String,
    /// The installer, run with administrator rights: its name ends in `install.exe`, so the
    /// portable packer hands the extracted app `--install`, and that installs in place into
    /// whatever directory the registry's `InstallLocation` names.
    pub installer_url: String,
    /// SHA-256 of every file in the package; the post-install check compares the installed
    /// files against it and repairs only what differs.
    pub manifest_url: String,
    /// The unsigned package, used to repair files the installer did not replace.
    pub zip_url: String,
    /// The release page, for a human to look at.
    pub page_url: String,
}

static LATEST: Mutex<Option<Update>> = Mutex::new(None);

/// What the update card, the wizard and the installer all read while an upgrade is in flight.
static PROGRESS: Mutex<Option<Progress>> = Mutex::new(None);

/// Staged upgrade: written by the update card, read by the wizard's first page (which runs in
/// the freshly unpacked build) and by the post-install check.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Pending {
    pub from_version: String,
    pub to_version: String,
    pub install_dir: String,
    /// The staged build's own executable, started with `--install` for one UAC prompt.
    pub installer: String,
    /// Where that build was unpacked; the repair copies files from here.
    #[serde(default)]
    pub staged_dir: String,
    pub manifest: String,
    pub zip_url: String,
    pub backup: String,
    /// When the installer was started, for the log and for telling "staged" from "running".
    pub opened: String,
}

#[derive(Clone)]
struct Entry {
    hash: String,
    rel: String,
}

/// Outcome of the post-install check.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub version: String,
    pub from_version: String,
    #[serde(default)]
    pub install_dir: String,
    pub backup: String,
    pub total: usize,
    pub bad: Vec<String>,
    pub repaired: usize,
}

#[derive(Clone, Default, Serialize)]
struct Progress {
    step: String,
    text: String,
    done: u64,
    total: u64,
    finished: bool,
    error: String,
}

/// `reg` and `tar` are console programs; without this they flash a window.
#[cfg(windows)]
fn quiet_command(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.creation_flags(0x0800_0000);
    cmd
}

#[cfg(not(windows))]
fn quiet_command(program: &str) -> std::process::Command {
    std::process::Command::new(program)
}

pub fn cached() -> Option<Update> {
    LATEST.lock().ok().and_then(|slot| slot.clone())
}

/// Asks the fork for its newest release in the background. The answer reaches the UI as the
/// `rustdesk_update_available` event, so a slow or unreachable GitHub never delays a start.
pub fn spawn_check() {
    std::thread::spawn(|| {
        // Right after an upgrade this removes what the installer could not remove itself.
        finish_leftovers();
        match check() {
            Ok(()) => {}
            Err(err) => log::info!("update check failed: {}", err),
        }
    });
}

#[tokio::main(flavor = "current_thread")]
async fn check() -> ResultType<()> {
    let url = format!("{}/repos/{}/releases/latest", API, REPO);
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(&url, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url).unwrap_or(TlsType::Rustls);
    let client = create_http_client_async(tls_type, false);
    let response = match client
        .get(&url)
        .header("User-Agent", "rustdesk-update-check")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(response) => {
            upsert_tls_cache(tls_url, tls_type, false);
            response
        }
        Err(err) => {
            if get_cached_tls_type(tls_url).is_none() && err.is_request() {
                let client = create_http_client_async(TlsType::NativeTls, false);
                let response = client
                    .get(&url)
                    .header("User-Agent", "rustdesk-update-check")
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await?;
                upsert_tls_cache(tls_url, TlsType::NativeTls, false);
                response
            } else {
                return Err(err.into());
            }
        }
    };
    let bytes = response.bytes().await?;
    let release: serde_json::Value = serde_json::from_slice(&bytes)?;
    let tag = release
        .get("tag_name")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let version = tag.trim_start_matches('v').to_owned();
    if version.is_empty() || get_version_number(&version) <= get_version_number(crate::VERSION) {
        log::debug!(
            "no build newer than {} (newest release: {})",
            crate::VERSION,
            tag
        );
        if let Ok(mut slot) = LATEST.lock() {
            *slot = None;
        }
        return Ok(());
    }
    let update = Update {
        version,
        installer_url: asset_url(&release, |name| {
            name.starts_with("rustdesk-") && name.ends_with("-install.exe")
        }),
        manifest_url: asset_url(&release, |name| {
            name.starts_with("rustdesk-") && name.ends_with("-files.sha256")
        }),
        zip_url: asset_url(&release, |name| {
            name.starts_with("rustdesk-unsigned-") && name.ends_with(".zip")
        }),
        page_url: release
            .get("html_url")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned(),
    };
    log::info!(
        "build {} is available for upgrade to (installer: {}, {} files: {})",
        update.version,
        update.installer_url,
        update.manifest_url,
        update.zip_url
    );
    if let Ok(mut slot) = LATEST.lock() {
        *slot = Some(update.clone());
    }
    let mut event = HashMap::new();
    event.insert("name", EVENT_UPDATE_AVAILABLE);
    event.insert("version", update.version.as_str());
    event.insert("url", update.installer_url.as_str());
    event.insert("page", update.page_url.as_str());
    if let Ok(data) = serde_json::to_string(&event) {
        let _ = crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, data);
    }
    Ok(())
}

/// The download URL of the first asset whose name matches, empty when there is none.
fn asset_url(release: &serde_json::Value, matches: impl Fn(&str) -> bool) -> String {
    release
        .get("assets")
        .and_then(|value| value.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name")?.as_str()?;
                if matches(name) {
                    asset
                        .get("browser_download_url")
                        .and_then(|url| url.as_str())
                        .map(|url| url.to_owned())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

/// Called from the update card. Stages the installer and the hash list next to each other,
/// backs the current installation up, then hands the installer to Windows for one UAC prompt.
/// Everything happens in this process: no PowerShell, no .ps1 (Chinese AV products delete
/// those mid-upgrade). The UI follows along by polling [`progress_json`].
pub fn start_update() -> ResultType<()> {
    let Some(update) = cached() else {
        bail!("No newer build is known yet. Please try again in a moment.");
    };
    if update.zip_url.is_empty() {
        bail!("That release has no package to download.");
    }
    if update.manifest_url.is_empty() {
        bail!("That release has no file hash list to check the install against.");
    }
    if prepare_in_flight() {
        bail!("An upgrade is already being prepared.");
    }
    begin_progress();
    std::thread::spawn(|| {
        if let Err(err) = prepare() {
            log::info!("upgrade prepare failed: {}", err);
            finish_progress(&err.to_string(), true);
        }
    });
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn prepare() -> ResultType<()> {
    let update = cached().ok_or_else(|| anyhow!("update disappeared"))?;
    let install_dir = install_dir()?;
    let from_version = crate::VERSION.to_owned();
    let dir = pending_dir();
    // Start clean: a cancelled upgrade leaves a package and an unpacked build behind, and both
    // are tens of megabytes.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    if update.zip_url.is_empty() {
        bail!("That release has no package to download.");
    }
    if update.manifest_url.is_empty() {
        bail!("That release has no file hash list to check the install against.");
    }
    let zip = dir.join("package.zip");
    let manifest = dir.join(MANIFEST_FILE);
    let staged = dir.join("files");
    download_to(&update.zip_url, &zip, "下载新版本包").await?;
    download_to(&update.manifest_url, &manifest, "下载哈希清单").await?;
    // Unpacked with the `tar` that ships with Windows 10+ - no zip crate, no PowerShell.
    set_progress("extract", "解压新版本", 0, 0);
    std::fs::create_dir_all(&staged)?;
    let status = quiet_command("tar")
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&staged)
        .status()?;
    if !status.success() {
        bail!("tar failed to unpack the package");
    }
    // Deliberately *not* the release's self-extracting installer: that is an unsigned exe which
    // unpacks itself and installs, and Chinese AV products block exactly that. The unpacked
    // build is the same program that is already trusted, and running it with `--install` shows
    // this app's own install page.
    let staged_exe = staged.join("rustdesk.exe");
    if !staged_exe.is_file() {
        bail!("the package does not contain rustdesk.exe");
    }
    let backup = {
        let install_dir = install_dir.clone();
        let from_version = from_version.clone();
        tokio::task::spawn_blocking(move || backup_install_dir(&install_dir, &from_version))
            .await
            .map_err(|err| anyhow!("backup task failed: {}", err))??
    };
    save_pending(&Pending {
        from_version,
        to_version: update.version.clone(),
        install_dir: install_dir.to_string_lossy().to_string(),
        installer: staged_exe.to_string_lossy().to_string(),
        staged_dir: staged.to_string_lossy().to_string(),
        manifest: manifest.to_string_lossy().to_string(),
        zip_url: update.zip_url.clone(),
        backup,
        opened: String::new(),
    })?;
    log::info!(
        "upgrade to {} staged at {} (backup: {})",
        update.version,
        staged.display(),
        pending_dir().display()
    );
    set_progress("launch", "等待管理员授权（UAC）后开始安装", 0, 0);
    #[cfg(not(windows))]
    bail!("In-place upgrade is only implemented for Windows.");
    #[cfg(windows)]
    if !crate::platform::windows::run_uac(staged_exe.to_string_lossy().as_ref(), "--install")? {
        bail!("The upgrade was not started - was the administrator prompt declined?");
    }
    #[cfg(windows)]
    mark_opened()?;
    Ok(())
}

/// Runs inside the installer process: the files are in place and the UI has not been restarted
/// yet. Checks every file against the release's hash list, replaces the ones the installer could
/// not write (a running .ps1 stays locked, for instance), deletes the packer's extraction cache
/// and refreshes the registry values this fork relies on. Does nothing unless the install came
/// from our own update card.
pub fn after_install_files() {
    let Some(pending) = load_pending() else {
        return;
    };
    set_progress("verify", "校验安装文件", 0, 0);
    match verify_and_repair(&pending) {
        Ok(report) => {
            let _ = write_report(&report);
            finish_progress("", false);
            log::info!(
                "post-install check: {}/{} files match ({} repaired)",
                report.total - report.bad.len(),
                report.total,
                report.repaired
            );
        }
        Err(err) => {
            log::info!("post-install check failed: {}", err);
            finish_progress(&err.to_string(), true);
        }
    }
}

fn verify_and_repair(pending: &Pending) -> ResultType<Report> {
    let entries = read_manifest(Path::new(&pending.manifest))?;
    let total = entries.len();
    let mut bad = compare_files(&pending.install_dir, &entries)?;
    let mut repaired = 0usize;
    if !bad.is_empty() {
        // A failed repair must not cost us the cleanup and the registry sync: the report then
        // simply says which files are still off.
        match repair_files(pending, &bad) {
            Ok(done) => repaired = done,
            Err(err) => log::info!("repairing {} file(s) failed: {}", bad.len(), err),
        }
        bad = compare_files(&pending.install_dir, &entries)?;
    }
    clean_leftovers(&pending.install_dir);
    clean_staging(pending);
    // Waits for the install batch (which `run_cmds` started without waiting) to finish writing
    // its own registry values, then writes ours, so ours are the ones that stay.
    set_progress("registry", "同步注册表记录", 0, 0);
    sync_registry_after_batch(&pending.install_dir);
    let _ = std::fs::remove_file(pending_path());
    Ok(Report {
        version: pending.to_version.clone(),
        from_version: pending.from_version.clone(),
        install_dir: pending.install_dir.clone(),
        backup: pending.backup.clone(),
        total,
        bad,
        repaired,
    })
}

/// Replaces the files the installer did not write. The service is stopped around the copy
/// because the DLLs it has loaded cannot be overwritten while it runs.
fn repair_files(pending: &Pending, bad: &[String]) -> ResultType<usize> {
    let source = repair_source(pending)?;
    let app_name = crate::get_app_name();
    let service = app_name.as_str();
    let _ = quiet_command("sc").args(["stop", service]).status();
    let mut repaired = 0usize;
    for rel in bad {
        let src = entry_path(&source, rel);
        let dst = entry_path(&pending.install_dir, rel);
        if !src.is_file() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::copy(&src, &dst).is_ok() {
            repaired += 1;
        }
    }
    let _ = quiet_command("sc").args(["start", service]).status();
    Ok(repaired)
}

fn compare_files(install_dir: &str, entries: &[Entry]) -> ResultType<Vec<String>> {
    let mut bad = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let path = entry_path(install_dir, &entry.rel);
        let matches = path
            .is_file()
            .then(|| sha256_file(&path).ok())
            .flatten()
            .map(|hash| hash == entry.hash)
            .unwrap_or(false);
        if !matches {
            bad.push(entry.rel.clone());
        }
        set_progress("verify", "校验安装文件", (index + 1) as u64, entries.len() as u64);
    }
    Ok(bad)
}

/// The files to repair from: the build that was unpacked before the install, so a repair needs
/// no download. Falls back to fetching the package when that staging directory is gone.
fn repair_source(pending: &Pending) -> ResultType<String> {
    if !pending.staged_dir.is_empty()
        && Path::new(&pending.staged_dir).join("rustdesk.exe").is_file()
    {
        return Ok(pending.staged_dir.clone());
    }
    unpack_repair_source(pending)
}

/// Downloads the unsigned package and unpacks it with the `tar` that ships with Windows 10+,
/// so the repair needs neither a zip crate nor PowerShell.
fn unpack_repair_source(pending: &Pending) -> ResultType<String> {
    if pending.zip_url.is_empty() {
        bail!("no package to repair from");
    }
    let dir = pending_dir().join("repair");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let zip = pending_dir().join("package.zip");
    set_progress("repair", "下载用于修复的文件包", 0, 0);
    download_blocking(&pending.zip_url, &zip)?;
    let status = quiet_command("tar")
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&dir)
        .status()?;
    if !status.success() {
        bail!("tar failed to unpack the package");
    }
    Ok(dir.to_string_lossy().to_string())
}

fn read_manifest(path: &Path) -> ResultType<Vec<Entry>> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| anyhow!("cannot read {}: {}", path.display(), err))?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(rel)) = (parts.next(), parts.next()) else {
            continue;
        };
        entries.push(Entry {
            hash: hash.to_lowercase(),
            rel: rel.trim().replace('\\', "/"),
        });
    }
    Ok(entries)
}

fn sha256_file(path: &Path) -> ResultType<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The packer extracts a copy of itself under %LOCALAPPDATA%\<app>; it is not a second
/// installation (the service, the shortcuts and the registry all point at the real directory),
/// but it is several hundred megabytes of leftovers, so it goes away again.
///
/// The installer itself cannot remove it - that directory *is* the running installer - so the
/// removal is retried by the build that the installer starts afterwards; see
/// [`finish_leftovers`].
fn clean_leftovers(install_dir: &str) {
    remove_app_cache();
    remove_stray_broker(&std::env::temp_dir());
    if let Some(parent) = Path::new(install_dir).parent() {
        if let Some(name) = Path::new(install_dir).file_name() {
            prune_backups(parent, &name.to_string_lossy(), 3);
        }
    }
}

/// Called once per start, from the update check's thread. Only acts right after an upgrade, when
/// the report of the post-install check is still there: the installer has exited by then, so the
/// extraction cache it ran from can finally be deleted, and the staged tens of megabytes too.
pub fn finish_leftovers() {
    if !pending_dir().join(REPORT_FILE).exists() {
        return;
    }
    // The registry is synced by the installer (see `sync_registry_after_batch`): this runs in
    // the user's own process, where writing HKLM is not allowed anyway.
    for _ in 0..5 {
        if !remove_app_cache() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        remove_stray_broker(Path::new(&local));
    }
    if let Some(pending) = load_pending() {
        clean_staging(&pending);
    }
    let dir = pending_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name != REPORT_FILE {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

/// True when the cache was there and could not be removed (still in use), false once it is gone.
fn remove_app_cache() -> bool {
    let Ok(local) = std::env::var("LOCALAPPDATA") else {
        return false;
    };
    let cache = Path::new(&local).join(crate::get_app_name().to_lowercase());
    if !cache.is_dir() {
        return false;
    }
    set_progress("cleanup", "清理打包器的解压缓存", 0, 0);
    std::fs::remove_dir_all(&cache).is_err() && cache.is_dir()
}

/// The unpacked build and the package are tens of megabytes; once the check is done only the
/// report is worth keeping. The staging directory cannot always be removed from here - the
/// installer is running out of it - so the new build retries it on the next start.
fn clean_staging(pending: &Pending) {
    let _ = std::fs::remove_dir_all(pending_dir().join("repair"));
    let _ = std::fs::remove_file(pending_dir().join("package.zip"));
    if !pending.staged_dir.is_empty() {
        let _ = std::fs::remove_dir_all(&pending.staged_dir);
    }
    if !pending.installer.is_empty() {
        let _ = std::fs::remove_file(&pending.installer);
    }
}

fn remove_stray_broker(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == "runtimebroker_rustdesk.exe" {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The installer deletes and rewrites ...\Uninstall\<app>; our own updater and the home page
/// also read InstallLocation and BuildDate from it, and a repaired install must not lose them.
///
/// `run_cmds` starts the install batch without waiting for it, and that batch ends with its own
/// `reg delete` + `reg add` on the uninstall key. Writing ours first means ours is what gets
/// deleted - that is how the key ended up empty and BuildDate stayed on the previous build. So
/// this waits for the batch to have written its values, then writes ours.
fn sync_registry_after_batch(install_dir: &str) {
    let uninstall_key = "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\RustDesk";
    // `UninstallString` is the last *string* value the batch writes (only two REG_DWORDs follow),
    // so seeing it means every value we overwrite has already been written once. Probing
    // `DisplayName` instead would be too early: our values would still be clobbered by the
    // InstallLocation/BuildDate writes that come after it.
    let batch_done = || -> bool {
        let Ok(out) = quiet_command("reg")
            .args(["query", uninstall_key, "/v", "UninstallString"])
            .output()
        else {
            return false;
        };
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("rustdesk.exe")
    };
    for _ in 0..30 {
        if batch_done() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    write_registry(install_dir);
}

fn write_registry(install_dir: &str) {
    let exe = format!("{}\\rustdesk.exe", install_dir.trim_end_matches('\\'));
    let uninstall = format!("\"{}\" --uninstall", exe);
    let version = crate::VERSION.to_owned();
    let build_date = crate::BUILD_DATE.to_owned();
    for key in [
        "HKLM\\SOFTWARE\\RustDesk",
        "HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\RustDesk",
    ] {
        for (name, value) in [
            ("DisplayName", "RustDesk"),
            ("DisplayVersion", version.as_str()),
            ("Version", version.as_str()),
            ("InstallLocation", install_dir),
            ("UninstallString", uninstall.as_str()),
            ("BuildDate", build_date.as_str()),
        ] {
            let ok = quiet_command("reg")
                .args(["add", key, "/f", "/v", name, "/t", "REG_SZ", "/d", value])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !ok {
                log::info!("registry sync: {} {} failed", key, name);
            }
        }
    }
    log::info!(
        "registry synced: InstallLocation={} BuildDate={}",
        install_dir,
        build_date
    );
}

/// Backs the installation up beside itself and keeps only the newest few, so repeated in-app
/// upgrades do not fill the disk.
fn backup_install_dir(install_dir: &Path, from_version: &str) -> ResultType<String> {
    let parent = install_dir
        .parent()
        .ok_or_else(|| anyhow!("installation has no parent directory"))?;
    let name = install_dir
        .file_name()
        .ok_or_else(|| anyhow!("installation has no directory name"))?
        .to_string_lossy()
        .to_string();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let version: String = from_version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    let dest = parent.join(format!("{}.bak-{}-{}", name, stamp, version));
    set_progress("backup", "备份当前安装目录", 0, 0);
    copy_dir(install_dir, &dest)?;
    prune_backups(parent, &name, 3);
    Ok(dest.to_string_lossy().to_string())
}

fn copy_dir(src: &Path, dest: &Path) -> ResultType<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn prune_backups(parent: &Path, name: &str, keep: usize) {
    let prefix = format!("{}.bak-", name);
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .map(|n| n.to_string_lossy().starts_with(&prefix))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    if dirs.len() > keep {
        for dir in &dirs[..dirs.len() - keep] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

async fn download_to(url: &str, dest: &Path, label: &str) -> ResultType<()> {
    let proxy_conf = Config::get_socks();
    let tls_url = get_url_for_tls(url, &proxy_conf);
    let tls_type = get_cached_tls_type(tls_url).unwrap_or(TlsType::Rustls);
    let client = create_http_client_async(tls_type, false);
    let mut response = client.get(url).send().await?;
    if !response.status().is_success() {
        bail!("{} failed: HTTP {}", dest.display(), response.status());
    }
    let total = response.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(dest)?;
    let mut done = 0u64;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk)?;
        done += chunk.len() as u64;
        set_progress("download", label, done, total);
    }
    file.flush()?;
    Ok(())
}

/// The repair path runs on the installer's thread, where there is no async runtime.
fn download_blocking(url: &str, dest: &Path) -> ResultType<()> {
    let mut response = reqwest::blocking::Client::builder()
        .build()?
        .get(url)
        .send()?;
    if !response.status().is_success() {
        bail!("download failed: HTTP {}", response.status());
    }
    let total = response.content_length().unwrap_or(0);
    let mut file = std::fs::File::create(dest)?;
    let mut done = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            break;
        }
        file.write_all(&buf[..read])?;
        done += read as u64;
        set_progress("repair", "下载用于修复的文件包", done, total);
    }
    file.flush()?;
    Ok(())
}

fn entry_path(dir: &str, rel: &str) -> PathBuf {
    Path::new(dir).join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn pending_dir() -> PathBuf {
    std::env::temp_dir().join("rustdesk-upgrade")
}

fn pending_path() -> PathBuf {
    pending_dir().join(PENDING_FILE)
}

pub fn load_pending() -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_pending(pending: &Pending) -> ResultType<()> {
    std::fs::create_dir_all(pending_dir())?;
    std::fs::write(pending_path(), serde_json::to_string_pretty(pending)?)?;
    Ok(())
}

fn mark_opened() -> ResultType<()> {
    if let Some(mut pending) = load_pending() {
        pending.opened = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        save_pending(&pending)?;
    }
    Ok(())
}

fn write_report(report: &Report) -> ResultType<()> {
    std::fs::create_dir_all(pending_dir())?;
    std::fs::write(
        pending_dir().join(REPORT_FILE),
        serde_json::to_string_pretty(report)?,
    )?;
    Ok(())
}

/// What the wizard's first page shows: what is about to be installed, and whether the previous
/// installation was backed up. Empty when nothing is staged for *this* build - a plain
/// first-time install, or an installer started long after the upgrade it belonged to.
pub fn pending_info_json() -> String {
    match load_pending() {
        Some(pending) if pending.to_version == crate::VERSION => {
            serde_json::to_string(&pending).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// The result of the post-install check, for the build that is running now.
pub fn report_json() -> String {
    std::fs::read_to_string(pending_dir().join(REPORT_FILE)).unwrap_or_default()
}

/// Called once the running build has shown the report, so it is not shown on every start.
pub fn clear_report() {
    let _ = std::fs::remove_file(pending_dir().join(REPORT_FILE));
}

pub fn progress_json() -> String {
    let progress = PROGRESS
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
    serde_json::to_string(&progress).unwrap_or_default()
}

fn begin_progress() {
    if let Ok(mut slot) = PROGRESS.lock() {
        *slot = Some(Progress {
            step: "download".to_owned(),
            text: "准备升级".to_owned(),
            ..Default::default()
        });
    }
}

fn set_progress(step: &str, text: &str, done: u64, total: u64) {
    // Deliberately no event per tick: the download and the per-file check call this hundreds of
    // times, and the wizard and the card both poll `progress_json` anyway.
    if let Ok(mut slot) = PROGRESS.lock() {
        let finished = slot.as_ref().map(|p| p.finished).unwrap_or(false);
        *slot = Some(Progress {
            step: step.to_owned(),
            text: text.to_owned(),
            done,
            total,
            finished,
            error: String::new(),
        });
    }
}

fn finish_progress(error: &str, failed: bool) {
    if let Ok(mut slot) = PROGRESS.lock() {
        let mut progress = slot.clone().unwrap_or_default();
        progress.finished = true;
        progress.error = error.to_owned();
        progress.step = if failed { "failed".to_owned() } else { "done".to_owned() };
        *slot = Some(progress);
    }
    // One event at the end, so a UI that is only listening (and not polling) still learns.
    let data = progress_json();
    if !data.is_empty() {
        let _ = crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, data);
    }
}

/// True while a prepare step is running in this process, so a second card click does not start
/// a second download and a second installer.
fn prepare_in_flight() -> bool {
    PROGRESS
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .map(|progress| !progress.finished)
        .unwrap_or(false)
}

/// The directory this installation lives in (beside the running executable).
fn install_dir() -> ResultType<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.to_path_buf())
        .ok_or_else(|| anyhow!("Cannot tell which directory the installation is in"))
}

