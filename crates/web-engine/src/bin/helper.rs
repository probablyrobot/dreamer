//! The helper executable CEF starts for Chromium's renderer, GPU, and
//! utility processes. The Dreamer Web component holds five copies of it,
//! one per helper app (`dreamer Helper.app`, `dreamer Helper (GPU).app`,
//! ...), each loading the framework that sits beside the helper apps.

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(web_engine::run_subprocess());
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("dreamer-web-helper: web panes are macOS-only so far");
        std::process::exit(1);
    }
}
