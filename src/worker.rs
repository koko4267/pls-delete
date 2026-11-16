use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::drives::get_disk_space;
use crate::sdelete::cleanup_temp_files;

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressUpdate {
    pub drive: String,
    pub fraction: f32,
    pub status_text: String,
    pub eta_text: Option<String>,
    pub is_finalizing: bool,
}

pub enum WorkerMsg {
    Progress(ProgressUpdate),
    Finished(Result<String, String>),
}

pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    if total_secs < 60 {
        format!("~{}s", total_secs)
    } else if total_secs < 3600 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("~{}m {}s", mins, secs)
    } else {
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        format!("~{}h {}m", hours, mins)
    }
}

pub fn play_notification_sound() {
    use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;
    unsafe {
        MessageBeep(MB_ICONASTERISK);
    }
}

#[derive(Clone, Default)]
pub struct ProcessController {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    active_drive: Arc<Mutex<Option<String>>>,
}

impl ProcessController {
    pub fn set_active(&self, drive: String) {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut guard = self.active_drive.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(drive);
    }

    pub fn kill_process(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
        }
        let active = {
            let mut drive_guard = self.active_drive.lock().unwrap_or_else(|e| e.into_inner());
            drive_guard.take()
        };
        if let Some(drive) = active {
            cleanup_temp_files(&drive);
        }
    }

    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        let mut drive_guard = self.active_drive.lock().unwrap_or_else(|e| e.into_inner());
        *drive_guard = None;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub fn spawn_wipe_worker(
    drive: String,
    sdelete_path: PathBuf,
    controller: ProcessController,
    tx: Sender<WorkerMsg>,
) {
    thread::spawn(move || {
        if controller.is_cancelled() {
            let _ = tx.send(WorkerMsg::Finished(Err("Wipe cancelled".to_string())));
            return;
        }

        let initial_free = get_disk_space(&drive).map(|(free, _)| free).unwrap_or(0);
        let drive_info = drive.clone();
        let _ = tx.send(WorkerMsg::Progress(ProgressUpdate {
            drive: drive_info.clone(),
            fraction: 0.0,
            status_text: format!("{}: Starting...", drive_info),
            eta_text: None,
            is_finalizing: false,
        }));

        let mut cmd = Command::new(&sdelete_path);
        cmd.args(["-accepteula", "-nobanner", "-z", &drive]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(WorkerMsg::Finished(Err(format!(
                    "Process launch failed: {}",
                    e
                ))));
                return;
            }
        };

        let stdout = child.stdout.take();
        let stdout_errors = Arc::new(Mutex::new(Vec::new()));
        let stdout_errors_clone = Arc::clone(&stdout_errors);

        if let Some(mut stream) = stdout {
            thread::spawn(move || {
                let mut raw_buf = [0u8; 256];
                let mut pending_byte: Option<u8> = None;
                let mut u16_buf = Vec::new();

                while let Ok(n) = stream.read(&mut raw_buf) {
                    if n == 0 {
                        break;
                    }
                    for &b in &raw_buf[..n] {
                        if let Some(prev) = pending_byte.take() {
                            let ch = u16::from_le_bytes([prev, b]);
                            if ch == 0x000D || ch == 0x000A {
                                if !u16_buf.is_empty() {
                                    let line =
                                        String::from_utf16_lossy(&u16_buf).trim().to_string();
                                    if !line.is_empty()
                                        && (line.contains("error")
                                            || line.contains("Error")
                                            || line.contains("failed")
                                            || line.contains("Failed")
                                            || line.contains("Access is denied"))
                                    {
                                        let mut guard = stdout_errors_clone
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner());
                                        guard.push(line);
                                    }
                                    u16_buf.clear();
                                }
                            } else {
                                u16_buf.push(ch);
                            }
                        } else {
                            pending_byte = Some(b);
                        }
                    }
                }
            });
        }

        {
            let mut guard = controller.child.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(child);
        }

        let start_time = Instant::now();
        let mut min_free_seen = initial_free;
        let mut phase2_detected = false;

        loop {
            if controller.is_cancelled() {
                let _ = tx.send(WorkerMsg::Finished(Err("Wipe cancelled".to_string())));
                return;
            }

            let exit_status = {
                let mut guard = controller.child.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(ref mut child) = *guard {
                    match child.try_wait() {
                        Ok(Some(status)) => Some(Ok(status)),
                        Ok(None) => None,
                        Err(e) => Some(Err(e)),
                    }
                } else {
                    Some(Ok(std::process::ExitStatus::default()))
                }
            };

            if let Some(status_result) = exit_status {
                if controller.is_cancelled() {
                    let _ = tx.send(WorkerMsg::Finished(Err("Wipe cancelled".to_string())));
                    return;
                }

                play_notification_sound();

                match status_result {
                    Ok(status) if status.success() => {
                        let _ = tx.send(WorkerMsg::Finished(Ok(
                            "Wipe completed successfully".to_string()
                        )));
                    }
                    Ok(status) => {
                        let errs = stdout_errors.lock().unwrap_or_else(|e| e.into_inner());
                        let err_msg = if let Some(last_err) = errs.last() {
                            format!("Error: {}", last_err)
                        } else {
                            format!("Process exited with code: {:?}", status.code())
                        };
                        let _ = tx.send(WorkerMsg::Finished(Err(err_msg)));
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::Finished(Err(format!(
                            "Process execution error: {}",
                            e
                        ))));
                    }
                }
                return;
            }

            if let Some((current_free, _)) = get_disk_space(&drive) {
                if current_free < min_free_seen {
                    min_free_seen = current_free;
                }

                let reached_bottom = min_free_seen < (initial_free / 10).max(500 * 1024 * 1024);
                if reached_bottom && current_free > min_free_seen + (100 * 1024 * 1024) {
                    phase2_detected = true;
                }

                if phase2_detected {
                    let _ = tx.send(WorkerMsg::Progress(ProgressUpdate {
                        drive: drive.clone(),
                        fraction: 1.0,
                        status_text: format!("{}: Finalizing: freeing temporary space...", drive),
                        eta_text: Some("Cleaning up temp files...".to_string()),
                        is_finalizing: true,
                    }));
                } else {
                    let wiped = initial_free.saturating_sub(current_free);
                    let fraction = if initial_free > 0 {
                        (wiped as f32 / initial_free as f32).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let wiped_gb = wiped as f64 / 1_073_741_824.0;
                    let total_gb = initial_free as f64 / 1_073_741_824.0;
                    let pct = fraction * 100.0;

                    let elapsed = start_time.elapsed();
                    let eta_text = if wiped > 50 * 1024 * 1024 && elapsed.as_secs() >= 3 {
                        let bytes_per_sec = wiped as f64 / elapsed.as_secs_f64();
                        let remaining_bytes = initial_free.saturating_sub(wiped);
                        if bytes_per_sec > 0.0 {
                            let secs = (remaining_bytes as f64 / bytes_per_sec).round() as u64;
                            Some(format!(
                                "ETA: {}",
                                format_duration(Duration::from_secs(secs))
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let _ = tx.send(WorkerMsg::Progress(ProgressUpdate {
                        drive: drive.clone(),
                        fraction,
                        status_text: format!(
                            "{}: Wiping free space: {:.1} / {:.1} GB ({:.0}%)",
                            drive, wiped_gb, total_gb, pct
                        ),
                        eta_text,
                        is_finalizing: false,
                    }));
                }
            }

            thread::sleep(Duration::from_millis(250));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(35)), "~35s");
        assert_eq!(format_duration(Duration::from_secs(60)), "~1m 0s");
        assert_eq!(format_duration(Duration::from_secs(125)), "~2m 5s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "~1h 0m");
        assert_eq!(format_duration(Duration::from_secs(3720)), "~1h 2m");
    }
}
