#![windows_subsystem = "windows"]

use eframe::egui;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

const CREATE_NO_WINDOW: u32 = 0x08000000;

enum WorkerMsg {
    Progress(String),
    Finished(Result<String, String>),
}

fn cleanup_temp_files(drive: &str) {
    let root = if drive.ends_with('\\') {
        drive.to_string()
    } else {
        format!("{}\\", drive)
    };
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                let lower = name.to_lowercase();
                if lower.starts_with("sdeltemp")
                    || (lower.starts_with("sdel") && lower.ends_with(".tmp"))
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[derive(Clone, Default)]
struct ProcessController {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    active_drive: Arc<Mutex<Option<String>>>,
}

impl ProcessController {
    fn set_active(&self, drive: String) {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut guard = self.active_drive.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(drive);
    }

    fn kill_process(&self) {
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

    fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        let mut drive_guard = self.active_drive.lock().unwrap_or_else(|e| e.into_inner());
        *drive_guard = None;
    }
}

struct PlsDeleteApp {
    drives: Vec<String>,
    selected_drive: String,
    status: String,
    is_running: bool,
    controller: ProcessController,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
}

impl PlsDeleteApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
        let font_path = format!(r"{}\Fonts\segoeui.ttf", windir);
        if let Ok(font_bytes) = std::fs::read(&font_path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("segoeui".to_owned(), egui::FontData::from_owned(font_bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "segoeui".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("segoeui".to_owned());
            cc.egui_ctx.set_fonts(fonts);
        }

        let drives = get_fixed_drives();
        let selected_drive = drives.first().cloned().unwrap_or_default();
        let (tx, rx) = channel();

        Self {
            drives,
            selected_drive,
            status: "Ready".to_string(),
            is_running: false,
            controller: ProcessController::default(),
            tx,
            rx,
        }
    }

    fn start_cleaning(&mut self) {
        if self.selected_drive.is_empty() {
            self.status = "Error: No drive selected".to_string();
            return;
        }

        let sdelete_path = match find_sdelete() {
            Some(path) => path,
            None => {
                self.status = "Error: sdelete64.exe / sdelete.exe not found".to_string();
                return;
            }
        };

        let drive = self.selected_drive.clone();
        self.is_running = true;
        self.status = format!("Preparing to wipe {}...", drive);
        self.controller.reset();
        self.controller.set_active(drive.clone());

        let controller = self.controller.clone();
        let tx = self.tx.clone();

        thread::spawn(move || {
            if controller.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(WorkerMsg::Finished(Err("Wipe cancelled".to_string())));
                return;
            }

            let initial_free = get_disk_space(&drive).map(|(free, _)| free).unwrap_or(0);
            let drive_info = drive.clone();
            let _ = tx.send(WorkerMsg::Progress(format!("{}: Starting...", drive_info)));

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
                                                || line.contains("Failed"))
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

            let mut min_free_seen = initial_free;
            let mut phase2_detected = false;

            loop {
                if controller.cancelled.load(Ordering::SeqCst) {
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
                    if controller.cancelled.load(Ordering::SeqCst) {
                        let _ = tx.send(WorkerMsg::Finished(Err("Wipe cancelled".to_string())));
                        return;
                    }

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
                        let _ = tx.send(WorkerMsg::Progress(format!(
                            "{}: Finalizing: freeing temporary space...",
                            drive
                        )));
                    } else {
                        let wiped = initial_free.saturating_sub(current_free);
                        let pct = if initial_free > 0 {
                            ((wiped as f64 / initial_free as f64) * 100.0).clamp(0.0, 100.0)
                        } else {
                            100.0
                        };
                        let wiped_gb = wiped as f64 / 1_073_741_824.0;
                        let total_gb = initial_free as f64 / 1_073_741_824.0;

                        let _ = tx.send(WorkerMsg::Progress(format!(
                            "{}: Wiping free space: {:.1} / {:.1} GB ({:.0}%)",
                            drive, wiped_gb, total_gb, pct
                        )));
                    }
                }

                thread::sleep(Duration::from_millis(250));
            }
        });
    }

    fn cancel(&mut self) {
        self.controller.kill_process();
        self.status = "Aborting process...".to_string();
    }
}

impl eframe::App for PlsDeleteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Progress(text) => {
                    self.status = text;
                }
                WorkerMsg::Finished(res) => {
                    self.is_running = false;
                    match res {
                        Ok(success) => {
                            self.status = success;
                        }
                        Err(err) => {
                            self.status = err;
                        }
                    }
                }
            }
        }

        if self.is_running {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);

                ui.add_enabled_ui(!self.is_running && !self.drives.is_empty(), |ui| {
                    ui.horizontal(|ui| {
                        let row_width = 150.0;
                        let offset = ((ui.available_width() - row_width) / 2.0).max(0.0);
                        ui.add_space(offset);
                        ui.label("Drive:");
                        egui::ComboBox::from_id_salt("drive_selector")
                            .width(80.0)
                            .selected_text(if self.selected_drive.is_empty() {
                                "—"
                            } else {
                                &self.selected_drive
                            })
                            .show_ui(ui, |ui| {
                                for drive in &self.drives {
                                    ui.selectable_value(
                                        &mut self.selected_drive,
                                        drive.clone(),
                                        drive,
                                    );
                                }
                            });
                    });
                });

                ui.add_space(16.0);

                ui.add_enabled_ui(!self.selected_drive.is_empty() && !self.is_running, |ui| {
                    let btn = egui::Button::new("Wipe Selected Drive");
                    if ui.add_sized([240.0, 36.0], btn).clicked() {
                        self.start_cleaning();
                    }
                });

                ui.add_space(18.0);

                ui.label(
                    egui::RichText::new(&self.status)
                        .size(13.0)
                        .color(if self.is_running {
                            ui.visuals().text_color()
                        } else {
                            ui.visuals().weak_text_color()
                        }),
                );

                ui.add_space(18.0);

                if self.is_running {
                    let cancel_btn = egui::Button::new("Cancel");
                    if ui.add_sized([100.0, 28.0], cancel_btn).clicked() {
                        self.cancel();
                    }
                } else {
                    ui.add_space(28.0);
                }
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cancel();
    }
}

fn find_sdelete() -> Option<PathBuf> {
    let names = ["sdelete64.exe", "sdelete.exe"];

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            for name in &names {
                let candidate = parent.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        for name in &names {
            let candidate = cwd.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in &names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

fn get_disk_space(drive: &str) -> Option<(u64, u64)> {
    let path = if drive.ends_with('\\') {
        drive.to_string()
    } else {
        format!("{}\\", drive)
    };
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut free_bytes = 0u64;
    let mut total_bytes = 0u64;
    let mut total_free = 0u64;

    let res = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes,
            &mut total_bytes,
            &mut total_free,
        )
    };

    (res != 0).then_some((free_bytes, total_bytes))
}

fn get_fixed_drives() -> Vec<String> {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if len == 0 || len > buffer.len() as u32 {
        return Vec::new();
    }

    let mut drives = Vec::new();
    for part in buffer[..len as usize].split(|&c| c == 0) {
        if part.is_empty() {
            continue;
        }
        let mut root_path = part.to_vec();
        root_path.push(0);

        let drive_type = unsafe { GetDriveTypeW(root_path.as_ptr()) };
        if drive_type == DRIVE_FIXED {
            if let Ok(s) = String::from_utf16(part) {
                drives.push(s.trim_end_matches('\\').to_string());
            }
        }
    }
    drives
}

fn load_window_icon() -> egui::IconData {
    let icon_bytes = include_bytes!("../assets/logo.png");
    if let Ok(img) = image::load_from_memory(icon_bytes) {
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }
    } else {
        egui::IconData::default()
    }
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 220.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_title("pls-delete")
            .with_icon(Arc::new(load_window_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "pls-delete",
        native_options,
        Box::new(|cc| Ok(Box::new(PlsDeleteApp::new(cc)))),
    )
}
