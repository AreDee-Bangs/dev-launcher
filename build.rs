fn main() {
    // Use SOURCE_DATE_EPOCH (reproducible builds) if set, otherwise call the
    // platform date command.  Falls back to an empty string so the build never
    // fails on a platform where the command is unavailable.
    let ts = if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        epoch
    } else {
        #[cfg(unix)]
        {
            std::process::Command::new("date")
                .arg("+%m%d%Y%H%M")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
        }
        #[cfg(windows)]
        {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    "Get-Date -Format 'MMddyyyyHHmm'"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .unwrap_or_default()
        }
    };
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", ts.trim());
    println!("cargo:rerun-if-changed=build.rs");
}
