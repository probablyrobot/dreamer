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
//!
//! With a [`Host`] ([`Script::with_host`]), `dreamer` is the module
//! generated from the API schema (`dreamer.terminal.read{...}` and so on).
//! Each call goes to the host, which performs it under the script's grant.
//! The script yields while the call is in flight: the time budget counts
//! only time the script itself runs, and each host call has its own
//! deadline.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, LuaOptions, StdLib, Value, VmState};

pub mod values;

/// Resource limits for one script instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Hard ceiling on VM heap usage in bytes.
    pub memory_bytes: usize,
    /// Time one call into the script may run, not counting time spent
    /// waiting for host calls.
    pub call_time: Duration,
    /// Deadline of one host call.
    pub host_call: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 16 << 20,
            call_time: Duration::from_millis(50),
            host_call: Duration::from_secs(5),
        }
    }
}

/// A host call that failed: the daemon's error, or a missed deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub code: i32,
    pub message: String,
}

pub type HostFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, HostError>>>>;

/// Where a script's `dreamer` calls go: one method, with its parameter
/// record as JSON, performed under the script's grant.
pub trait Host {
    fn call(&self, method: String, params: serde_json::Value) -> HostFuture;
}

/// The script's running time within one call into it: paused while it
/// waits for the host.
#[derive(Debug, Default)]
struct Clock {
    limit: Cell<Duration>,
    spent: Cell<Duration>,
    since: Cell<Option<Instant>>,
}

impl Clock {
    fn start(&self, limit: Duration) {
        self.limit.set(limit);
        self.spent.set(Duration::ZERO);
        self.since.set(Some(Instant::now()));
    }

    fn pause(&self) {
        if let Some(s) = self.since.take() {
            self.spent.set(self.spent.get() + s.elapsed());
        }
    }

    fn resume(&self) {
        if self.limit.get() > Duration::ZERO {
            self.since.set(Some(Instant::now()));
        }
    }

    fn stop(&self) {
        self.since.set(None);
        self.limit.set(Duration::ZERO);
    }

    fn exceeded(&self) -> bool {
        self.since
            .get()
            .is_some_and(|s| self.spent.get() + s.elapsed() >= self.limit.get())
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
const HOST_ERROR_MARKER: &str = "dreamer: host error";

/// One sandboxed Luau VM.
#[derive(Debug)]
pub struct Script {
    lua: Lua,
    limits: Limits,
    clock: Rc<Clock>,
}

impl Script {
    /// A VM whose `dreamer` table only has `version` and `echo` (for the
    /// M0 gate and benchmarks).
    pub fn new(limits: Limits) -> Result<Self, ScriptError> {
        let setup = |e: mlua::Error| ScriptError::Setup(e.to_string());
        let (lua, clock) = vm(limits)?;
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
        seal(&lua, limits)?;
        Ok(Self { lua, limits, clock })
    }

    /// A VM whose `dreamer` is the generated API module, calling `host`.
    pub fn with_host(limits: Limits, host: Rc<dyn Host>) -> Result<Self, ScriptError> {
        let setup = |e: mlua::Error| ScriptError::Setup(e.to_string());
        let (lua, clock) = vm(limits)?;
        let call_clock = clock.clone();
        let call = lua
            .create_async_function(move |lua, (method, params): (String, Value)| {
                let (host, clock) = (host.clone(), call_clock.clone());
                async move {
                    let params = values::to_json(&params, true).map_err(mlua::Error::runtime)?;
                    // Waiting is not the script's running time.
                    clock.pause();
                    let r =
                        tokio::time::timeout(limits.host_call, host.call(method.clone(), params))
                            .await;
                    clock.resume();
                    let result = match r {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => {
                            return Err(mlua::Error::runtime(format!(
                                "{HOST_ERROR_MARKER} {} in {method}: {}",
                                e.code, e.message
                            )));
                        }
                        Err(_) => {
                            return Err(mlua::Error::runtime(format!(
                                "{HOST_ERROR_MARKER} {} in {method}: no answer within {} ms",
                                api::code::UNAVAILABLE,
                                limits.host_call.as_millis()
                            )));
                        }
                    };
                    values::to_lua(&lua, &result)
                }
            })
            .map_err(setup)?;
        let make: mlua::Function = lua
            .load(api::LUAU_MODULE)
            .set_name("dreamer")
            .eval()
            .map_err(setup)?;
        let module: mlua::Table = make.call(call).map_err(setup)?;
        lua.globals().set("dreamer", module).map_err(setup)?;
        seal(&lua, limits)?;
        Ok(Self { lua, limits, clock })
    }

    /// Run a chunk of Luau source under the call-time budget.
    pub fn exec(&self, name: &str, source: &str) -> Result<Value, ScriptError> {
        self.clock.start(self.limits.call_time);
        let result = self.lua.load(source).set_name(name).eval::<Value>();
        self.clock.stop();
        result.map_err(classify)
    }

    /// Run a chunk that may call the host; it yields while calls are in
    /// flight. Needs a Tokio runtime (for host-call deadlines).
    pub async fn run(&self, name: &str, source: &str) -> Result<(), ScriptError> {
        self.clock.start(self.limits.call_time);
        let result = self.lua.load(source).set_name(name).exec_async().await;
        self.clock.stop();
        result.map_err(classify)
    }

    /// Call the script's global function `handler` with `arg`; `None` if
    /// the script defines no such function.
    pub async fn handle(
        &self,
        handler: &str,
        arg: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ScriptError> {
        let f: Option<mlua::Function> = self
            .lua
            .globals()
            .get(handler)
            .map_err(|e| ScriptError::Script(e.to_string()))?;
        let Some(f) = f else {
            return Ok(None);
        };
        let arg = values::to_lua(&self.lua, arg).map_err(classify)?;
        self.clock.start(self.limits.call_time);
        let result = f.call_async::<Value>(arg).await;
        self.clock.stop();
        let v = result.map_err(classify)?;
        values::to_json(&v, false)
            .map(Some)
            .map_err(ScriptError::Script)
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

/// A sandbox VM with the budgets' hooks, before its globals are frozen.
fn vm(limits: Limits) -> Result<(Lua, Rc<Clock>), ScriptError> {
    let setup = |e: mlua::Error| ScriptError::Setup(e.to_string());
    // Luau's safe libraries only. `StdLib::ALL_SAFE` excludes nothing
    // dangerous here because Luau itself has no io/os.execute/package.
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::new()).map_err(setup)?;
    let clock = Rc::new(Clock::default());
    let hook = clock.clone();
    lua.set_interrupt(move |_| {
        if hook.exceeded() {
            Err(mlua::Error::runtime(TIME_BUDGET_MARKER))
        } else {
            Ok(VmState::Continue)
        }
    });
    let _ = limits;
    Ok((lua, clock))
}

/// Freeze the built-in libraries and globals (scripts get their own
/// writable environment on top) and apply the memory ceiling.
fn seal(lua: &Lua, limits: Limits) -> Result<(), ScriptError> {
    let setup = |e: mlua::Error| ScriptError::Setup(e.to_string());
    lua.sandbox(true).map_err(setup)?;
    lua.set_memory_limit(limits.memory_bytes).map_err(setup)?;
    Ok(())
}

fn classify(e: mlua::Error) -> ScriptError {
    if matches!(e, mlua::Error::MemoryError(_)) {
        return ScriptError::Budget(Budget::Memory);
    }
    let text = e.to_string();
    if text.contains(TIME_BUDGET_MARKER) {
        ScriptError::Budget(Budget::Time)
    } else if let Some(rest) = text.split(HOST_ERROR_MARKER).nth(1) {
        ScriptError::Script(format!(
            "host call failed:{}",
            rest.lines().next().unwrap_or_default()
        ))
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
            ..Limits::default()
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

    /// A host that answers `daemon.version`, waits `delay` first, and
    /// refuses everything else like a narrow grant would.
    struct Fake {
        delay: Duration,
        calls: std::cell::RefCell<Vec<(String, serde_json::Value)>>,
    }

    impl Host for Rc<Fake> {
        fn call(&self, method: String, params: serde_json::Value) -> HostFuture {
            self.calls.borrow_mut().push((method.clone(), params));
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                if method == "daemon.version" {
                    Ok(serde_json::json!({ "protocol_major": 1, "daemon_version": "x" }))
                } else {
                    Err(HostError {
                        code: api::code::FORBIDDEN,
                        message: format!("`{method}` is not in this connection's grant"),
                    })
                }
            })
        }
    }

    fn hosted(delay: Duration, limits: Limits) -> (Script, Rc<Fake>) {
        let fake = Rc::new(Fake {
            delay,
            calls: std::cell::RefCell::default(),
        });
        let s = Script::with_host(limits, Rc::new(fake.clone())).expect("vm");
        (s, fake)
    }

    #[tokio::test]
    async fn host_calls_yield_and_waiting_is_not_running_time() {
        // Each call waits 120 ms; the script may run only 50 ms.
        let (s, fake) = hosted(Duration::from_millis(120), Limits::default());
        s.run(
            "calls",
            "local v = dreamer.daemon.version() \
             local w = dreamer.daemon.version({}) \
             seen = v.protocol_major + w.protocol_major",
        )
        .await
        .expect("waiting does not count against the budget");
        assert_eq!(
            s.exec("t", "return seen").expect("seen"),
            Value::Number(2.0)
        );
        assert_eq!(fake.calls.borrow().len(), 2);
        assert_eq!(fake.calls.borrow()[0].1, serde_json::json!({}));
    }

    #[tokio::test]
    async fn host_errors_and_deadlines_reach_the_script() {
        let (s, _) = hosted(
            Duration::from_millis(300),
            Limits {
                host_call: Duration::from_millis(50),
                ..Limits::default()
            },
        );
        let err = s
            .run("late", "dreamer.daemon.version()")
            .await
            .expect_err("deadline");
        assert!(err.to_string().contains("no answer within 50 ms"), "{err}");
        let (s, _) = hosted(Duration::ZERO, Limits::default());
        s.run(
            "refused",
            "local ok, e = pcall(dreamer.daemon.shutdown) \
             refused = not ok and string.find(tostring(e), 'not in this connection', 1, true) ~= nil",
        )
        .await
        .expect("a refusal is an error the script can catch");
        assert_eq!(
            s.exec("t", "return refused").expect("r"),
            Value::Boolean(true)
        );
        let err = s
            .run("uncaught", "dreamer.daemon.shutdown()")
            .await
            .expect_err("uncaught");
        assert!(err.to_string().contains("-32005"), "{err}");
    }

    #[tokio::test]
    async fn a_runaway_after_a_host_call_hits_the_budget() {
        let (s, _) = hosted(Duration::from_millis(10), Limits::default());
        let err = s
            .run("spin", "dreamer.daemon.version() while true do end")
            .await
            .expect_err("budget");
        assert!(matches!(err, ScriptError::Budget(Budget::Time)), "{err}");
    }

    #[tokio::test]
    async fn the_module_is_the_only_way_out_and_it_is_frozen() {
        let (s, fake) = hosted(Duration::ZERO, Limits::default());
        for (name, src) in [
            ("replace", "dreamer.daemon = {}"),
            ("rawset", "rawset(dreamer.daemon, 'version', print)"),
            ("unknown", "dreamer.nothing.here()"),
        ] {
            assert!(s.run(name, src).await.is_err(), "{name}");
        }
        assert!(fake.calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn handlers_receive_and_return_records() {
        let (s, _) = hosted(Duration::ZERO, Limits::default());
        s.run(
            "h",
            "function on_event(e) \
               local v = dreamer.daemon.version() \
               return { seen = e.kind, protocol = v.protocol_major } \
             end",
        )
        .await
        .expect("load");
        let out = s
            .handle("on_event", &serde_json::json!({ "kind": "tmux.topology" }))
            .await
            .expect("handler");
        assert_eq!(
            out,
            Some(serde_json::json!({ "seen": "tmux.topology", "protocol": 1 }))
        );
        assert_eq!(
            s.handle("on_nothing", &serde_json::json!({}))
                .await
                .expect("none"),
            None
        );
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
