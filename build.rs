fn version_to_file_version(version: &str) -> String {
    // Map semver like 2.0.1[-pre] to Windows file version 2.0.1.0
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    format!("{}.{}.{}.0", major, minor, patch)
}

fn main() {
    #[cfg(windows)]
    {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
        let file_version = version_to_file_version(&version);

        let mut res = winres::WindowsResource::new();
        res.set("ProductName", "TurboFind");
        res.set(
            "FileDescription",
            "Fast file indexer and search for Windows",
        );
        res.set("ProductVersion", &version);
        res.set("FileVersion", &file_version);
        res.set("OriginalFilename", "turbofind.exe");

        if let Err(e) = res.compile() {
            panic!("failed to compile Windows resources: {}", e);
        }
    }
}
