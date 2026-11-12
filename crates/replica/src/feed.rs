//! A pane feed for a view (spec 9.5, 16.2): a dedicated owner thread keeps
//! a replica current and publishes frames to whoever paints them.
//!
//! - A frame is built only when records were applied or a checkpoint was
//!   restored. An idle pane costs no wakeups: the owner waits on the
//!   connection with no timeout and the view waits on its wake channel.
//! - Frames coalesce: the newest replaces any unpainted one in a
//!   latest-value slot, and the wake channel holds at most one signal.
//! - A synchronized-output hold delays publishing for at most
//!   [`HOLD_LIMIT`]; then the newest frame is shown anyway, so a broken
//!   application cannot freeze the view.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use terminal_ghostty::core::FrameSnapshot;

use crate::{FollowError, FollowState, Follower, RetryPolicy};

pub const HOLD_LIMIT: Duration = Duration::from_secs(1);

/// Where a feed stands, for the view to show honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedStatus {
    Connecting,
    Following,
    /// Lost the daemon; retrying.
    Reconnecting(String),
    PaneClosed,
    UnableToCatchUp(String),
}

#[derive(Debug, Default)]
pub struct FeedStats {
    /// Times the owner thread woke to process data.
    pub owner_wakeups: AtomicU64,
    /// Frames built from the engine.
    pub frames: AtomicU64,
    /// Wake signals sent to the view (coalesced).
    pub view_wakeups: AtomicU64,
    /// Holds that hit the limit and were shown anyway.
    pub holds_expired: AtomicU64,
}

#[derive(Debug)]
pub struct Feed {
    latest: Arc<Mutex<Option<Arc<FrameSnapshot>>>>,
    status: Arc<Mutex<FeedStatus>>,
    pub stats: Arc<FeedStats>,
    /// One signal means "a newer frame is waiting"; never more than one.
    pub wake: async_channel::Receiver<()>,
}

impl Feed {
    /// Follow `pane` through the daemon at `socket` on a new owner thread.
    /// With `launch`, the daemon is started first if it is not running.
    pub fn start(
        socket: PathBuf,
        pane: u32,
        launch: Option<client::Launch>,
    ) -> std::io::Result<Feed> {
        let latest = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(FeedStatus::Connecting));
        let stats = Arc::new(FeedStats::default());
        let (wake_tx, wake) = async_channel::bounded(1);
        let owner = Owner {
            latest: latest.clone(),
            status: status.clone(),
            stats: stats.clone(),
            wake: wake_tx,
        };
        std::thread::Builder::new()
            .name(format!("dreamer-pane-{pane}"))
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    owner.set(FeedStatus::UnableToCatchUp("no runtime".into()));
                    return;
                };
                rt.block_on(owner.run(socket, pane, launch));
            })?;
        Ok(Feed {
            latest,
            status,
            stats,
            wake,
        })
    }

    /// The newest unpainted frame, if any.
    pub fn take(&self) -> Option<Arc<FrameSnapshot>> {
        self.latest.lock().ok().and_then(|mut l| l.take())
    }

    pub fn status(&self) -> FeedStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or(FeedStatus::Connecting)
    }
}

struct Owner {
    latest: Arc<Mutex<Option<Arc<FrameSnapshot>>>>,
    status: Arc<Mutex<FeedStatus>>,
    stats: Arc<FeedStats>,
    wake: async_channel::Sender<()>,
}

impl Owner {
    fn set(&self, s: FeedStatus) {
        if let Ok(mut st) = self.status.lock() {
            *st = s;
        }
    }

    fn publish(&self, frame: Arc<FrameSnapshot>) {
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(frame);
        }
        // Full means a wake is already pending: the view takes the newest.
        if self.wake.try_send(()).is_ok() {
            self.stats.view_wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn connect(&self, socket: &std::path::Path, pane: u32) -> Follower {
        let mut backoff = Duration::from_millis(100);
        loop {
            match Follower::start(socket.to_path_buf(), pane, RetryPolicy::default()).await {
                Ok(f) => return f,
                Err(e) => {
                    self.set(FeedStatus::Reconnecting(e.to_string()));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
        }
    }

    async fn run(self, socket: PathBuf, pane: u32, launch: Option<client::Launch>) {
        if let Some(l) = &launch
            && let Err(e) = client::ensure_running(l, "gui").await
        {
            self.set(FeedStatus::Reconnecting(e.to_string()));
        }
        let mut f = self.connect(&socket, pane).await;
        self.set(FeedStatus::Following);
        let mut held_since: Option<tokio::time::Instant> = None;
        loop {
            let step = match held_since {
                Some(t) => match tokio::time::timeout_at(t + HOLD_LIMIT, f.step()).await {
                    Ok(r) => r,
                    Err(_) => {
                        // The application held output too long: show it.
                        self.stats.holds_expired.fetch_add(1, Ordering::Relaxed);
                        held_since = None;
                        if let Some(Ok(frame)) = f.frame() {
                            self.publish(frame);
                        }
                        continue;
                    }
                },
                None => f.step().await,
            };
            self.stats.owner_wakeups.fetch_add(1, Ordering::Relaxed);
            match step {
                Ok(s) if s.records > 0 || s.restored => {
                    let Some(Ok(frame)) = f.frame() else { continue };
                    self.stats.frames.fetch_add(1, Ordering::Relaxed);
                    if frame.render_hold && !s.restored {
                        // Not shown yet; a later frame or the limit shows it.
                        held_since.get_or_insert_with(tokio::time::Instant::now);
                        continue;
                    }
                    held_since = None;
                    self.publish(frame);
                }
                Ok(_) => match &f.state {
                    FollowState::Following => {}
                    FollowState::PaneClosed => {
                        self.set(FeedStatus::PaneClosed);
                        return;
                    }
                    FollowState::UnableToCatchUp(why) => {
                        self.set(FeedStatus::UnableToCatchUp(why.clone()));
                        return;
                    }
                    FollowState::Disconnected => self.reconnect(&mut f).await,
                },
                Err(FollowError::Disconnected) => self.reconnect(&mut f).await,
                Err(e) => {
                    self.set(FeedStatus::Reconnecting(e.to_string()));
                    self.reconnect(&mut f).await;
                }
            }
        }
    }

    async fn reconnect(&self, f: &mut Follower) {
        let mut backoff = Duration::from_millis(100);
        loop {
            self.set(FeedStatus::Reconnecting(
                "the daemon connection ended".into(),
            ));
            tokio::time::sleep(backoff).await;
            if f.reconnect().await.is_ok() {
                self.set(FeedStatus::Following);
                return;
            }
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }
}
