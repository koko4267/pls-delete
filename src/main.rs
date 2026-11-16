#![windows_subsystem = "windows"]

mod app;
mod drives;
mod sdelete;
mod worker;

use eframe::egui;
use std::sync::Arc;

use app::PlsDeleteApp;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

struct SingleInstanceGuard {
    _handle: HANDLE,
}

fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    let name: Vec<u16> = "Global\\pls-delete-single-instance-lock"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle.is_null() {
        return None;
    }

    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let msg: Vec<u16> = "pls-delete is already running."
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let title: Vec<u16> = "pls-delete"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                msg.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        return None;
    }

    Some(SingleInstanceGuard { _handle: handle })
}

fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
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
    let _instance = match acquire_single_instance() {
        Some(guard) => guard,
        None => return Ok(()),
    };

    let is_admin = is_elevated();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([360.0, 240.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_title("pls-delete")
            .with_icon(Arc::new(load_window_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "pls-delete",
        native_options,
        Box::new(move |cc| Ok(Box::new(PlsDeleteApp::new(cc, is_admin)))),
    )
}
