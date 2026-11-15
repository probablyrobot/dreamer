//! Where a view's daemon calls go: the local daemon's own tmux, or a
//! remote host through the local daemon's coordinator (`hosts.*`).
//!
//! The connection is always to the local daemon. For a host, unary calls
//! are forwarded with `hosts.call`, a pane is followed with
//! `hosts.subscribe`, and the topology with `hosts.watch`; the replies and
//! pushed events have the same shape either way, so everything above this
//! module is the same for local and remote panes.

use api::{Request, Response};
use client::{Client, ClientError};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum Via {
    /// The local daemon's tmux.
    #[default]
    Local,
    /// A host connected through the local daemon, by its name.
    Host(String),
}

fn unexpected() -> ClientError {
    ClientError::Wire(wire::WireError::Unexpected("a reply to another method"))
}

impl Via {
    pub fn host(&self) -> Option<&str> {
        match self {
            Via::Local => None,
            Via::Host(h) => Some(h),
        }
    }

    /// One unary call.
    pub async fn call(&self, c: &Client, request: Request) -> Result<Response, ClientError> {
        let Via::Host(name) = self else {
            return c.call(request).await;
        };
        let method = request.method();
        let Response::HostsCall(reply) = c
            .call(Request::HostsCall(api::HostCall {
                name: name.clone(),
                method: method.to_owned(),
                payload: request.encode(),
            }))
            .await?
        else {
            return Err(unexpected());
        };
        Response::decode(method, &reply.payload).map_err(|e| ClientError::Rpc(e.into()))
    }

    /// Follow a pane: its events then arrive on `c` as for a local pane.
    pub async fn subscribe(
        &self,
        c: &Client,
        params: api::TerminalSubscribe,
    ) -> Result<api::Subscription, ClientError> {
        let request = match self {
            Via::Local => Request::TerminalSubscribe(params),
            Via::Host(name) => Request::HostsSubscribe(api::HostSubscribe {
                name: name.clone(),
                subscribe: Some(params),
            }),
        };
        match c.call(request).await? {
            Response::TerminalSubscribe(s) | Response::HostsSubscribe(s) => Ok(s),
            _ => Err(unexpected()),
        }
    }

    /// The topology now; `tmux.topology` events follow on `c`.
    pub async fn watch(&self, c: &Client) -> Result<api::TopologyUpdate, ClientError> {
        let request = match self {
            Via::Local => Request::TmuxWatch(api::TopologyParams::default()),
            Via::Host(name) => Request::HostsWatch(api::HostRef { name: name.clone() }),
        };
        match c.call(request).await? {
            Response::TmuxWatch(u) | Response::HostsWatch(u) => Ok(u),
            _ => Err(unexpected()),
        }
    }

    /// Paste, split into parts the daemon reassembles (as
    /// [`client::paste`], through this route).
    pub async fn paste(
        &self,
        c: &Client,
        pane: u32,
        generation: Option<u64>,
        data: &[u8],
    ) -> Result<(), ClientError> {
        if *self == Via::Local {
            return client::paste(c, pane, generation, data).await.map(drop);
        }
        const PART: usize = 512 << 10;
        let id = rand_id();
        let parts: Vec<&[u8]> = if data.is_empty() {
            vec![&[][..]]
        } else {
            data.chunks(PART).collect()
        };
        for (i, part) in parts.iter().enumerate() {
            let req = api::TerminalInput {
                pane,
                generation,
                endpoint: None,
                client: None,
                lease: None,
                event: Some(api::terminal_input::Event::Paste(api::PasteInput {
                    data: part.to_vec(),
                    more: i + 1 < parts.len(),
                    id,
                })),
            };
            self.call(c, Request::TerminalInput(req)).await?;
        }
        Ok(())
    }
}

fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32) ^ N.fetch_add(1, Ordering::Relaxed)
}
