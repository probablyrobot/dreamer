//! The app host's TypeScript runtime: a script app's `app.ts`, its types
//! stripped by the CLI, run in QuickJS (`rquickjs`). It is one client of
//! the app process protocol: the host (`app_model::run_process`) talks to
//! it over channels as it talks to a program over pipes, so an app's
//! calls pass the host's one capability check, and its view is published
//! the same way (the whole tree, then keyed patches).
//!
//! - **Its own thread.** Each app runs on a thread of its own, never on
//!   the pane's input and drawing loop.
//! - **Limits.** Every entry into the app (starting it, a handler, an
//!   answer, a timer, drawing) has a deadline; code still running at it is
//!   interrupted, and the app stops, its last view kept and marked stale.
//!   The app's heap has a memory limit, and its stack a size limit.
//! - **Globals.** The app sees `app`, `ui`, `dreamer`, the timers,
//!   `console`, and `fetch` (`prelude.js`), and the language's own
//!   built-ins. There is no filesystem, process, or module loader: the
//!   only module an app can import is `dreamer`.
//! - **`fetch`** reaches only the hosts the manifest lists, and only when
//!   the app's grant holds `net.fetch`, as the host learns it from the
//!   daemon ([`fetch`], `app_model::NetGrant`). Each request runs on a thread of
//!   its own with a time and size limit; its answer enters the app like a
//!   call's, under the same per-entry deadline.
//! - **Focus and controls.** The host moves the focus and keeps each
//!   control's value ([`controls`]); the app's handlers hear about it.

mod controls;
mod fetch;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_model::{EngineOut, EngineStart};
use app_protocol::{AppMessage, HostMessage, PROTOCOL};
use rquickjs::{
    CatchResultExt, Context, Ctx, Exception, Function, Module, Object, Persistent, Runtime, Value,
};
use tokio::sync::mpsc;
use view_tree::Frame;

pub use controls::{Act, Controls};

/// How long one entry into the app may run.
pub const CALL_LIMIT: Duration = Duration::from_millis(250);
/// The most memory an app's heap may hold.
pub const MEMORY_LIMIT: usize = 64 << 20;
/// The most stack an app's code may use.
const STACK_LIMIT: usize = 1 << 20;
/// The most timers an app may have pending.
const TIMERS: usize = 1000;
/// How long one `fetch` may take, redirects and the body included.
pub const FETCH_TIME: Duration = Duration::from_secs(30);
/// The largest response body (and request body) a `fetch` takes.
pub const FETCH_SIZE: usize = 8 << 20;
/// The most requests an app may have in flight.
const FETCHES: usize = 16;

const PRELUDE: &str = include_str!("prelude.js");

/// The module an app imports its globals from.
const DREAMER_MODULE: &str = "export const app = globalThis.app;
export const ui = globalThis.ui;
export const dreamer = globalThis.dreamer;
";

/// An app's limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub call: Duration,
    pub memory: usize,
    /// How long one `fetch` may take.
    pub fetch_time: Duration,
    /// The largest body one `fetch` sends or takes.
    pub fetch_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            call: CALL_LIMIT,
            memory: MEMORY_LIMIT,
            fetch_time: FETCH_TIME,
            fetch_size: FETCH_SIZE,
        }
    }
}

/// The engine the app host gives `app_model::run_process`.
pub fn engine() -> app_model::Engine {
    engine_with(Limits::default())
}

/// The engine, with other limits.
pub fn engine_with(limits: Limits) -> app_model::Engine {
    Arc::new(move |start| spawn(start, limits))
}

/// Whether `js` (named `name`) compiles as a module: its syntax error, at
/// its line and column, if not.
pub fn check(name: &str, js: &str) -> Result<(), String> {
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    rt.set_memory_limit(MEMORY_LIMIT);
    let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
    ctx.with(|ctx| {
        Module::declare(ctx.clone(), "dreamer", DREAMER_MODULE)
            .and_then(|_| Module::declare(ctx.clone(), name, js))
            .map(|_| ())
            .catch(&ctx)
            .map_err(|e| one_error(&e.to_string()))
    })
}

/// An engine error as one line: the message and the first place in the
/// app's stack.
fn one_error(e: &str) -> String {
    let mut lines = e.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines
        .next()
        .unwrap_or("an error")
        .trim_start_matches("Error: ");
    match lines.next() {
        Some(at) if at.starts_with("at ") => format!("{first} ({at})"),
        _ => first.to_owned(),
    }
}

/// Run the app on a thread of its own.
fn spawn(start: EngineStart, limits: Limits) -> Result<(), String> {
    std::thread::Builder::new()
        .name(format!("app {}", start.app.id))
        .stack_size(8 << 20)
        .spawn(move || {
            let output = start.output.clone();
            let end = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt.block_on(run(start, limits)),
                Err(e) => Err(e.to_string()),
            };
            let _ = match end {
                Ok(End::Closed) => Ok(()),
                Ok(End::Quit) => output.send(EngineOut::Quit),
                Err(why) => output.send(EngineOut::Failed(why)),
            };
        })
        .map(|_| ())
        .map_err(|e| format!("could not start the app's thread: {e}"))
}

enum End {
    /// The host closed the app's input.
    Closed,
    /// The user quit.
    Quit,
}

/// What the app's code asked of the host during a turn.
enum Native {
    Call(u64, String, String),
    Fetch(u64, fetch::Request),
    Log(String),
    Fatal(String),
}

/// The app's context and the host's side of it.
struct Js {
    // Dropped in order: the hooks, then the context, then the runtime.
    hooks: Option<Persistent<Object<'static>>>,
    ctx: Context,
    _rt: Runtime,
    deadline: Rc<Cell<Option<Instant>>>,
    tripped: Rc<Cell<bool>>,
    queue: Rc<RefCell<Vec<Native>>>,
    timers: Rc<RefCell<HashMap<u64, Instant>>>,
    /// Requests sent and not yet answered.
    fetching: Rc<Cell<usize>>,
    client: fetch::Client,
    limits: Limits,
}

impl Drop for Js {
    fn drop(&mut self) {
        if let Some(h) = self.hooks.take() {
            self.ctx.with(|_| drop(h));
        }
    }
}

impl Js {
    fn new(
        limits: Limits,
        app: &app_protocol::AppInfo,
        methods: &[String],
        net: fetch::Net,
        fixture: Option<&str>,
    ) -> Result<Js, String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(limits.memory);
        rt.set_max_stack_size(STACK_LIMIT);
        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let tripped = Rc::new(Cell::new(false));
        {
            let (deadline, tripped) = (deadline.clone(), tripped.clone());
            rt.set_interrupt_handler(Some(Box::new(move || {
                let over = deadline.get().is_some_and(|d| Instant::now() >= d);
                if over {
                    tripped.set(true);
                }
                over
            })));
        }
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        let mut js = Js {
            hooks: None,
            ctx,
            _rt: rt,
            deadline,
            tripped,
            queue: Rc::new(RefCell::new(Vec::new())),
            timers: Rc::new(RefCell::new(HashMap::new())),
            fetching: Rc::new(Cell::new(0)),
            client: fetch::Client::new(net, limits.fetch_time, limits.fetch_size),
            limits,
        };
        let hooks = js.within("starting the app", |ctx| {
            let host = Object::new(ctx.clone())?;
            let q = js.queue.clone();
            host.set(
                "call",
                Function::new(
                    ctx.clone(),
                    move |id: f64, method: String, params: String| {
                        q.borrow_mut().push(Native::Call(id as u64, method, params));
                    },
                )?,
            )?;
            let t = js.timers.clone();
            host.set(
                "timer",
                Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>, id: f64, ms: f64| -> rquickjs::Result<()> {
                        let mut t = t.borrow_mut();
                        if t.len() >= TIMERS && !t.contains_key(&(id as u64)) {
                            return Err(Exception::throw_message(
                                &ctx,
                                &format!("more than {TIMERS} timers"),
                            ));
                        }
                        let ms = if ms.is_finite() {
                            ms.clamp(0.0, 1e9)
                        } else {
                            0.0
                        };
                        t.insert(id as u64, Instant::now() + Duration::from_millis(ms as u64));
                        Ok(())
                    },
                )?,
            )?;
            let t = js.timers.clone();
            host.set(
                "cancel",
                Function::new(ctx.clone(), move |id: f64| {
                    t.borrow_mut().remove(&(id as u64));
                })?,
            )?;
            // A request: refused here (an error to reject with) when the
            // app may not send it, else queued for a thread of its own.
            let (q, fetching, client) = (js.queue.clone(), js.fetching.clone(), js.client.clone());
            host.set(
                "fetch",
                Function::new(
                    ctx.clone(),
                    move |id: f64,
                          url: String,
                          method: String,
                          headers: String,
                          body: Option<String>,
                          redirect: String|
                          -> Option<String> {
                        if fetching.get() >= FETCHES {
                            return Some(format!("fetch: more than {FETCHES} requests in flight"));
                        }
                        match fetch::Request::new(
                            client.net(),
                            &url,
                            &method,
                            &headers,
                            body,
                            &redirect,
                            client.size(),
                        ) {
                            Ok(req) => {
                                fetching.set(fetching.get() + 1);
                                q.borrow_mut().push(Native::Fetch(id as u64, req));
                                None
                            }
                            Err(why) => Some(why),
                        }
                    },
                )?,
            )?;
            let q = js.queue.clone();
            host.set(
                "log",
                Function::new(ctx.clone(), move |text: String| {
                    q.borrow_mut().push(Native::Log(text));
                })?,
            )?;
            let q = js.queue.clone();
            host.set(
                "fatal",
                Function::new(ctx.clone(), move |text: String| {
                    q.borrow_mut().push(Native::Fatal(text));
                })?,
            )?;
            let info = Object::new(ctx.clone())?;
            info.set("id", app.id.as_str())?;
            info.set("version", app.version.as_str())?;
            info.set("title", app.title.as_str())?;
            let fixture = match fixture {
                Some(text) => ctx.json_parse(text)?,
                None => Value::new_null(ctx.clone()),
            };
            let prelude: Function = ctx.eval(PRELUDE)?;
            let hooks: Object = prelude.call((host, methods.to_vec(), info, fixture))?;
            Ok(Persistent::save(ctx, hooks))
        })?;
        js.hooks = Some(hooks);
        Ok(js)
    }

    /// Run `f` in the app's context under the deadline, then the jobs it
    /// queued (promise reactions). An interrupt, or what the app's code
    /// cannot recover from, is an error.
    fn within<R>(
        &self,
        what: &str,
        f: impl for<'js> FnOnce(&Ctx<'js>) -> rquickjs::Result<R>,
    ) -> Result<R, String> {
        self.tripped.set(false);
        self.deadline.set(Some(Instant::now() + self.limits.call));
        let r = self.ctx.with(|ctx| {
            let r = f(&ctx).catch(&ctx).map_err(|e| one_error(&e.to_string()));
            while !self.tripped.get() && ctx.execute_pending_job() {}
            r
        });
        self.deadline.set(None);
        if self.tripped.get() {
            return Err(format!(
                "{what} ran longer than {} ms and was stopped",
                self.limits.call.as_millis()
            ));
        }
        if let Some(why) = self.queue.borrow().iter().find_map(|n| match n {
            Native::Fatal(why) => Some(why.clone()),
            _ => None,
        }) {
            return Err(format!("{what}: {why}"));
        }
        r.map_err(|e| format!("{what}: {e}"))
    }

    /// Call hook `name` with `args`.
    fn hook<A, R>(&self, what: &str, name: &str, args: A) -> Result<R, String>
    where
        A: for<'js> rquickjs::function::IntoArgs<'js>,
        R: for<'js> rquickjs::FromJs<'js>,
    {
        let hooks = self.hooks.as_ref().ok_or("the app is not running")?;
        self.within(what, move |ctx| {
            let hooks = hooks.clone().restore(ctx)?;
            let f: Function = hooks.get(name)?;
            f.call(args)
        })
    }

    /// Evaluate the app's code as a module that may import `dreamer`.
    fn evaluate(&self, name: &str, js: &str) -> Result<(), String> {
        self.within("the app's code", |ctx| {
            Module::declare(ctx.clone(), "dreamer", DREAMER_MODULE)?;
            let done = Module::evaluate(ctx.clone(), name, js)?;
            while ctx.execute_pending_job() {}
            match done.finish::<()>() {
                Err(rquickjs::Error::WouldBlock) => Ok(()),
                r => r,
            }
        })
    }

    fn take_queue(&self) -> Vec<Native> {
        std::mem::take(&mut *self.queue.borrow_mut())
    }

    /// When the next timer is due.
    fn next_timer(&self) -> Option<Instant> {
        self.timers.borrow().values().min().copied()
    }

    /// The timers due now, soonest first; they leave the table.
    fn due(&self) -> Vec<u64> {
        let now = Instant::now();
        let mut t = self.timers.borrow_mut();
        let mut due: Vec<(Instant, u64)> = t
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, at)| (*at, *id))
            .collect();
        due.sort();
        for (_, id) in &due {
            t.remove(id);
        }
        due.into_iter().map(|(_, id)| id).collect()
    }
}

/// The host's hello: the methods the app may call, and whether the host
/// is connected. `None` when the host went away first.
async fn hello(input: &mut mpsc::Receiver<String>) -> Result<Option<(Vec<String>, bool)>, String> {
    loop {
        let Some(line) = input.recv().await else {
            return Ok(None);
        };
        if line.trim().is_empty() {
            continue;
        }
        return match serde_json::from_str::<HostMessage>(&line) {
            Ok(HostMessage::Hello {
                protocol,
                methods,
                live,
                ..
            }) if protocol == PROTOCOL => Ok(Some((methods, live))),
            Ok(HostMessage::Hello { protocol, .. }) => Err(format!(
                "the host speaks protocol {protocol}; this runtime speaks {PROTOCOL}"
            )),
            _ => Err("the host did not start with a hello".into()),
        };
    }
}

/// The API methods the capabilities `methods` let an app call, by name:
/// what `dreamer.*` holds. The host checks every call again.
fn granted(methods: &[String]) -> Vec<String> {
    api::METHODS
        .iter()
        .map(|m| m.name.to_owned())
        .filter(|m| app_model::allowed(methods, m).is_ok())
        .collect()
}

async fn run(start: EngineStart, limits: Limits) -> Result<End, String> {
    let EngineStart {
        app,
        name,
        js: source,
        fixture,
        hosts,
        net,
        mut input,
        output,
    } = start;
    let send = |m: &AppMessage| {
        let line = app_protocol::line(m);
        let _ = output.send(EngineOut::Line(line.trim_end().to_owned()));
    };
    let Some((methods, live)) = hello(&mut input).await? else {
        return Ok(End::Closed);
    };
    send(&AppMessage::Hello {
        protocol: PROTOCOL,
        name: Some(format!("app.ts runtime {}", env!("CARGO_PKG_VERSION"))),
    });
    let net = fetch::Net::new(net, &hosts);
    let js = Js::new(limits, &app, &granted(&methods), net, fixture.as_deref())?;
    // Answers to requests, from their threads.
    let (fetched_tx, mut fetched) = mpsc::unbounded_channel();
    js.evaluate(&name, &source)?;
    js.hook::<_, ()>("starting the app", "start", (live,))?;
    let mut controls = Controls::default();
    let mut drawn: Option<Frame> = None;
    let mut shown: Option<Frame> = None;
    let mut out = view_tree::Outgoing::default();
    let mut reshape = false;
    loop {
        for n in js.take_queue() {
            match n {
                Native::Call(id, method, params) => send(&AppMessage::Call {
                    id,
                    method,
                    params: serde_json::from_str(&params).unwrap_or_default(),
                }),
                Native::Fetch(id, req) => {
                    let (client, done) = (js.client.clone(), fetched_tx.clone());
                    let ran = std::thread::Builder::new()
                        .name(format!("fetch {id}"))
                        .spawn(move || {
                            let _ = done.send((id, client.fetch(req)));
                        });
                    if let Err(e) = ran {
                        let _ = fetched_tx.send((id, Err(format!("fetch: no thread for it: {e}"))));
                    }
                }
                Native::Log(line) => {
                    let _ = output.send(EngineOut::Log(line));
                }
                Native::Fatal(_) => {}
            }
        }
        if let Some(json) = js.hook::<_, Option<String>>("drawing the view", "render", ())? {
            let f: Frame = serde_json::from_str(&json).map_err(|e| format!("the view: {e}"))?;
            f.check().map_err(|e| format!("the view: {e}"))?;
            drawn = Some(f);
            reshape = true;
        }
        if reshape && let Some(d) = &drawn {
            let s = controls.shape(d);
            match out.next(&s) {
                Some(view_tree::Update::Full { frame, .. }) => {
                    send(&AppMessage::View { tree: frame });
                }
                Some(view_tree::Update::Patch { ops, .. }) => send(&AppMessage::Patch { ops }),
                None => {}
            }
            shown = Some(s);
            reshape = false;
        }
        let timer = js.next_timer();
        let wait = async {
            match timer {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => std::future::pending().await,
            }
        };
        let line = tokio::select! {
            l = input.recv() => match l {
                Some(l) => l,
                None => return Ok(End::Closed),
            },
            () = wait => {
                for id in js.due() {
                    js.hook::<_, ()>("a timer", "fire", (id as f64,))?;
                }
                continue;
            }
            Some((id, answer)) = fetched.recv() => {
                js.fetching.set(js.fetching.get().saturating_sub(1));
                let (head, body, error) = match answer {
                    Ok(f) => (Some(f.head_json()), Some(f.body), None),
                    Err(why) => (None, None, Some(why)),
                };
                js.hook::<_, ()>("a fetch's answer", "fetched", (id as f64, head, body, error))?;
                continue;
            }
        };
        let Ok(msg) = serde_json::from_str::<HostMessage>(&line) else {
            continue;
        };
        match msg {
            HostMessage::Hello { .. } => {}
            HostMessage::Quit => return Ok(End::Closed),
            HostMessage::Resync { .. } => {
                out.resync();
                reshape = true;
            }
            HostMessage::Link { live, .. } => js.hook::<_, ()>("the link", "link", (live,))?,
            HostMessage::Result { id, result, error } => {
                let (ok, message, code) = match error {
                    Some(e) => (None, Some(e.message), e.code),
                    None => (Some(result.unwrap_or_default().to_string()), None, 0),
                };
                js.hook::<_, ()>("a call's answer", "result", (id as f64, ok, message, code))?;
            }
            HostMessage::Notify { topic, payload } => {
                js.hook::<_, ()>("an event", "notify", (topic, payload.to_string()))?;
            }
            HostMessage::Event { event } => {
                let Some(s) = &shown else { continue };
                let acts = controls.on(s, event);
                reshape = true;
                for act in acts {
                    match act {
                        Act::Quit => return Ok(End::Quit),
                        Act::Key(k) => js.hook::<_, ()>("a key handler", "key", (k,))?,
                        Act::Component {
                            node,
                            action,
                            value,
                        } => {
                            let value = (!value.is_null()).then(|| value.to_string());
                            js.hook::<_, bool>("a handler", "event", (node, action, value))?;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
