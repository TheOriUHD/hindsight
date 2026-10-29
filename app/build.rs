fn main() {
    // FFmpeg DLLs + ffmpeg.exe must sit next to the binary for dev runs; the
    // installer ships them via bundle.resources.
    let bin = std::path::Path::new("../vendor/ffmpeg/bin");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR = target/<profile>/build/<pkg>/out -> target/<profile>
    if let Some(profile_dir) = out.ancestors().nth(3) {
        if let Ok(entries) = std::fs::read_dir(bin) {
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                if name.ends_with(".dll") || name == "ffmpeg.exe" {
                    let dst = profile_dir.join(&name);
                    if !dst.exists() {
                        let _ = std::fs::copy(&p, dst);
                    }
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=../vendor/ffmpeg/bin");
    tauri_build::build()
}
