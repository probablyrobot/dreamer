//! `dreamerd`: headless Dreamer server.
//!
//! Placeholder until its milestone. It exists so the headless build, the
//! ARM64 Linux cross-build, and the dependency-boundary check have a real
//! target from the start.

// The server owns the domain model; linking it here keeps it in the
// headless build on every target.
use domain::ConnectionEpoch;

fn version_line() -> String {
    format!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => println!("{}", version_line()),
        Some("--self-check") => {
            // Exercises the domain types in the shipped binary.
            let epoch = ConnectionEpoch::default().next();
            println!("domain ok (first epoch {})", epoch.0);
        }
        _ => {
            eprintln!("{}: not implemented yet", env!("CARGO_BIN_NAME"));
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_line_names_the_binary() {
        assert!(super::version_line().starts_with("dreamerd "));
    }
}
