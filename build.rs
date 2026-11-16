use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};
use std::process::Command;

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let is_release = std::env::var("PROFILE")
            .map(|p| p == "release")
            .unwrap_or(false);

        // Вшиваем UAC-манифест с требованием прав администратора для Release-сборки
        if is_release {
            embed_manifest(
                new_manifest("pls-delete")
                    .requested_execution_level(ExecutionLevel::RequireAdministrator),
            )
            .expect("Не удалось внедрить UAC-манифест");
        }

        // Компилируем и вшиваем иконку исполняемого файла (.exe) через windres
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let obj_path = format!("{}/app_icon.o", out_dir);
        let status = Command::new("windres")
            .args(["assets/app.rc", "-O", "coff", "-o", &obj_path])
            .status();

        if let Ok(s) = status {
            if s.success() {
                println!("cargo:rustc-link-arg={}", obj_path);
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
