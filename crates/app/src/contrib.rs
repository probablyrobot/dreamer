//! Contribution points: what installed app packages add to
//! the desktop. Palette commands (with optional keybindings), sidebar
//! entries, and compact status items. The desktop places and draws them;
//! a package only declares them.
//!
//! A thread of its own reads `<config dir>/apps` (again when a manifest
//! changes), refreshes status items, and runs contributed commands. Each
//! package gets one connection that keeps only the package's declared
//! capabilities, so a contribution can do no more than its package
//! declares. Nothing from a package runs on the UI thread.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_model::installed::{Installed, Package};
use app_model::{Endpoint, Session};

/// How often status items are read again.
const STATUS_EVERY: Duration = Duration::from_secs(2);

/// What the view shows.
#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    pub packages: Vec<Package>,
    pub problems: Vec<String>,
    /// Status item text by `<package id>.<item id>`.
    pub status: BTreeMap<String, String>,
    /// The last contributed command's outcome.
    pub outcome: Option<String>,
}

impl Snapshot {
    /// Contributed palette commands: (package index, command index), in
    /// package order.
    pub(crate) fn commands(&self) -> Vec<(usize, usize)> {
        self.packages
            .iter()
            .enumerate()
            .flat_map(|(p, pkg)| (0..pkg.manifest.contributes.commands.len()).map(move |c| (p, c)))
            .collect()
    }
}

#[derive(Debug)]
enum Request {
    /// Run `<package id>`'s command `run`, titled for the outcome.
    Run {
        package: String,
        run: String,
        title: String,
    },
}

#[derive(Debug)]
pub(crate) struct Contributions {
    latest: Arc<Mutex<Snapshot>>,
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    pub wake: async_channel::Receiver<()>,
}

impl Contributions {
    /// Follow the packages in `apps` (if any), talking to the daemon at
    /// `socket`. `taken` are the desktop's own keybindings.
    pub(crate) fn start(apps: Option<PathBuf>, socket: PathBuf, taken: Vec<&'static str>) -> Self {
        let latest: Arc<Mutex<Snapshot>> = Arc::default();
        let (requests, rx) = tokio::sync::mpsc::unbounded_channel();
        let (wake_tx, wake) = async_channel::bounded(1);
        let l = latest.clone();
        let _ = std::thread::Builder::new()
            .name("dreamer-contrib".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(run(apps, socket, taken, l, wake_tx, rx));
            });
        Contributions {
            latest,
            requests,
            wake,
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.latest.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Run a package's contributed command (by package id and command).
    pub(crate) fn run(&self, package: &str, run: &str, title: &str) {
        let _ = self.requests.send(Request::Run {
            package: package.to_owned(),
            run: run.to_owned(),
            title: title.to_owned(),
        });
    }
}

async fn run(
    apps: Option<PathBuf>,
    socket: PathBuf,
    taken: Vec<&'static str>,
    latest: Arc<Mutex<Snapshot>>,
    wake: async_channel::Sender<()>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
) {
    let endpoint = Endpoint {
        socket,
        role: "contrib",
        ticket: false,
        narrow: true,
    };
    let mut stamps = None;
    let mut installed = Installed::default();
    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut status_due = Instant::now();
    loop {
        let request = tokio::select! {
            _ = tick.tick() => None,
            r = requests.recv() => match r {
                Some(r) => Some(r),
                None => return,
            },
        };
        let mut changed = false;
        if let Some(dir) = &apps {
            let now = app_model::installed::stamps(dir);
            if stamps.as_ref() != Some(&now) {
                installed = app_model::installed::scan(dir, &taken);
                for p in &installed.problems {
                    eprintln!("dreamer: app package: {p}");
                }
                // A changed package may declare other capabilities.
                sessions.clear();
                stamps = Some(now);
                status_due = Instant::now();
                if let Ok(mut s) = latest.lock() {
                    s.packages = installed.packages.clone();
                    s.problems = installed.problems.clone();
                    s.status.clear();
                }
                changed = true;
            }
        }
        if let Some(Request::Run {
            package,
            run,
            title,
        }) = request
        {
            let said = match installed.packages.iter().find(|p| p.manifest.id == package) {
                None => format!("{title}: the package is no longer installed"),
                Some(p) => match session(&mut sessions, &endpoint, p).await {
                    Err(e) => format!("{title}: {e}"),
                    Ok(s) => match s.command(&p.manifest, &run).await {
                        Ok(()) => format!("{title}: done"),
                        Err((gone, e)) => {
                            if gone {
                                sessions.remove(&package);
                            }
                            format!("{title}: {e}")
                        }
                    },
                },
            };
            if let Ok(mut s) = latest.lock() {
                s.outcome = Some(said);
            }
            status_due = Instant::now();
            changed = true;
        }
        if Instant::now() >= status_due {
            status_due = Instant::now() + STATUS_EVERY;
            let mut values = BTreeMap::new();
            for p in &installed.packages {
                if p.manifest.contributes.status.is_empty() {
                    continue;
                }
                let id = p.manifest.id.clone();
                for item in &p.manifest.contributes.status {
                    let key = format!("{id}.{}", item.id);
                    let text = match session(&mut sessions, &endpoint, p).await {
                        Err(e) => format!("({e})"),
                        Ok(s) => match s.source(&p.manifest, &item.source).await {
                            Ok(rows) => match &item.field {
                                None => rows.len().to_string(),
                                Some(f) => rows
                                    .first()
                                    .and_then(|r| app_model::manifest::field(r, f))
                                    .map(app_model::text)
                                    .unwrap_or_else(|| "–".into()),
                            },
                            Err((gone, e)) => {
                                if gone {
                                    sessions.remove(&id);
                                }
                                format!("({e})")
                            }
                        },
                    };
                    let text = match &item.label {
                        Some(l) => format!("{l} {text}"),
                        None => text,
                    };
                    values.insert(key, text);
                }
            }
            if let Ok(mut s) = latest.lock()
                && s.status != values
            {
                s.status = values;
                changed = true;
            }
        }
        if changed {
            let _ = wake.try_send(());
        }
    }
}

/// The package's connection, opened (and narrowed) on first use.
async fn session<'a>(
    sessions: &'a mut BTreeMap<String, Session>,
    endpoint: &Endpoint,
    p: &Package,
) -> Result<&'a Session, String> {
    let id = p.manifest.id.clone();
    if !sessions.contains_key(&id) {
        let s = Session::open(endpoint, &p.manifest).await?;
        sessions.insert(id.clone(), s);
    }
    sessions.get(&id).ok_or_else(|| "no connection".into())
}
