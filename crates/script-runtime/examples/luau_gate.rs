//! Measurements for the Luau feasibility gate.
//!
//! `cargo run --release -p script-runtime --example luau_gate`
//!
//! Prints VM creation time, heap per idle VM, host-call overhead, and
//! budget enforcement latency. Numbers from emulated runs are not
//! performance evidence.

use std::hint::black_box;
use std::time::{Duration, Instant};

use script_runtime::{Limits, Script};

fn main() {
    let host = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    // VM creation and idle heap.
    const VMS: usize = 500;
    let started = Instant::now();
    let vms: Vec<Script> = (0..VMS)
        .map(|_| Script::new(Limits::default()).expect("vm"))
        .collect();
    let create = started.elapsed() / VMS as u32;
    let heap: usize = vms.iter().map(Script::used_memory).sum::<usize>() / VMS;
    drop(vms);

    // Host-call overhead: a loop calling dreamer.echo versus an empty loop.
    let s = Script::new(Limits {
        call_time: Duration::from_secs(30),
        ..Limits::default()
    })
    .expect("vm");
    const CALLS: u32 = 1_000_000;
    let time = |src: &str| {
        let t = Instant::now();
        black_box(s.exec("bench", src).expect("bench"));
        t.elapsed()
    };
    let empty = time(&format!(
        "local x = 0 for i = 1, {CALLS} do x = x + i end return x"
    ));
    let calls = time(&format!(
        "local x = 0 for i = 1, {CALLS} do x = x + dreamer.echo(i) end return x"
    ));
    let per_call = calls.saturating_sub(empty) / CALLS;

    // How long a runaway loop runs past a 20 ms budget.
    let budget = Duration::from_millis(20);
    let r = Script::new(Limits {
        call_time: budget,
        ..Limits::default()
    })
    .expect("vm");
    let t = Instant::now();
    let _ = r.exec("loop", "while true do end");
    let overrun = t.elapsed().saturating_sub(budget);

    println!("luau-gate host={host}");
    println!("  vm_create_mean_us     {:.1}", create.as_secs_f64() * 1e6);
    println!("  vm_idle_heap_bytes    {heap}");
    println!("  host_call_ns          {}", per_call.as_nanos());
    println!(
        "  time_budget_overrun_us {:.0}",
        overrun.as_secs_f64() * 1e6
    );
}
