use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    // Вшиваем UAC-манифест с требованием прав администратора
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(
            new_manifest("SDeleteGUI")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("Не удалось внедрить UAC-манифест");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
