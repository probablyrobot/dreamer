//! The helper executable CEF starts for Chromium's renderer, GPU, and
//! utility processes. On macOS the Dreamer Web component holds five copies
//! of it, one per helper app (`dreamer Helper.app`, `dreamer Helper
//! (GPU).app`, ...), each loading the framework that sits beside the
//! helper apps. On Linux it sits beside `libcef.so` in the component and
//! loads it from there (nothing links it).

fn main() {
    std::process::exit(web_engine::run_subprocess());
}
