//! Self-update.
//!
//! Shape of it: a release publishes `latest.json` beside its binaries, the app fetches that one
//! small file, compares versions, downloads the installer, checks its SHA-256 against the
//! manifest, then hands over to the installer and quits so the file it is about to replace is not
//! locked.
//!
//! Why a manifest and not the GitHub API: `releases/latest/download/<asset>` is a plain redirect
//! that needs no token and is not rate limited per IP the way `api.github.com` is (60 requests an
//! hour, shared by everyone behind the same address).
//!
//! What leaves the machine: an HTTPS GET with a version-only user agent. No identifiers, no
//! configuration, no host name - GitHub sees the request and the IP address it came from, which is
//! unavoidable for any download, and nothing else.
//!
//! Integrity rests on TLS to github.com plus the manifest's digest. The digest catches a truncated
//! or corrupted download and a swapped asset; it cannot substitute for a code signature, because
//! whoever could replace the asset could replace the manifest as well. Signing the installer is
//! the missing piece and needs a certificate (see README).

use std::path::Path;
use std::sync::Mutex;

use serde::Deserialize;

/// Kept in one place so a fork only has to change this line.
pub const MANIFEST_URL: &str =
    "https://github.com/maketryuk/remote-Input-bridge/releases/latest/download/latest.json";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Re-checked once a day, which is often enough for a tool nobody wants to think about and rare
/// enough to be invisible.
const AUTO_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// Long enough that the connection is up after a resume, short enough that the first check does
/// not race the bridge's own reconnect.
const FIRST_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(20);

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_INSTALLER_BYTES: u64 = 128 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes_url: String,
    #[serde(default)]
    pub windows: Option<Artifact>,
    /// Read by the Mac half, which consumes the same manifest; parsed here so that a manifest
    /// carrying both platforms is not rejected as unexpected.
    #[serde(default)]
    #[allow(dead_code)]
    pub macos: Option<Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// Compares dotted numeric versions. Anything unparseable counts as zero, so a malformed manifest
/// can never look newer than the running build.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(text: &str) -> Vec<u64> {
        text.trim()
            .trim_start_matches('v')
            // A pre-release suffix ("0.3.0-rc1") is not ordered here: the numeric prefix decides,
            // and the suffix is ignored rather than guessed at.
            .split(|c: char| c == '.' || c == '-' || c == '+')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    }
    let (left, right) = (parts(candidate), parts(current));
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Nothing has been checked yet in this run.
    Idle,
    Checking,
    UpToDate,
    Available(String),
    /// Percentage of the installer downloaded, or `None` when its size is unknown.
    Downloading(Option<u8>),
    Installing,
    Failed(String),
}

struct State {
    stage: Stage,
    pending: Option<(String, Artifact)>,
    busy: bool,
}

static STATE: Mutex<State> = Mutex::new(State { stage: Stage::Idle, pending: None, busy: false });

fn set_stage(stage: Stage) {
    STATE.lock().unwrap().stage = stage;
    crate::ui::refresh();
}

pub fn stage() -> Stage {
    STATE.lock().unwrap().stage.clone()
}

/// One line for the settings window and the tray tooltip.
pub fn summary() -> String {
    match stage() {
        Stage::Idle => format!("Version {VERSION}"),
        Stage::Checking => format!("Version {VERSION} - checking for updates..."),
        Stage::UpToDate => format!("Version {VERSION} - up to date"),
        Stage::Available(version) => format!("Version {version} is available (you have {VERSION})"),
        Stage::Downloading(Some(percent)) => format!("Downloading the update... {percent}%"),
        Stage::Downloading(None) => "Downloading the update...".into(),
        Stage::Installing => "Installing - the app will restart".into(),
        Stage::Failed(reason) => format!("Update check failed: {reason}"),
    }
}

/// True when there is a downloaded-or-downloadable newer release waiting for a decision.
pub fn update_ready() -> bool {
    matches!(stage(), Stage::Available(_))
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Checks in the background. `manual` only affects reporting: an automatic check that fails stays
/// quiet in the log instead of putting a scary line in the window.
pub fn check(manual: bool) {
    if !claim() {
        return;
    }
    std::thread::Builder::new()
        .name("rib-update".into())
        .spawn(move || {
            set_stage(Stage::Checking);
            let outcome = check_now();
            release();
            match outcome {
                Ok(Some(version)) => {
                    crate::log::info(&format!("update available: {version}"));
                    set_stage(Stage::Available(version));
                }
                Ok(None) => {
                    crate::log::info(&format!("no update: {VERSION} is the latest release"));
                    set_stage(Stage::UpToDate);
                }
                Err(e) => {
                    if manual {
                        crate::log::warn(&format!("update check failed: {e}"));
                        set_stage(Stage::Failed(e));
                    } else {
                        crate::log::info(&format!("background update check failed: {e}"));
                        set_stage(Stage::Idle);
                    }
                }
            }
        })
        .ok();
}

/// Downloads the pending update, verifies it and hands over to the installer. The app quits from
/// inside the installer hand-off, so this never returns to a running UI on success.
pub fn install() {
    let Some((version, artifact)) = STATE.lock().unwrap().pending.clone() else {
        check(true);
        return;
    };
    if !claim() {
        return;
    }
    std::thread::Builder::new()
        .name("rib-update-install".into())
        .spawn(move || {
            set_stage(Stage::Downloading(None));
            match download_and_launch(&version, &artifact) {
                Ok(()) => {
                    set_stage(Stage::Installing);
                    // The installer is running; get out of the way so it can replace the binary.
                    crate::log::info("handing over to the installer");
                    crate::ui::quit();
                }
                Err(e) => {
                    crate::log::warn(&format!("update failed: {e}"));
                    release();
                    set_stage(Stage::Failed(e));
                }
            }
        })
        .ok();
}

/// Daily check in the background, honouring the setting each time round so switching it off takes
/// effect without a restart.
pub fn spawn_auto_check() {
    std::thread::Builder::new()
        .name("rib-update-auto".into())
        .spawn(|| {
            std::thread::sleep(FIRST_CHECK_DELAY);
            loop {
                if crate::state::state().config().auto_check_updates {
                    check(false);
                }
                std::thread::sleep(AUTO_CHECK_INTERVAL);
            }
        })
        .ok();
}

fn claim() -> bool {
    let mut state = STATE.lock().unwrap();
    if state.busy {
        return false;
    }
    state.busy = true;
    true
}

fn release() {
    STATE.lock().unwrap().busy = false;
}

// ---------------------------------------------------------------------------
// The work
// ---------------------------------------------------------------------------

fn check_now() -> Result<Option<String>, String> {
    let body = http::get(MANIFEST_URL, MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_slice(&body)
        .map_err(|e| format!("the release manifest is not valid JSON: {e}"))?;
    if !is_newer(&manifest.version, VERSION) {
        STATE.lock().unwrap().pending = None;
        return Ok(None);
    }
    let artifact = manifest
        .windows
        .clone()
        .ok_or_else(|| format!("release {} has no Windows build", manifest.version))?;
    if !manifest.notes_url.is_empty() {
        crate::log::info(&format!("release notes: {}", manifest.notes_url));
    }
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the manifest carries no usable SHA-256 for the Windows build".into());
    }
    if !artifact.url.starts_with("https://") {
        return Err("the manifest points at a non-HTTPS download".into());
    }
    STATE.lock().unwrap().pending = Some((manifest.version.clone(), artifact));
    Ok(Some(manifest.version))
}

fn download_and_launch(version: &str, artifact: &Artifact) -> Result<(), String> {
    let target = std::env::temp_dir().join(format!("RemoteInputBridge-Setup-{version}.exe"));
    // A leftover from an interrupted attempt is never trusted: re-download rather than run a file
    // of unknown provenance sitting in a world-writable directory.
    let _ = std::fs::remove_file(&target);
    let digest = http::download(&artifact.url, &target, artifact.size, MAX_INSTALLER_BYTES)?;
    if !digest.eq_ignore_ascii_case(&artifact.sha256) {
        let _ = std::fs::remove_file(&target);
        return Err(format!(
            "the download does not match the manifest digest (got {digest}); it was discarded"
        ));
    }
    crate::log::info(&format!("verified {} ({digest})", target.display()));
    launch_installer(&target)
}

/// Starts the installer detached and returns. `/SILENT` still shows a progress window, which is
/// the right amount of feedback for something the user asked for; `/CLOSEAPPLICATIONS` covers the
/// case where this process has not finished exiting yet.
#[cfg(windows)]
fn launch_installer(installer: &Path) -> Result<(), String> {
    let log = std::env::temp_dir().join("RemoteInputBridge-Setup.log");
    std::process::Command::new(installer)
        .arg("/SILENT")
        .arg("/SUPPRESSMSGBOXES")
        .arg("/CLOSEAPPLICATIONS")
        .arg("/NOCANCEL")
        // Read by the installer's [Code] section, which relaunches the app afterwards. A plain
        // `/SILENT` install deliberately runs nothing, so without this the update would leave the
        // bridge stopped.
        .arg("/RIBRESTART=1")
        .arg(format!("/LOG={}", log.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the installer: {e}"))
}

#[cfg(not(windows))]
fn launch_installer(_installer: &Path) -> Result<(), String> {
    Err("the Windows installer cannot be run here".into())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// A small WinHTTP wrapper. WinHTTP rather than a Rust HTTP client because it is already in the
/// operating system: no TLS stack to vendor, no root store to ship, and the system proxy
/// configuration is honoured for free.
#[cfg(windows)]
mod http {
    use std::io::Write;
    use std::path::Path;

    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Networking::WinHttp::*;

    const CHUNK: usize = 64 * 1024;
    /// Milliseconds. A stalled update must not keep a thread forever.
    const TIMEOUT: i32 = 30_000;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn fail(operation: &str) -> String {
        format!("{operation}: {}", std::io::Error::last_os_error())
    }

    struct Handle(*mut core::ffi::c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    /// Field order is the drop order, and WinHTTP wants the request closed before the connection
    /// and the connection before the session.
    struct Response {
        request: Handle,
        _connection: Handle,
        _session: Handle,
        declared_length: Option<u64>,
    }

    struct Target {
        host: String,
        path: String,
        port: u16,
        secure: bool,
    }

    fn split(url: &str) -> Result<Target, String> {
        let (scheme, rest) = url.split_once("://").ok_or("the URL has no scheme")?;
        let secure = match scheme {
            "https" => true,
            "http" => false,
            other => return Err(format!("unsupported URL scheme: {other}")),
        };
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, "/".to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse().map_err(|_| format!("bad port in {url}"))?,
            ),
            None => (authority.to_string(), if secure { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err("the URL has no host".into());
        }
        Ok(Target { host, path, port, secure })
    }

    fn open(url: &str) -> Result<Response, String> {
        let target = split(url)?;
        // Version only: enough for a server-side error report to be actionable, and nothing that
        // identifies the machine.
        let agent = wide(&format!("RemoteInputBridge/{} (Windows)", super::VERSION));
        let session = Handle(unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        });
        if session.0.is_null() {
            return Err(fail("WinHttpOpen"));
        }
        unsafe { WinHttpSetTimeouts(session.0, TIMEOUT, TIMEOUT, TIMEOUT, TIMEOUT) };
        // Windows 10 enables TLS 1.2 by default, but a machine whose defaults were tightened or
        // loosened by policy is not worth guessing about: ask for the modern protocols, and fall
        // back to 1.2 alone on the builds that reject the 1.3 bit.
        for protocols in [0x0800u32 | 0x2000, 0x0800] {
            let ok = unsafe {
                WinHttpSetOption(
                    session.0,
                    WINHTTP_OPTION_SECURE_PROTOCOLS,
                    (&protocols as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                )
            };
            if ok != 0 {
                break;
            }
        }

        let connection = Handle(unsafe {
            WinHttpConnect(session.0, wide(&target.host).as_ptr(), target.port, 0)
        });
        if connection.0.is_null() {
            return Err(fail("WinHttpConnect"));
        }
        // Without an explicit accept list WinHTTP sends no Accept header at all, which most
        // servers tolerate and some object to.
        let any = wide("*/*");
        let accept_types = [any.as_ptr(), std::ptr::null()];
        let request = Handle(unsafe {
            WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(&target.path).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                accept_types.as_ptr(),
                if target.secure { WINHTTP_FLAG_SECURE } else { 0 },
            )
        });
        if request.0.is_null() {
            return Err(fail("WinHttpOpenRequest"));
        }
        // GitHub redirects a release asset to a storage host; WinHTTP follows that itself and
        // refuses any redirect that would downgrade to plain HTTP.
        let sent = unsafe {
            WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0)
        };
        if sent == 0 {
            return Err(fail("WinHttpSendRequest"));
        }
        if unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) } == 0 {
            return Err(fail("WinHttpReceiveResponse"));
        }
        let status = query_number(&request, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if status != 200 {
            return Err(format!("the server answered HTTP {status}"));
        }
        let declared_length =
            query_number(&request, WINHTTP_QUERY_CONTENT_LENGTH).map(|value| value as u64);
        Ok(Response {
            request,
            _connection: connection,
            _session: session,
            declared_length,
        })
    }

    fn query_number(request: &Handle, level: u32) -> Option<u32> {
        let mut value: u32 = 0;
        let mut length = std::mem::size_of::<u32>() as u32;
        let ok = unsafe {
            WinHttpQueryHeaders(
                request.0,
                level | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                (&mut value as *mut u32).cast(),
                &mut length,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            None
        } else {
            Some(value)
        }
    }

    fn read(response: &Response, buffer: &mut [u8]) -> Result<usize, String> {
        let mut read = 0u32;
        let ok = unsafe {
            WinHttpReadData(
                response.request.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(fail("WinHttpReadData"));
        }
        Ok(read as usize)
    }

    /// Fetches a small resource into memory.
    pub fn get(url: &str, limit: usize) -> Result<Vec<u8>, String> {
        let response = open(url)?;
        let mut body = Vec::new();
        let mut chunk = vec![0u8; CHUNK.min(limit + 1)];
        loop {
            let read = read(&response, &mut chunk)?;
            if read == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&chunk[..read]);
            if body.len() > limit {
                return Err(format!("the response is larger than the {limit} byte limit"));
            }
        }
    }

    /// Streams a download to disk and returns its lowercase hex SHA-256. Hashing as it arrives
    /// means the file is never read back, so nothing can change between check and use.
    pub fn download(
        url: &str,
        target: &Path,
        expected_size: u64,
        limit: u64,
    ) -> Result<String, String> {
        let response = open(url)?;
        let total = match (response.declared_length, expected_size) {
            (Some(length), _) => Some(length),
            (None, 0) => None,
            (None, size) => Some(size),
        };
        if total.is_some_and(|bytes| bytes > limit) {
            return Err(format!("the download claims {} bytes, which is more than the {limit} byte limit", total.unwrap()));
        }
        let mut file = std::fs::File::create(target)
            .map_err(|e| format!("could not create {}: {e}", target.display()))?;
        let mut hasher = Sha256::new();
        let mut chunk = vec![0u8; CHUNK];
        let mut written = 0u64;
        let mut last_reported = u8::MAX;
        loop {
            let read = read(&response, &mut chunk)?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > limit {
                let _ = std::fs::remove_file(target);
                return Err(format!("the download exceeded the {limit} byte limit"));
            }
            hasher.update(&chunk[..read]);
            file.write_all(&chunk[..read])
                .map_err(|e| format!("could not write {}: {e}", target.display()))?;
            if let Some(total) = total.filter(|total| *total > 0) {
                let percent = ((written * 100) / total).min(100) as u8;
                if percent != last_reported {
                    last_reported = percent;
                    super::set_stage(super::Stage::Downloading(Some(percent)));
                }
            }
        }
        file.flush().map_err(|e| format!("could not flush {}: {e}", target.display()))?;
        if let Some(total) = total.filter(|total| *total > 0) {
            if written != total {
                let _ = std::fs::remove_file(target);
                return Err(format!("the download stopped after {written} of {total} bytes"));
            }
        }
        Ok(crate::crypto::hex_encode(&hasher.finalize()))
    }
}

#[cfg(not(windows))]
mod http {
    use std::path::Path;

    pub fn get(_url: &str, _limit: usize) -> Result<Vec<u8>, String> {
        Err("updates are only implemented for the Windows sender".into())
    }

    pub fn download(
        _url: &str,
        _target: &Path,
        _expected_size: u64,
        _limit: u64,
    ) -> Result<String, String> {
        Err("updates are only implemented for the Windows sender".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_recognised() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("v0.2.1", "0.2.0"), "a tag-style v prefix is tolerated");
        assert!(is_newer("0.2.0.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("nonsense", "0.2.0"), "an unparseable version is never newer");
        assert!(!is_newer("", "0.0.1"));
    }

    #[test]
    fn manifest_parses_with_only_the_fields_we_need() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"version":"0.3.0","notes_url":"https://example.invalid/notes",
                "windows":{"url":"https://example.invalid/setup.exe","sha256":"ab","size":12},
                "extra_field_from_a_future_release":true}"#,
        )
        .expect("manifest should parse");
        assert_eq!(manifest.version, "0.3.0");
        assert_eq!(manifest.windows.unwrap().size, 12);
        assert!(manifest.macos.is_none(), "a manifest without a Mac build is still valid");
    }

    #[test]
    fn summary_never_hides_the_running_version_when_idle() {
        assert!(summary().contains(VERSION));
    }
}
