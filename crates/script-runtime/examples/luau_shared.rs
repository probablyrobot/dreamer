//! Cost of many scripts sharing one sandboxed Luau VM (ADR 0001).
//!
//! `cargo run --release -p script-runtime --example luau_shared`

use mlua::{Function, Lua, LuaOptions, StdLib};
fn main() {
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::new()).expect("luau");
    lua.sandbox(true).expect("luau");
    lua.gc_collect().expect("luau");
    let base = lua.used_memory();
    let mut keep = Vec::new();
    const N: usize = 200;
    for i in 0..N {
        // A small filter-like script: its own environment table, a handler
        // closure, and a little state.
        let env = lua.create_table().expect("luau");
        env.set_metatable(Some({
            let m = lua.create_table().expect("luau");
            m.set("__index", lua.globals()).expect("luau");
            m
        }))
        .expect("luau");
        let f: Function = lua
            .load(format!("local count = 0\nlocal id = {i}\nreturn function(line) if string.find(line, 'error') then count = count + 1 end return count, id end"))
            .set_environment(env)
            .eval()
            .expect("luau");
        keep.push(f);
    }
    lua.gc_collect().expect("luau");
    let per = (lua.used_memory() - base) / N;
    println!("vm_base_bytes {base}  per_script_bytes {per}");
    let t = std::time::Instant::now();
    let mut total = 0i64;
    for _ in 0..1000 {
        for f in &keep {
            let (c, _id): (i64, i64) = f.call("an error line").expect("luau");
            total += c;
        }
    }
    println!(
        "handler_call_ns {}",
        t.elapsed().as_nanos() / (1000 * N as u128)
    );
    std::hint::black_box(total);
}
