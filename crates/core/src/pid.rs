use crate::config::Config;
use crate::error::PidError;
use std::fs;
use std::path::PathBuf;

// ──────────────────────────────────────────────────────────────
// PID file state machine:
//
//   [none] ──create──▶ [recording] ──remove──▶ [none]
//                           │
//                     (process dies)
//                           │
//                           ▼
//                      [stale] ──cleanup──▶ [none]
//
// Files:
//   ~/.minutes/recording.pid   — contains PID as text
//   ~/.minutes/current.wav     — audio being captured
//   ~/.minutes/last-result.json — written by record on shutdown
// ──────────────────────────────────────────────────────────────

/// Path to the recording PID file (`~/.minutes/recording.pid`).
pub fn pid_path() -> PathBuf {
    Config::minutes_dir().join("recording.pid")
}

/// Path to the in-progress audio capture file (`~/.minutes/current.wav`).
pub fn current_wav_path() -> PathBuf {
    Config::minutes_dir().join("current.wav")
}

/// Path to the last recording result JSON (`~/.minutes/last-result.json`).
pub fn last_result_path() -> PathBuf {
    Config::minutes_dir().join("last-result.json")
}

/// Path to the processing status JSON (`~/.minutes/processing-status.json`).
pub fn processing_status_path() -> PathBuf {
    Config::minutes_dir().join("processing-status.json")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessingStatus {
    pub processing: bool,
    pub stage: Option<String>,
    pub owner_pid: u32,
}

pub fn set_processing_status(stage: Option<&str>) -> std::io::Result<()> {
    let path = processing_status_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let status = ProcessingStatus {
        processing: true,
        stage: stage.map(String::from),
        owner_pid: std::process::id(),
    };
    let json = serde_json::to_string(&status)?;
    fs::write(path, json)
}

pub fn clear_processing_status() -> std::io::Result<()> {
    let path = processing_status_path();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn read_processing_status() -> ProcessingStatus {
    let path = processing_status_path();
    if !path.exists() {
        return ProcessingStatus {
            processing: false,
            stage: None,
            owner_pid: 0,
        };
    }

    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<ProcessingStatus>(&s).ok())
        .and_then(|status| {
            if status.owner_pid != 0 && is_process_alive(status.owner_pid) {
                Some(status)
            } else {
                clear_processing_status().ok();
                None
            }
        })
        .unwrap_or(ProcessingStatus {
            processing: false,
            stage: None,
            owner_pid: 0,
        })
}

/// Check if a recording is currently in progress.
/// Returns Ok(Some(pid)) if recording, Ok(None) if not.
/// Cleans up stale PID files automatically.
pub fn check_recording() -> Result<Option<u32>, PidError> {
    let path = pid_path();
    if !path.exists() {
        return Ok(None);
    }

    let pid_str = fs::read_to_string(&path)?;
    let pid: u32 = pid_str.trim().parse().map_err(|_| PidError::StalePid(0))?;

    if is_process_alive(pid) {
        Ok(Some(pid))
    } else {
        // Stale PID — process is dead. Clean up.
        tracing::warn!("stale PID file found (PID {pid} is dead). Cleaning up.");
        cleanup_stale()?;
        Ok(None)
    }
}

/// Create PID file for current process with exclusive file lock.
/// Uses flock to make the check-and-write atomic, preventing TOCTOU races
/// when two `minutes record` invocations start simultaneously.
pub fn create() -> Result<(), PidError> {
    use fs2::FileExt;
    use std::io::Write;

    let path = pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open/create the PID file and acquire an exclusive lock.
    // This is atomic: if another process holds the lock, we block briefly then check.
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    // Try non-blocking lock — if we can't get it, another recorder is running
    if file.try_lock_exclusive().is_err() {
        // Read the existing PID to report which process holds it
        let existing_pid = fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        return Err(PidError::AlreadyRecording(existing_pid));
    }

    // We hold the lock. Check if there's a stale PID from a crashed process.
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if let Ok(old_pid) = existing.trim().parse::<u32>() {
        if old_pid != 0 && is_process_alive(old_pid) {
            file.unlock().ok();
            return Err(PidError::AlreadyRecording(old_pid));
        }
    }

    // Write our PID (we still hold the lock)
    let pid = std::process::id();
    // Truncate and write
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)?;
    write!(file, "{}", pid)?;

    tracing::debug!("PID file created: {} (PID {})", path.display(), pid);
    Ok(())
}

/// Remove PID file. Called on graceful shutdown.
pub fn remove() -> Result<(), PidError> {
    let path = pid_path();
    if path.exists() {
        fs::remove_file(&path)?;
        tracing::debug!("PID file removed: {}", path.display());
    }
    Ok(())
}

/// Clean up stale recording artifacts.
fn cleanup_stale() -> Result<(), PidError> {
    let path = pid_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    // Don't delete current.wav — it may contain recoverable audio
    Ok(())
}

/// Check if a process with the given PID is alive.
pub fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks if the process exists without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Recording status, returned by `minutes status`.
#[derive(Debug, serde::Serialize)]
pub struct RecordingStatus {
    pub recording: bool,
    pub processing: bool,
    pub processing_stage: Option<String>,
    pub pid: Option<u32>,
    pub duration_secs: Option<f64>,
    pub wav_path: Option<String>,
}

/// Get current recording status.
pub fn status() -> RecordingStatus {
    let processing = read_processing_status();
    match check_recording() {
        Ok(Some(pid)) => {
            let wav = current_wav_path();
            let duration = wav
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|modified| {
                    std::time::SystemTime::now()
                        .duration_since(modified)
                        .ok()
                        .map(|d| d.as_secs_f64())
                });

            RecordingStatus {
                recording: true,
                processing: false,
                processing_stage: None,
                pid: Some(pid),
                // Duration is approximate: time since WAV was last modified.
                // The record process writes continuously, so this is close.
                duration_secs: duration,
                wav_path: Some(wav.display().to_string()),
            }
        }
        _ => RecordingStatus {
            recording: false,
            processing: processing.processing,
            processing_stage: processing.stage,
            pid: None,
            duration_secs: None,
            wav_path: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn is_process_alive_detects_current_process() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn is_process_alive_returns_false_for_dead_pid() {
        // PID 99999999 almost certainly doesn't exist
        assert!(!is_process_alive(99_999_999));
    }

    #[test]
    fn processing_status_round_trip() {
        set_processing_status(Some("Transcribing audio")).unwrap();
        let status = read_processing_status();
        assert!(status.processing);
        assert_eq!(status.stage.as_deref(), Some("Transcribing audio"));
        assert_eq!(status.owner_pid, std::process::id());
        clear_processing_status().unwrap();
    }
}
