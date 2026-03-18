use minutes_core::{Config, ContentType};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct AppState {
    pub recording: Arc<AtomicBool>,
    pub stop_flag: Arc<AtomicBool>,
}

fn preserve_failed_capture(wav_path: &std::path::Path, config: &Config) -> Option<PathBuf> {
    let metadata = wav_path.metadata().ok()?;
    if metadata.len() == 0 {
        return None;
    }

    let dir = config.output_dir.join("failed-captures");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!(
        "{}-capture.wav",
        chrono::Local::now().format("%Y-%m-%d-%H%M%S")
    ));

    std::fs::copy(wav_path, &dest).ok()?;
    std::fs::remove_file(wav_path).ok();
    Some(dest)
}

pub fn recording_active(recording: &Arc<AtomicBool>) -> bool {
    recording.load(Ordering::Relaxed) || minutes_core::pid::status().recording
}

pub fn request_stop(
    recording: &Arc<AtomicBool>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<(), String> {
    match minutes_core::pid::check_recording() {
        Ok(Some(pid)) => {
            if pid == std::process::id() {
                stop_flag.store(true, Ordering::Relaxed);
                recording.store(true, Ordering::Relaxed);
                Ok(())
            } else {
                let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok(())
            }
        }
        Ok(None) => {
            recording.store(false, Ordering::Relaxed);
            Err("Not recording".into())
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn wait_for_recording_shutdown(timeout: std::time::Duration) -> bool {
    let pid_path = minutes_core::pid::pid_path();
    let start = std::time::Instant::now();
    while pid_path.exists() && start.elapsed() < timeout {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    !pid_path.exists()
}

/// Start recording in a background thread.
pub fn start_recording(
    _app_handle: tauri::AppHandle,
    recording: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
) {
    recording.store(true, Ordering::Relaxed);
    stop_flag.store(false, Ordering::Relaxed);

    let config = Config::load();
    let wav_path = minutes_core::pid::current_wav_path();

    if let Err(e) = minutes_core::pid::create() {
        eprintln!("Failed to create PID: {}", e);
        recording.store(false, Ordering::Relaxed);
        return;
    }

    minutes_core::notes::save_recording_start().ok();
    eprintln!("Recording started...");

    let mut remove_current_wav = false;
    match minutes_core::capture::record_to_wav(&wav_path, stop_flag, &config) {
        Ok(()) => {
            let title = chrono::Local::now()
                .format("Recording %Y-%m-%d %H:%M")
                .to_string();
            match minutes_core::process(&wav_path, ContentType::Meeting, Some(&title), &config) {
                Ok(result) => {
                    remove_current_wav = true;
                    eprintln!(
                        "Saved: {} ({} words)",
                        result.path.display(),
                        result.word_count
                    );
                }
                Err(e) => {
                    if let Some(saved) = preserve_failed_capture(&wav_path, &config) {
                        eprintln!(
                            "Pipeline error: {}. Raw audio preserved at {}",
                            e,
                            saved.display()
                        );
                    } else {
                        eprintln!(
                            "Pipeline error: {}. Raw audio left at {}",
                            e,
                            wav_path.display()
                        );
                    }
                }
            }
        }
        Err(e) => {
            if let Some(saved) = preserve_failed_capture(&wav_path, &config) {
                eprintln!(
                    "Capture error: {}. Partial audio preserved at {}",
                    e,
                    saved.display()
                );
            } else {
                eprintln!("Capture error: {}", e);
            }
        }
    }

    minutes_core::notes::cleanup();
    minutes_core::pid::remove().ok();
    if remove_current_wav && wav_path.exists() {
        std::fs::remove_file(&wav_path).ok();
    }
    recording.store(false, Ordering::Relaxed);
}

#[tauri::command]
pub fn cmd_start_recording(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<(), String> {
    if recording_active(&state.recording) {
        return Err("Already recording".into());
    }
    state.recording.store(true, Ordering::Relaxed);
    let rec = state.recording.clone();
    let stop = state.stop_flag.clone();
    crate::update_tray_state(&app, true);
    let app_done = app.clone();
    std::thread::spawn(move || {
        start_recording(app, rec, stop);
        crate::update_tray_state(&app_done, false);
    });
    Ok(())
}

#[tauri::command]
pub fn cmd_stop_recording(state: tauri::State<AppState>) -> Result<(), String> {
    request_stop(&state.recording, &state.stop_flag)
}

#[tauri::command]
pub fn cmd_add_note(text: String) -> Result<String, String> {
    minutes_core::notes::add_note(&text)
}

#[tauri::command]
pub fn cmd_status(state: tauri::State<AppState>) -> serde_json::Value {
    let recording = state.recording.load(Ordering::Relaxed);
    let status = minutes_core::pid::status();

    // Get elapsed time if recording
    let elapsed = if recording || status.recording {
        let start_path = minutes_core::notes::recording_start_path();
        if start_path.exists() {
            if let Ok(s) = std::fs::read_to_string(&start_path) {
                if let Ok(start) = s.trim().parse::<u64>() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let e = now.saturating_sub(start);
                    Some(format!("{}:{:02}", e / 60, e % 60))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let audio_level = if recording || status.recording {
        minutes_core::capture::audio_level()
    } else {
        0
    };

    serde_json::json!({
        "recording": recording || status.recording,
        "pid": status.pid,
        "elapsed": elapsed,
        "audioLevel": audio_level,
    })
}

#[tauri::command]
pub fn cmd_list_meetings(limit: Option<usize>) -> serde_json::Value {
    let config = Config::load();
    let filters = minutes_core::search::SearchFilters {
        content_type: None,
        since: None,
        attendee: None,
    };
    match minutes_core::search::search("", &config, &filters) {
        Ok(results) => {
            let limited: Vec<_> = results.into_iter().take(limit.unwrap_or(20)).collect();
            serde_json::to_value(&limited).unwrap_or(serde_json::json!([]))
        }
        Err(_) => serde_json::json!([]),
    }
}

#[tauri::command]
pub fn cmd_search(query: String) -> serde_json::Value {
    let config = Config::load();
    let filters = minutes_core::search::SearchFilters {
        content_type: None,
        since: None,
        attendee: None,
    };
    match minutes_core::search::search(&query, &config, &filters) {
        Ok(results) => serde_json::to_value(&results).unwrap_or(serde_json::json!([])),
        Err(_) => serde_json::json!([]),
    }
}

#[tauri::command]
pub fn cmd_open_file(path: String) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn preserve_failed_capture_moves_audio_into_failed_captures() {
        let dir = TempDir::new().unwrap();
        let config = Config {
            output_dir: dir.path().join("meetings"),
            ..Config::default()
        };
        let wav = dir.path().join("current.wav");
        std::fs::write(&wav, vec![1_u8; 256]).unwrap();

        let preserved = preserve_failed_capture(&wav, &config).unwrap();

        assert!(!wav.exists());
        assert!(preserved.exists());
        assert!(preserved.starts_with(config.output_dir.join("failed-captures")));
    }
}
