fn main() {
    let icon = std::path::Path::new("icons/icon.ico");
    if !icon.exists() {
        std::fs::create_dir_all("icons").expect("create icon directory");
        // A valid 1x1 32-bit ICO used for the feasibility binary's Windows resource.
        let bytes: [u8; 70] = [
            0, 0, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 32, 0, 48, 0, 0, 0, 22, 0, 0, 0, 40, 0, 0, 0, 1, 0,
            0, 0, 2, 0, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 168, 230, 94, 255, 0, 0, 0, 0,
        ];
        std::fs::write(icon, bytes).expect("write feasibility icon");
    }
    tauri_build::build()
}
