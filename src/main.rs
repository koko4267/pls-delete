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

use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDriveStringsW};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

// Флаг Windows API: создание скрытого процесса без окна консоли
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Сообщения от фонового рабочего потока к графическому интерфейсу
enum WorkerMsg {
    Progress(String),
    Finished(Result<String, String>),
}

/// Управление дочерним процессом для отмены и безопасного завершения
#[derive(Clone, Default)]
struct ProcessController {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl ProcessController {
    fn kill_process(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap();
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
        }
    }

    fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut guard = self.child.lock().unwrap();
        *guard = None;
    }
}

/// Состояние приложения
struct SDeleteApp {
    drives: Vec<String>,
    selected_drive: String,
    status: String,
    is_running: bool,
    controller: ProcessController,
    tx: Sender<WorkerMsg>,
    rx: Receiver<WorkerMsg>,
}

impl SDeleteApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Установка темной темы по умолчанию
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        // Загрузка системного шрифта Segoe UI для четкого отображения кириллицы
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(font_bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
            fonts.font_data.insert(
                "segoeui".to_owned(),
                egui::FontData::from_owned(font_bytes),
            );
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
            status: "Готов к работе".to_string(),
            is_running: false,
            controller: ProcessController::default(),
            tx,
            rx,
        }
    }

    /// Запуск зачистки свободного места на выбранном диске
    fn start_cleaning(&mut self) {
        if self.selected_drive.is_empty() {
            self.status = "Ошибка: диск не выбран".to_string();
            return;
        }

        // Поиск исполняемого файла sdelete64.exe
        let sdelete_path = match find_sdelete() {
            Some(path) => path,
            None => {
                self.status = "Ошибка: sdelete64.exe не найден рядом или в PATH".to_string();
                return;
            }
        };

        let drive = self.selected_drive.clone();
        self.is_running = true;
        self.status = format!("Подготовка к зачистке {}...", drive);
        self.controller.reset();

        let controller = self.controller.clone();
        let tx = self.tx.clone();

        thread::spawn(move || {
            if controller.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(WorkerMsg::Finished(Err("Очистка отменена".to_string())));
                return;
            }

            let drive_info = drive.clone();
            let _ = tx.send(WorkerMsg::Progress(format!("{}: запуск...", drive_info)));

            // Формируем команду запуска: sdelete64.exe -accepteula -nobanner -z <ДИСК>
            let mut cmd = Command::new(&sdelete_path);
            cmd.args(["-accepteula", "-nobanner", "-z", &drive]);
            cmd.creation_flags(CREATE_NO_WINDOW);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Finished(Err(format!(
                        "Ошибка запуска процесса: {}",
                        e
                    ))));
                    return;
                }
            };

            let stdout = child.stdout.take();

            // Сохраняем дескриптор дочернего процесса для возможности отмены
            {
                let mut guard = controller.child.lock().unwrap();
                *guard = Some(child);
            }

            // Читаем поток вывода sdelete (в формате UTF-16LE) для отслеживания хода работы
            if let Some(mut stream) = stdout {
                let mut raw_buf = [0u8; 128];
                let mut pending_byte: Option<u8> = None;
                let mut u16_buf = Vec::new();

                while let Ok(n) = stream.read(&mut raw_buf) {
                    if n == 0 || controller.cancelled.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut i = 0;
                    while i < n {
                        let b = raw_buf[i];
                        i += 1;
                        if let Some(prev) = pending_byte.take() {
                            let ch = u16::from_le_bytes([prev, b]);
                            // Сбрасываем строку по \r, \n или сразу по символу '%'
                            if ch == 0x000D || ch == 0x000A || ch == 0x0025 {
                                if ch == 0x0025 {
                                    u16_buf.push(ch);
                                }
                                if !u16_buf.is_empty() {
                                    let line = String::from_utf16_lossy(&u16_buf).trim().to_string();
                                    if !line.is_empty() {
                                        let _ = tx.send(WorkerMsg::Progress(format!("{}: {}", drive_info, line)));
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

                // Обработка возможного остатка строки при завершении
                if !u16_buf.is_empty() {
                    let line = String::from_utf16_lossy(&u16_buf).trim().to_string();
                    if !line.is_empty() {
                        let _ = tx.send(WorkerMsg::Progress(format!("{}: {}", drive_info, line)));
                    }
                }
            }

            // Дожидаемся завершения процесса текущего диска
            let wait_result = {
                let mut guard = controller.child.lock().unwrap();
                if let Some(mut child) = guard.take() {
                    child.wait()
                } else {
                    Ok(std::process::ExitStatus::default())
                }
            };

            if controller.cancelled.load(Ordering::SeqCst) {
                let _ = tx.send(WorkerMsg::Finished(Err("Очистка отменена".to_string())));
                return;
            }

            if let Err(e) = wait_result {
                let _ = tx.send(WorkerMsg::Finished(Err(format!(
                    "Ошибка выполнения процесса: {}",
                    e
                ))));
                return;
            }

            let _ = tx.send(WorkerMsg::Finished(Ok("Очистка успешно завершена".to_string())));
        });
    }

    /// Принудительная отмена процесса
    fn cancel(&mut self) {
        self.controller.kill_process();
        self.status = "Прерывание процесса...".to_string();
    }
}

impl eframe::App for SDeleteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Обработка сообщений из рабочего потока
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

        // Если зачистка активна, перерисовываем интерфейс для плавного отображения статуса
        if self.is_running {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);

                // Выпадающий список выбора диска
                ui.add_enabled_ui(!self.is_running && !self.drives.is_empty(), |ui| {
                    ui.horizontal(|ui| {
                        let row_width = 150.0;
                        let offset = ((ui.available_width() - row_width) / 2.0).max(0.0);
                        ui.add_space(offset);
                        ui.label("Диск:");
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

                // Единственная кнопка: Очистить выбранный диск
                ui.add_enabled_ui(!self.is_running && !self.selected_drive.is_empty(), |ui| {
                    let btn = egui::Button::new("Очистить выбранный диск");
                    if ui.add_sized([240.0, 36.0], btn).clicked() {
                        self.start_cleaning();
                    }
                });

                ui.add_space(18.0);

                // Текстовый статус процесса
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

                // Кнопка отмены активна только во время процесса; в режиме ожидания отступ сохраняет сетку
                if self.is_running {
                    let cancel_btn = egui::Button::new("Отмена");
                    if ui.add_sized([100.0, 28.0], cancel_btn).clicked() {
                        self.cancel();
                    }
                } else {
                    ui.add_space(28.0);
                }
            });
        });
    }

    /// При закрытии окна принудительно убиваем дочерний процесс
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cancel();
    }
}

/// Поиск sdelete64.exe в текущей директории, рядом с бинарником или в PATH
fn find_sdelete() -> Option<PathBuf> {
    // 1. Поиск в текущей рабочей директории
    if let Ok(cwd) = std::env::current_dir() {
        let local = cwd.join("sdelete64.exe");
        if local.is_file() {
            return Some(local);
        }
    }

    // 2. Поиск рядом с запущенным бинарником
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let local = parent.join("sdelete64.exe");
            if local.is_file() {
                return Some(local);
            }
        }
    }

    // 3. Поиск в переменной окружения PATH
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("sdelete64.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Получение списка фиксированных (локальных физических) дисков системы
fn get_fixed_drives() -> Vec<String> {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if len == 0 || len > buffer.len() as u32 {
        return Vec::new();
    }

    let mut drives = Vec::new();
    let slice = &buffer[..len as usize];
    for part in slice.split(|&c| c == 0) {
        if part.is_empty() {
            continue;
        }
        // Добавляем завершающий 0 для нуль-терминированной строки
        let mut root_path = part.to_vec();
        root_path.push(0);

        let drive_type = unsafe { GetDriveTypeW(root_path.as_ptr()) };
        // Оставляем только DRIVE_FIXED
        if drive_type == DRIVE_FIXED {
            if let Ok(s) = String::from_utf16(part) {
                // Обрезаем обратный слэш: "C:\" -> "C:"
                let trimmed = s.trim_end_matches('\\').to_string();
                drives.push(trimmed);
            }
        }
    }
    drives
}

fn main() -> eframe::Result<()> {
    // Настройки нативного окна: фиксированный размер 320x220, без изменения размера и кнопки максимизации
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 220.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_title("SDelete GUI"),
        ..Default::default()
    };

    eframe::run_native(
        "SDelete GUI",
        native_options,
        Box::new(|cc| Ok(Box::new(SDeleteApp::new(cc)))),
    )
}
