//! `dream job-run DIR -- PROGRAM [ARGS...]`: the managed wrapper of a
//! tmux-backed job (spec 9: durable jobs). It runs the exact program in
//! its pane, records the program's pid and then its exit status in DIR,
//! and exits with the same status. It needs no daemon: a job started this
//! way runs to the end, and is recorded, whether or not `dreamerd` is
//! running.

use std::io::Write;
use std::path::Path;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Write `name` in `dir` atomically (a temporary file, then a rename).
fn write(dir: &Path, name: &str, text: &str) {
    let tmp = dir.join(format!(".{name}.tmp"));
    let ok = std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    if let Err(e) = ok {
        eprintln!("dream job-run: {}: {e}", dir.join(name).display());
    }
}

pub fn run(args: &[String]) -> Result<std::process::ExitCode, String> {
    let usage = "usage: dream job-run DIR -- PROGRAM [ARGS...]";
    let [dir, sep, argv @ ..] = args else {
        return Err(usage.into());
    };
    if sep != "--" || argv.is_empty() {
        return Err(usage.into());
    }
    let dir = Path::new(dir);
    // This wrapper's own pid: while it lives, a job whose program has
    // exited is still writing its status, not lost.
    write(dir, "wrapper", &format!("{}\n", std::process::id()));
    // Started by the daemon: wait (briefly) until it has set the pane to
    // stay after the program exits, so even instant output stays readable.
    if std::env::var_os("DREAMER_JOB_HOLD").is_some() {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dir.join("kept").exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    let child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .env_remove("DREAMER_JOB_HOLD")
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            println!("[dream: could not start {}: {e}]", argv[0]);
            write(
                dir,
                "status.json",
                &format!(
                    "{{\"exit_code\":127,\"error\":{},\"ended_unix_ms\":{}}}\n",
                    serde_json::Value::String(e.to_string()),
                    now_ms()
                ),
            );
            return Ok(std::process::ExitCode::from(127));
        }
    };
    write(dir, "pid", &format!("{}\n", child.id()));
    let status = child.wait().map_err(|e| e.to_string())?;
    use std::os::unix::process::ExitStatusExt;
    let (json, code, said) = match (status.code(), status.signal()) {
        (Some(c), _) => (
            format!("{{\"exit_code\":{c},\"ended_unix_ms\":{}}}\n", now_ms()),
            c,
            format!("exited with {c}"),
        ),
        (None, Some(s)) => (
            format!("{{\"signal\":{s},\"ended_unix_ms\":{}}}\n", now_ms()),
            128 + s,
            format!("ended by signal {s}"),
        ),
        _ => (
            format!("{{\"exit_code\":1,\"ended_unix_ms\":{}}}\n", now_ms()),
            1,
            "ended".to_owned(),
        ),
    };
    write(dir, "status.json", &json);
    println!("[job {said}]");
    Ok(std::process::ExitCode::from(
        u8::try_from(code).unwrap_or(1),
    ))
}
