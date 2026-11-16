use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

#[derive(Clone, Debug, PartialEq)]
pub struct DriveInfo {
    pub letter: String, // e.g. "C:"
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub is_removable: bool,
}

impl DriveInfo {
    pub fn display_label(&self) -> String {
        let free_gb = self.free_bytes as f64 / 1_073_741_824.0;
        let total_gb = self.total_bytes as f64 / 1_073_741_824.0;
        let drive_type_tag = if self.is_removable { " [USB]" } else { "" };
        format!(
            "{}{} ({:.1} / {:.1} GB free)",
            self.letter, drive_type_tag, free_gb, total_gb
        )
    }
}

pub fn get_disk_space(drive: &str) -> Option<(u64, u64)> {
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

pub fn get_available_drives() -> Vec<DriveInfo> {
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
        let is_fixed = drive_type == DRIVE_FIXED;
        let is_removable = drive_type == DRIVE_REMOVABLE;

        if is_fixed || is_removable {
            if let Ok(s) = String::from_utf16(part) {
                let letter = s.trim_end_matches('\\').to_string();
                let (free_bytes, total_bytes) = get_disk_space(&letter).unwrap_or((0, 0));
                drives.push(DriveInfo {
                    letter,
                    free_bytes,
                    total_bytes,
                    is_removable,
                });
            }
        }
    }
    drives
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drive_info_display_label() {
        let drive = DriveInfo {
            letter: "C:".to_string(),
            free_bytes: 53_687_091_200,   // 50 GB
            total_bytes: 107_374_182_400, // 100 GB
            is_removable: false,
        };
        assert_eq!(drive.display_label(), "C: (50.0 / 100.0 GB free)");

        let usb = DriveInfo {
            letter: "E:".to_string(),
            free_bytes: 16_106_127_360,  // 15 GB
            total_bytes: 34_359_738_368, // 32 GB
            is_removable: true,
        };
        assert_eq!(usb.display_label(), "E: [USB] (15.0 / 32.0 GB free)");
    }
}
