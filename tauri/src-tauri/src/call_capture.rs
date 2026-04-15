use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const NATIVE_CALL_BACKEND: &str = "screencapturekit-helper";
const SCREEN_RECORDING_SETTINGS_DETAIL: &str = "Minutes needs Screen & System Audio Recording access to capture call audio. Turn it on in System Settings > Privacy & Security > Screen & System Audio Recording. macOS may not show a permission prompt for this service, so enabling Minutes there manually is the reliable path. If Minutes already looks enabled there, use Fix Permissions to reset the stale grant.";
const SCREEN_RECORDING_START_FAILURE_SNIPPET: &str =
    "Screen & System Audio Recording access is required to capture call audio";
#[cfg(target_os = "macos")]
const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallCaptureAvailability {
    Available { backend: String },
    PermissionRequired { detail: String, can_start: bool },
    Unavailable { detail: String },
    Unsupported { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallCaptureCapability {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub detail: String,
    pub can_start: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallSourceHealth {
    pub backend: String,
    pub mic_live: bool,
    pub call_audio_live: bool,
    pub mic_level: u32,
    pub call_audio_level: u32,
    pub last_update: String,
}

/// Paths to per-source audio stems written by the native call helper.
#[derive(Debug, Clone)]
pub struct StemPaths {
    pub voice: PathBuf,
    pub system: PathBuf,
}

pub struct NativeCallCaptureSession {
    child: Child,
    output_path: PathBuf,
    health: Arc<Mutex<CallSourceHealth>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    #[allow(dead_code)] // used once pipeline stem attribution is wired up
    stem_paths: Arc<Mutex<Option<StemPaths>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum MicrophonePermission {
    Authorized,
    Denied,
    Restricted,
    NotDetermined,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionProbe {
    screen_recording: bool,
    microphone: MicrophonePermission,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
struct CachedAvailability {
    checked_at: Instant,
    value: CallCaptureAvailability,
}

#[cfg(target_os = "macos")]
static AVAILABILITY_CACHE: OnceLock<Mutex<Option<CachedAvailability>>> = OnceLock::new();
static SCREEN_RECORDING_SETTINGS_REQUIRED: AtomicBool = AtomicBool::new(false);

fn parse_macos_major_version(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn macos_major_version() -> Option<u32> {
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_macos_major_version(&String::from_utf8_lossy(&output.stdout))
}

impl CallCaptureAvailability {
    pub fn capability(&self) -> CallCaptureCapability {
        let status = match self {
            CallCaptureAvailability::Available { .. } => "available",
            CallCaptureAvailability::PermissionRequired { .. } => "permission-required",
            CallCaptureAvailability::Unavailable { .. } => "unavailable",
            CallCaptureAvailability::Unsupported { .. } => "unsupported",
        };

        let backend = match self {
            CallCaptureAvailability::Available { backend } => Some(backend.clone()),
            _ => None,
        };

        let can_start = match self {
            CallCaptureAvailability::Available { .. } => true,
            CallCaptureAvailability::PermissionRequired { can_start, .. } => *can_start,
            CallCaptureAvailability::Unavailable { .. }
            | CallCaptureAvailability::Unsupported { .. } => false,
        };

        CallCaptureCapability {
            status: status.into(),
            backend,
            detail: self.detail(),
            can_start,
        }
    }

    pub fn can_attempt_capture(&self) -> bool {
        matches!(self, CallCaptureAvailability::Available { .. })
            || matches!(
                self,
                CallCaptureAvailability::PermissionRequired {
                    can_start: true,
                    ..
                }
            )
    }

    pub fn detail(&self) -> String {
        match self {
            CallCaptureAvailability::Available { .. } => {
                "Capture both your microphone and call audio with native ScreenCaptureKit recording on macOS 15+.".into()
            }
            CallCaptureAvailability::PermissionRequired { detail, .. }
            | CallCaptureAvailability::Unavailable { detail }
            | CallCaptureAvailability::Unsupported { detail } => detail.clone(),
        }
    }
}

impl NativeCallCaptureSession {
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Return per-source stem paths if the helper reported them and the files exist.
    #[allow(dead_code)] // used once pipeline stem attribution is wired up
    pub fn stem_paths(&self) -> Option<StemPaths> {
        self.stem_paths
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|stems| stems.voice.exists() && stems.system.exists())
    }

    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        self.child.try_wait().map_err(|error| error.to_string())
    }

    pub fn source_health(&self) -> CallSourceHealth {
        self.health
            .lock()
            .map(|health| health.clone())
            .unwrap_or_else(|_| CallSourceHealth {
                backend: NATIVE_CALL_BACKEND.into(),
                mic_live: false,
                call_audio_live: false,
                mic_level: 0,
                call_audio_level: 0,
                last_update: chrono::Local::now().to_rfc3339(),
            })
    }

    pub fn child_failure_detail(&self, base: &str) -> String {
        helper_failure_message(base, &self.stderr_lines)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        #[cfg(not(target_os = "macos"))]
        {
            return Err("native call capture is unsupported on this platform".into());
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
                if status.success() {
                    return Ok(());
                }
                return Err(format!("native call helper exited with status {}", status));
            }

            let pid = self.child.id();
            let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                let _ = self.child.kill();
                return Err(format!(
                    "failed to stop native call helper (PID {}): {}",
                    pid, error
                ));
            }

            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(15) {
                if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("native call helper exited with status {}", status));
                }
                std::thread::sleep(Duration::from_millis(200));
            }

            let _ = self.child.kill();
            Err("native call helper did not stop within 15 seconds".into())
        }
    }
}

pub fn availability() -> CallCaptureAvailability {
    #[cfg(target_os = "macos")]
    {
        let cache = AVAILABILITY_CACHE.get_or_init(|| Mutex::new(None));
        if let Ok(guard) = cache.lock() {
            if let Some(cached) = guard.as_ref() {
                if cached.checked_at.elapsed() < Duration::from_secs(5) {
                    return cached.value.clone();
                }
            }
        }

        let fresh = availability_fresh();
        if let Ok(mut guard) = cache.lock() {
            *guard = Some(CachedAvailability {
                checked_at: Instant::now(),
                value: fresh.clone(),
            });
        }
        return fresh;
    }

    #[cfg(not(target_os = "macos"))]
    {
        availability_fresh()
    }
}

#[cfg(target_os = "macos")]
fn invalidate_availability_cache() {
    if let Some(cache) = AVAILABILITY_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

pub fn availability_fresh() -> CallCaptureAvailability {
    #[cfg(not(target_os = "macos"))]
    {
        return CallCaptureAvailability::Unsupported {
            detail: "Native call capture is currently implemented on macOS only.".into(),
        };
    }

    #[cfg(target_os = "macos")]
    {
        detect_availability()
    }
}

#[cfg(target_os = "macos")]
pub fn start_native_call_capture(
    preferred_microphone_name: Option<&str>,
) -> Result<NativeCallCaptureSession, String> {
    if let Some(major) = macos_major_version() {
        if major < 15 {
            return Err(format!(
                "native call capture requires macOS 15 or newer (found macOS {})",
                major
            ));
        }
    }

    let helper = find_native_call_helper_binary()
        .ok_or_else(|| "native call helper binary is unavailable".to_string())?;
    let output_path = native_call_output_path()?;
    let health = Arc::new(Mutex::new(CallSourceHealth {
        backend: NATIVE_CALL_BACKEND.into(),
        mic_live: false,
        call_audio_live: false,
        mic_level: 0,
        call_audio_level: 0,
        last_update: chrono::Local::now().to_rfc3339(),
    }));
    let mut command = Command::new(helper);
    command
        .arg(&output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = preferred_microphone_name.map(str::trim).filter(|name| !name.is_empty()) {
        command.arg("--microphone-name").arg(name);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start native call helper: {}", error))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "native call helper did not expose stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "native call helper did not expose stderr".to_string())?;
    let (tx, rx) = mpsc::channel();
    let stem_paths: Arc<Mutex<Option<StemPaths>>> = Arc::new(Mutex::new(None));
    let health_for_thread = Arc::clone(&health);
    let stems_for_thread = Arc::clone(&stem_paths);
    let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_for_thread = Arc::clone(&stderr_lines);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Ok(mut guard) = stderr_for_thread.lock() {
                guard.push(trimmed.to_string());
                if guard.len() > 8 {
                    let drain = guard.len().saturating_sub(8);
                    guard.drain(0..drain);
                }
            }
        }
    });

    let stderr_for_stdout = Arc::clone(&stderr_lines);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut ready_sent = false;

        loop {
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(error) => {
                    if !ready_sent {
                        let _ = tx.send(Err(helper_failure_message(
                            &format!("failed to read native call helper output: {}", error),
                            &stderr_for_stdout,
                        )));
                    }
                    break;
                }
            };

            if read == 0 {
                if !ready_sent {
                    let _ = tx.send(Err(helper_failure_message(
                        "native call helper exited before signaling readiness",
                        &stderr_for_stdout,
                    )));
                }
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if !ready_sent {
                ready_sent = true;
                let _ = tx.send(Ok(trimmed.to_string()));
                continue;
            }

            if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                match value.get("event").and_then(|v| v.as_str()) {
                    Some("health") => {
                        if let Ok(mut current) = health_for_thread.lock() {
                            current.mic_live = value
                                .get("mic_live")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            current.call_audio_live = value
                                .get("call_audio_live")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            current.mic_level = value
                                .get("mic_level")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as u32)
                                .unwrap_or(0);
                            current.call_audio_level = value
                                .get("call_audio_level")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as u32)
                                .unwrap_or(0);
                            current.last_update = chrono::Local::now().to_rfc3339();
                        }
                    }
                    Some("stems") => {
                        let voice = value
                            .get("voice_stem")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let system = value
                            .get("system_stem")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !voice.is_empty() && !system.is_empty() {
                            if let Ok(mut guard) = stems_for_thread.lock() {
                                *guard = Some(StemPaths {
                                    voice: PathBuf::from(voice),
                                    system: PathBuf::from(system),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(line)) if line == "ready" => Ok(NativeCallCaptureSession {
            child,
            output_path,
            health,
            stderr_lines,
            stem_paths,
        }),
        Ok(Ok(line)) => {
            let _ = child.kill();
            Err(format!(
                "native call helper returned unexpected readiness output: {}",
                line
            ))
        }
        Ok(Err(error)) => {
            let _ = child.kill();
            note_start_failure(&error);
            Err(error)
        }
        Err(_) => {
            let _ = child.kill();
            let error = helper_failure_message(
                "native call helper timed out waiting for ScreenCaptureKit readiness",
                &stderr_lines,
            );
            note_start_failure(&error);
            Err(error)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn start_native_call_capture(
    _preferred_microphone_name: Option<&str>,
) -> Result<NativeCallCaptureSession, String> {
    Err("native call capture is unsupported on this platform".into())
}

#[cfg(target_os = "macos")]
fn native_call_output_path() -> Result<PathBuf, String> {
    let dir = minutes_core::Config::minutes_dir().join("native-captures");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir.join(format!(
        "{}-call.mov",
        chrono::Local::now().format("%Y-%m-%d-%H%M%S")
    )))
}

#[cfg(target_os = "macos")]
fn detect_availability() -> CallCaptureAvailability {
    match macos_major_version() {
        Some(major) if major < 15 => {
            return CallCaptureAvailability::Unsupported {
                detail: format!(
                    "Native call capture requires macOS 15 or newer. This Mac reports macOS {}.",
                    major
                ),
            };
        }
        None => {
            return CallCaptureAvailability::Unavailable {
                detail: "Could not determine the macOS version for native call capture.".into(),
            };
        }
        _ => {}
    }

    let helper = match find_native_call_helper_binary() {
        Some(helper) => helper,
        None => {
            return CallCaptureAvailability::Unavailable {
                detail: "Bundled native call helper is missing from the app bundle.".into(),
            };
        }
    };

    match probe_permissions(&helper) {
        Ok(probe) => availability_from_probe(probe),
        Err(error) => CallCaptureAvailability::PermissionRequired {
            detail: format!(
                "Minutes could not verify Screen & System Audio Recording access cleanly ({}). Record Call will still try native capture and may prompt again or require re-enabling Minutes in System Settings > Privacy & Security > Screen & System Audio Recording.",
                error
            ),
            can_start: true,
        },
    }
}

#[cfg(target_os = "macos")]
fn probe_permissions(helper: &Path) -> Result<PermissionProbe, String> {
    let output = Command::new(helper)
        .arg("--probe")
        .output()
        .map_err(|error| format!("could not run helper: {}", error))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(format!("helper exited with status {}", output.status));
        }
        return Err(stderr);
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("could not parse helper probe output: {}", error))
}

fn availability_from_probe(probe: PermissionProbe) -> CallCaptureAvailability {
    if probe.screen_recording {
        SCREEN_RECORDING_SETTINGS_REQUIRED.store(false, Ordering::Relaxed);
    }
    let screen_recording_settings_required =
        !probe.screen_recording && SCREEN_RECORDING_SETTINGS_REQUIRED.load(Ordering::Relaxed);
    availability_from_probe_with_screen_recording_state(probe, screen_recording_settings_required)
}

fn availability_from_probe_with_screen_recording_state(
    probe: PermissionProbe,
    screen_recording_settings_required: bool,
) -> CallCaptureAvailability {
    if probe.screen_recording && probe.microphone == MicrophonePermission::Authorized {
        return CallCaptureAvailability::Available {
            backend: NATIVE_CALL_BACKEND.into(),
        };
    }

    let can_start = !screen_recording_settings_required
        && !matches!(
            probe.microphone,
            MicrophonePermission::Denied | MicrophonePermission::Restricted
        );

    let detail = if screen_recording_settings_required {
        match probe.microphone {
            MicrophonePermission::Denied => "Minutes needs Screen & System Audio Recording and Microphone access to capture both sides of a call. Turn them on in System Settings > Privacy & Security, then reopen Minutes if macOS asks.".into(),
            MicrophonePermission::Restricted => "Minutes needs Screen & System Audio Recording access to capture call audio, and Microphone access is restricted by macOS on this Mac.".into(),
            MicrophonePermission::Authorized | MicrophonePermission::NotDetermined => {
                SCREEN_RECORDING_SETTINGS_DETAIL.into()
            }
        }
    } else {
        match (probe.screen_recording, probe.microphone) {
        (false, MicrophonePermission::Authorized) => "Minutes needs Screen & System Audio Recording access to capture call audio. Turn Minutes on in System Settings > Privacy & Security > Screen & System Audio Recording, then try Record Call again.".into(),
        (false, MicrophonePermission::NotDetermined) => "Minutes needs Screen & System Audio Recording and Microphone access to capture both sides of a call. Turn Minutes on in System Settings > Privacy & Security > Screen & System Audio Recording, then start call capture again so macOS can ask for Microphone access if needed.".into(),
        (false, MicrophonePermission::Denied) => "Minutes needs Screen & System Audio Recording and Microphone access to capture both sides of a call. Turn on Minutes in System Settings > Privacy & Security > Screen & System Audio Recording and Microphone, then try Record Call again.".into(),
        (false, MicrophonePermission::Restricted) => "Minutes needs Screen & System Audio Recording and Microphone access to capture both sides of a call, but Microphone access is restricted by macOS on this Mac.".into(),
        (true, MicrophonePermission::NotDetermined) => "Minutes will ask for Microphone access when call capture starts.".into(),
        (true, MicrophonePermission::Denied) => "Minutes needs Microphone access to capture your side of the call. Turn it on in System Settings > Privacy & Security > Microphone.".into(),
        (true, MicrophonePermission::Restricted) => "Minutes needs Microphone access to capture your side of the call, but Microphone access is restricted by macOS on this Mac.".into(),
        (true, MicrophonePermission::Authorized) => unreachable!("handled above"),
        }
    };

    CallCaptureAvailability::PermissionRequired { detail, can_start }
}

#[cfg(target_os = "macos")]
fn note_start_failure(error: &str) {
    if error.contains(SCREEN_RECORDING_START_FAILURE_SNIPPET) {
        SCREEN_RECORDING_SETTINGS_REQUIRED.store(true, Ordering::Relaxed);
        invalidate_availability_cache();
    }
}

fn helper_failure_message(base: &str, stderr_lines: &Arc<Mutex<Vec<String>>>) -> String {
    let stderr = stderr_lines
        .lock()
        .ok()
        .map(|guard| guard.join(" "))
        .unwrap_or_default();
    if stderr.trim().is_empty() {
        base.into()
    } else {
        format!("{}: {}", base, stderr)
    }
}

#[cfg(target_os = "macos")]
fn find_native_call_helper_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let beside_exe = exe
            .parent()
            .unwrap_or(exe.as_ref())
            .join("system_audio_record");
        if beside_exe.exists() {
            return Some(beside_exe);
        }
    }

    let dev_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/system_audio_record");
    if dev_path.exists() {
        return Some(dev_path);
    }

    None
}

#[cfg(target_os = "macos")]
pub fn repair_permissions(bundle_identifier: &str) -> Result<(), String> {
    let screen_status = Command::new("tccutil")
        .args(["reset", "ScreenCapture", bundle_identifier])
        .status()
        .map_err(|error| format!("could not reset Screen & System Audio Recording access: {}", error))?;
    if !screen_status.success() {
        return Err(format!(
            "tccutil reset ScreenCapture {} exited with status {}",
            bundle_identifier, screen_status
        ));
    }

    let audio_status = Command::new("tccutil")
        .args(["reset", "AudioCapture", bundle_identifier])
        .status()
        .map_err(|error| format!("could not reset System Audio Recording access: {}", error))?;
    if !audio_status.success() {
        return Err(format!(
            "tccutil reset AudioCapture {} exited with status {}",
            bundle_identifier, audio_status
        ));
    }

    SCREEN_RECORDING_SETTINGS_REQUIRED.store(false, Ordering::Relaxed);
    invalidate_availability_cache();
    open_screen_recording_settings()
}

#[cfg(not(target_os = "macos"))]
pub fn repair_permissions(_bundle_identifier: &str) -> Result<(), String> {
    Err("call capture permission repair is only available on macOS".into())
}

#[cfg(target_os = "macos")]
fn open_screen_recording_settings() -> Result<(), String> {
    let status = Command::new("open")
        .arg(SCREEN_RECORDING_SETTINGS_URL)
        .status()
        .map_err(|error| format!("could not open Screen Recording settings: {}", error))?;
    if !status.success() {
        return Err(format!(
            "open {} exited with status {}",
            SCREEN_RECORDING_SETTINGS_URL, status
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        availability_from_probe, availability_from_probe_with_screen_recording_state,
        parse_macos_major_version, CallCaptureAvailability, MicrophonePermission, PermissionProbe,
        SCREEN_RECORDING_SETTINGS_DETAIL,
    };

    #[test]
    fn parses_major_version_from_product_version() {
        assert_eq!(parse_macos_major_version("15.0.1"), Some(15));
        assert_eq!(parse_macos_major_version("14.7"), Some(14));
        assert_eq!(parse_macos_major_version(""), None);
        assert_eq!(parse_macos_major_version("not-a-version"), None);
    }

    #[test]
    fn availability_probe_marks_screen_recording_prompt_as_startable() {
        let availability = availability_from_probe(PermissionProbe {
            screen_recording: false,
            microphone: MicrophonePermission::Authorized,
        });

        assert!(matches!(
            availability,
            CallCaptureAvailability::PermissionRequired {
                can_start: true,
                ..
            }
        ));
        assert!(availability.can_attempt_capture());
    }

    #[test]
    fn availability_probe_blocks_when_microphone_access_is_denied() {
        let availability = availability_from_probe(PermissionProbe {
            screen_recording: true,
            microphone: MicrophonePermission::Denied,
        });

        assert!(matches!(
            availability,
            CallCaptureAvailability::PermissionRequired {
                can_start: false,
                ..
            }
        ));
        assert!(!availability.can_attempt_capture());
        assert!(availability.detail().contains("Microphone"));
    }

    #[test]
    fn availability_probe_reports_ready_when_permissions_are_granted() {
        let availability = availability_from_probe(PermissionProbe {
            screen_recording: true,
            microphone: MicrophonePermission::Authorized,
        });

        assert!(matches!(
            availability,
            CallCaptureAvailability::Available { .. }
        ));
        assert!(availability.capability().can_start);
    }

    #[test]
    fn availability_probe_blocks_after_screen_recording_was_already_denied() {
        let availability = availability_from_probe_with_screen_recording_state(
            PermissionProbe {
                screen_recording: false,
                microphone: MicrophonePermission::Authorized,
            },
            true,
        );

        assert!(matches!(
            availability,
            CallCaptureAvailability::PermissionRequired {
                can_start: false,
                ..
            }
        ));
        assert_eq!(availability.detail(), SCREEN_RECORDING_SETTINGS_DETAIL);
    }
}
