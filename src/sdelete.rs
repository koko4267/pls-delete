use std::path::PathBuf;

pub fn is_sdelete_temp_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("sdeltemp") || (lower.starts_with("sdel") && lower.ends_with(".tmp"))
}

pub fn cleanup_temp_files(drive: &str) -> usize {
    let root = if drive.ends_with('\\') {
        drive.to_string()
    } else {
        format!("{}\\", drive)
    };

    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if is_sdelete_temp_file(&name) && std::fs::remove_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

pub fn find_sdelete() -> Option<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_sdelete_temp_file() {
        assert!(is_sdelete_temp_file("SDELtemp0001"));
        assert!(is_sdelete_temp_file("sdeltemp"));
        assert!(is_sdelete_temp_file("SDELTEMP.bin"));
        assert!(is_sdelete_temp_file("sdel123.tmp"));
        assert!(is_sdelete_temp_file("SDELabc.TMP"));

        assert!(!is_sdelete_temp_file("sdelete.exe"));
        assert!(!is_sdelete_temp_file("sdelete64.exe"));
        assert!(!is_sdelete_temp_file("important_file.txt"));
        assert!(!is_sdelete_temp_file("temp.tmp"));
    }
}
