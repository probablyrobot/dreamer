//! Sandboxed Luau scripts with memory and time budgets.
//!
//! Each [`Script`] owns one Luau VM. The VM is created in Luau's sandbox mode
//! (read-only built-in libraries and globals, a separate environment per
//! script), with only the safe standard libraries Luau ships: there is no
//! `io`, no `os.execute`/`os.getenv`, no `require`, and no native module
//! loading. The only way out is the host-provided `dreamer` table.
//!
//! Budgets:
//! - memory: a hard allocation ceiling per VM;
//! - time: an interrupt hook checks a deadline while Luau code runs and
//!   aborts the call when it passes.
//!
//! Exceeding a budget returns [`ScriptError::Budget`]; the VM stays usable,
//! and the caller decides whether to restart it (spec section 12.7).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, LuaOptions, StdLib, Value, VmState};

/// Resource limits for one script instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Hard ceiling on VM heap usage in bytes.
    pub memory_bytes: usize,
    /// Wall-clock budget for one call into the script.
    pub call_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 << 20,
            call_time: Duration::from_millis(50),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Memory,
    Time,
}

#[derive(Debug)]
pub enum ScriptError {
    Budget(Budget),
    /// Compile or runtime error inside the script.
    Script(String),
    /// Failure setting up the VM.
    Setup(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(Budget::Memory) => write!(f, "script exceeded its memory budget"),
            Self::Budget(Budget::Time) => write!(f, "script exceeded its time budget"),
            Self::Script(e) => write!(f, "script error: {e}"),
            Self::Setup(e) => write!(f, "script setup failed: {e}"),
        }
    }
}

impl std::error::Error for ScriptError {}

const TIME_BUDGET_MARKER: &str = "dreamer: time budget exceeded";

/// One sandboxed Luau VM.
#[derive(Debug)]
pub struct Script {
    lua: Lua,
    limits: Limits,
    deadline: Rc<Cell<Option<Instant>>>,
}

impl Script {
    pub fn new(limits: Limits) -> Result<Self, ScriptError> {
        let setup = |e: mlua::Error| ScriptError::Setup(e.to_string());
        // Luau's safe libraries only. `StdLib::ALL_SAFE` excludes nothing
        // dangerous here because Luau itself has no io/os.execute/package.
        let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::new()).map_err(setup)?;

        let host = lua.create_table().map_err(setup)?;
        host.set("version", env!("CARGO_PKG_VERSION"))
            .map_err(setup)?;
        // Identity call used to measure host-call overhead and in tests.
        host.set(
            "echo",
            lua.create_function(|_, v: Value| Ok(v)).map_err(setup)?,
        )
        .map_err(setup)?;
        host.set_readonly(true);
        lua.globals().set("dreamer", host).map_err(setup)?;

        // Freeze the built-in libraries and globals; scripts get their own
        // writable environment on top.
        lua.sandbox(true).map_err(setup)?;
        lua.set_memory_limit(limits.memory_bytes).map_err(setup)?;

        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let hook_deadline = Rc::clone(&deadline);
        lua.set_interrupt(move |_| match hook_deadline.get() {
            Some(d) if Instant::now() >= d => Err(mlua::Error::runtime(TIME_BUDGET_MARKER)),
            _ => Ok(VmState::Continue),
        });

        Ok(Self {
            lua,
            limits,
            deadline,
        })
    }

    /// Run a chunk of Luau source under the call-time budget.
    pub fn exec(&self, name: &str, source: &str) -> Result<Value, ScriptError> {
        self.deadline
            .set(Some(Instant::now() + self.limits.call_time));
        let result = self.lua.load(source).set_name(name).eval::<Value>();
        self.deadline.set(None);
        result.map_err(classify)
    }

    /// Bytes currently allocated by this VM.
    pub fn used_memory(&self) -> usize {
        self.lua.used_memory()
    }

    /// Access to the VM for host API registration and benchmarks.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }
}

fn classify(e: mlua::Error) -> ScriptError {
    if matches!(e, mlua::Error::MemoryError(_)) {
        return ScriptError::Budget(Budget::Memory);
    }
    let text = e.to_string();
    if text.contains(TIME_BUDGET_MARKER) {
        ScriptError::Budget(Budget::Time)
    } else if text.contains("not enough memory") {
        ScriptError::Budget(Budget::Memory)
    } else {
        ScriptError::Script(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> Script {
        Script::new(Limits::default()).expect("vm")
    }

    fn truthy(s: &Script, expr: &str) -> bool {
        match s.exec("t", &format!("return {expr}")).expect(expr) {
            Value::Boolean(b) => b,
            other => panic!("{expr} -> {other:?}"),
        }
    }

    #[test]
    fn no_ambient_io_process_env_or_module_loading() {
        let s = script();
        for expr in [
            "io == nil",
            "os.execute == nil",
            "os.getenv == nil",
            "os.remove == nil",
            "os.exit == nil",
            "package == nil",
            "dofile == nil",
            "loadfile == nil",
        ] {
            assert!(truthy(&s, expr), "{expr}");
        }
        // `require` and `load`, if present, cannot reach files or native code.
        assert!(s.exec("t", "return require('os')").is_err());
    }

    #[test]
    fn builtins_and_host_table_are_read_only() {
        let s = script();
        assert!(s.exec("t", "string.rep = nil").is_err());
        assert!(s.exec("t", "dreamer.echo = nil").is_err());
        assert!(truthy(&s, "dreamer.echo(41) + 1 == 42"));
        // Script globals are private and writable.
        s.exec("t", "counter = 1").expect("set global");
        assert!(truthy(&s, "counter == 1"));
    }

    #[test]
    fn runaway_loops_hit_the_time_budget_and_the_vm_survives() {
        let s = Script::new(Limits {
            call_time: Duration::from_millis(30),
            ..Limits::default()
        })
        .expect("vm");
        let started = Instant::now();
        let err = s.exec("loop", "while true do end").expect_err("must stop");
        assert!(matches!(err, ScriptError::Budget(Budget::Time)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(truthy(&s, "1 + 1 == 2"));
    }

    #[test]
    fn allocation_hits_the_memory_budget_and_the_vm_survives() {
        let s = Script::new(Limits {
            memory_bytes: 4 << 20,
            call_time: Duration::from_secs(5),
        })
        .expect("vm");
        let err = s
            .exec(
                "hog",
                "local t = {} for i = 1, 1e7 do t[i] = string.rep('x', 1024) .. i end",
            )
            .expect_err("must fail");
        assert!(matches!(err, ScriptError::Budget(Budget::Memory)), "{err}");
        s.lua().gc_collect().expect("gc");
        assert!(truthy(&s, "#'ok' == 2"));
    }

    #[test]
    fn script_errors_are_reported_as_such() {
        let err = script().exec("bad", "error('boom')").expect_err("error");
        assert!(
            matches!(&err, ScriptError::Script(m) if m.contains("boom")),
            "{err}"
        );
        let err = script().exec("syntax", "local = ").expect_err("error");
        assert!(matches!(err, ScriptError::Script(_)));
    }
}
