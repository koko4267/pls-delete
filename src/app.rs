use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use crate::drives::{get_available_drives, DriveInfo};
use crate::sdelete::find_sdelete;
use crate::worker::{spawn_wipe_worker, ProcessController, WorkerMsg};

pub struct PlsDeleteApp {
    drives: Vec<DriveInfo>,
    selected_drive: String,
    status: String,
    progress: f32,
    eta_text: Option<String>,
    is_running: bool,
    is_finalizing: bool,
    is_admin: bool,
    confirm_dialog_open: bool,
    controller: ProcessController,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
}

impl PlsDeleteApp {
    pub fn new(cc: &eframe::CreationContext<'_>, is_admin: bool) -> Self {
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

        let drives = get_available_drives();
        let selected_drive = drives.first().map(|d| d.letter.clone()).unwrap_or_default();
        let (tx, rx) = channel();

        Self {
            drives,
            selected_drive,
            status: "Ready".to_string(),
            progress: 0.0,
            eta_text: None,
            is_running: false,
            is_finalizing: false,
            is_admin,
            confirm_dialog_open: false,
            controller: ProcessController::default(),
            tx,
            rx,
        }
    }

    pub fn refresh_drives(&mut self) {
        self.drives = get_available_drives();
        if !self.drives.iter().any(|d| d.letter == self.selected_drive) {
            self.selected_drive = self
                .drives
                .first()
                .map(|d| d.letter.clone())
                .unwrap_or_default();
        }
    }

    pub fn start_cleaning(&mut self) {
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
        self.is_finalizing = false;
        self.progress = 0.0;
        self.eta_text = None;
        self.status = format!("Preparing to wipe {}...", drive);

        self.controller.reset();
        self.controller.set_active(drive.clone());

        spawn_wipe_worker(
            drive,
            sdelete_path,
            self.controller.clone(),
            self.tx.clone(),
        );
    }

    pub fn cancel(&mut self) {
        self.controller.kill_process();
        self.status = "Aborting process...".to_string();
        self.eta_text = None;
    }
}

impl eframe::App for PlsDeleteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::Progress(update) => {
                    self.status = update.status_text;
                    self.progress = update.fraction;
                    self.eta_text = update.eta_text;
                    self.is_finalizing = update.is_finalizing;
                }
                WorkerMsg::Finished(res) => {
                    self.is_running = false;
                    self.is_finalizing = false;
                    self.eta_text = None;
                    self.refresh_drives();
                    match res {
                        Ok(success) => {
                            self.status = success;
                            self.progress = 1.0;
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

        if self.confirm_dialog_open {
            egui::Window::new("Confirm Secure Wipe")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(280.0);
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Are you sure you want to wipe free space on {}?",
                            self.selected_drive
                        ))
                        .strong()
                        .size(13.0),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "This will zero-fill unallocated free space. Deleted files will become permanently unrecoverable. Existing files are untouched.",
                        )
                        .size(11.5)
                        .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_dialog_open = false;
                        }
                        ui.add_space(10.0);
                        let wipe_btn = egui::Button::new(
                            egui::RichText::new("Yes, Wipe Drive").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(180, 45, 45));
                        if ui.add(wipe_btn).clicked() {
                            self.confirm_dialog_open = false;
                            self.start_cleaning();
                        }
                    });
                    ui.add_space(4.0);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(12.0);

                if !self.is_admin {
                    ui.label(
                        egui::RichText::new("⚠ Warning: Not running as Administrator")
                            .size(11.5)
                            .color(egui::Color32::from_rgb(255, 185, 0)),
                    );
                    ui.add_space(6.0);
                }

                ui.add_enabled_ui(!self.is_running && !self.drives.is_empty(), |ui| {
                    ui.horizontal(|ui| {
                        let row_width = 250.0;
                        let offset = ((ui.available_width() - row_width) / 2.0).max(0.0);
                        ui.add_space(offset);

                        ui.label("Drive:");

                        let selected_display = self
                            .drives
                            .iter()
                            .find(|d| d.letter == self.selected_drive)
                            .map(|d| d.display_label())
                            .unwrap_or_else(|| {
                                if self.selected_drive.is_empty() {
                                    "—".to_string()
                                } else {
                                    self.selected_drive.clone()
                                }
                            });

                        egui::ComboBox::from_id_salt("drive_selector")
                            .width(170.0)
                            .selected_text(selected_display)
                            .show_ui(ui, |ui| {
                                for drive in &self.drives {
                                    ui.selectable_value(
                                        &mut self.selected_drive,
                                        drive.letter.clone(),
                                        drive.display_label(),
                                    );
                                }
                            });

                        if ui
                            .button("🔄")
                            .on_hover_text("Refresh drive list")
                            .clicked()
                        {
                            self.refresh_drives();
                        }
                    });
                });

                ui.add_space(14.0);

                ui.add_enabled_ui(!self.selected_drive.is_empty() && !self.is_running, |ui| {
                    let btn = egui::Button::new("Wipe Selected Drive");
                    if ui.add_sized([250.0, 34.0], btn).clicked() {
                        self.confirm_dialog_open = true;
                    }
                });

                ui.add_space(12.0);

                if self.is_running {
                    let pb = egui::ProgressBar::new(self.progress)
                        .show_percentage()
                        .animate(self.progress == 0.0 || self.is_finalizing);
                    ui.add_sized([250.0, 16.0], pb);
                    ui.add_space(6.0);
                }

                let status_color = if self.status.starts_with("Error") {
                    egui::Color32::from_rgb(255, 90, 90)
                } else if self.status.contains("successfully") {
                    egui::Color32::from_rgb(100, 220, 100)
                } else if self.is_running {
                    ui.visuals().text_color()
                } else {
                    ui.visuals().weak_text_color()
                };

                ui.label(
                    egui::RichText::new(&self.status)
                        .size(12.5)
                        .color(status_color),
                );

                if let Some(ref eta) = self.eta_text {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(eta)
                            .size(11.5)
                            .color(ui.visuals().weak_text_color()),
                    );
                }

                ui.add_space(12.0);

                if self.is_running {
                    let cancel_btn = egui::Button::new("Cancel");
                    if ui.add_sized([100.0, 26.0], cancel_btn).clicked() {
                        self.cancel();
                    }
                } else {
                    ui.add_space(26.0);
                }
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cancel();
    }
}
