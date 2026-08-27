use std::fs;
use std::path::Path;

pub struct Battery {
    path: String,
}

impl Battery {
    pub fn new() -> Option<Self> {
        let dir = fs::read_dir("/sys/class/power_supply").ok()?;
        for entry in dir.flatten() {
            let name = entry.file_name().into_string().ok()?;
            if name.starts_with("BAT") {
                let path = format!("/sys/class/power_supply/{name}");
                if Path::new(&format!("{path}/capacity")).exists() {
                    return Some(Battery { path });
                }
            }
        }
        None
    }

    /// Returns (capacity_percent, charging).
    pub fn read(&self) -> Option<(u8, bool)> {
        let cap = fs::read_to_string(format!("{}/capacity", self.path))
            .ok()?
            .trim()
            .parse::<u8>()
            .ok()?;
        let charging = fs::read_to_string(format!("{}/status", self.path))
            .map(|s| s.trim() == "Charging")
            .unwrap_or(false);
        Some((cap.min(100), charging))
    }
}