//! Lists audio devices and apps that are playing audio (read-only).
fn main() {
    for (label, output) in [("Output devices", true), ("Input devices", false)] {
        println!("{label}:");
        for d in hindsight_recorder::list_devices(output).unwrap_or_default() {
            println!("  {}", d.name);
        }
    }
    println!("Apps playing audio:");
    for a in hindsight_recorder::list_audio_apps().unwrap_or_default() {
        println!("  {}  [{}]", a.name, a.exe);
    }
}
