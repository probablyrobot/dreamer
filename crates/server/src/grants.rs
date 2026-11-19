//! What a connection may do (spec 12.5): its grant, checked by the daemon
//! before every request. The user's own clients hold the owner grant;
//! registered pane apps hold a narrow one. Nothing a client sends (and no
//! terminal output, manifest, or model response) can widen a grant: only
//! the daemon assigns them.

use api::{Request, RpcError, code};

/// What a request touches, for the pane scope.
#[derive(Debug, PartialEq, Eq)]
enum Targets {
    /// Only metadata, or the caller's own state.
    Nothing,
    /// These panes.
    Panes(Vec<u32>),
    /// Every pane, a window or session, another host, or the daemon itself.
    Everything,
}

fn targets(r: &Request) -> Targets {
    use api::action::Kind;
    match r {
        Request::TerminalRead(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalSubscribe(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalInput(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalSetTier(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalWatch(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalRun(p) => match p.split {
            Some(pane) => Targets::Panes(vec![pane]),
            // A run without a pane to split gets a window of its own.
            None => Targets::Everything,
        },
        Request::LeasesAcquire(p) => Targets::Panes(vec![p.pane]),
        Request::TerminalQuery(p) if p.panes.is_empty() || !p.hosts.is_empty() => {
            Targets::Everything
        }
        Request::TerminalQuery(p) => Targets::Panes(p.panes.clone()),
        Request::DecorationsList(p) => Targets::Panes(vec![p.pane]),
        // Command lines can hold anything: every process needs every pane.
        Request::ProcessesList(p) if p.panes.is_empty() => Targets::Everything,
        Request::AgentsRuns(_) => Targets::Everything,
        Request::ProcessesList(p) => Targets::Panes(p.panes.clone()),
        // A job gets a window of its own; cancelling signals a process.
        Request::JobsStart(_) | Request::JobsCancel(_) => Targets::Everything,
        // An app beside a pane splits it; else it gets a window of its own.
        Request::AppsLaunch(p) => match p.split {
            Some(pane) => Targets::Panes(vec![pane]),
            None => Targets::Everything,
        },
        // A page beside a pane splits it; else it gets a window of its own.
        Request::WebOpen(p) => match p.pane {
            Some(pane) => Targets::Panes(vec![pane]),
            None => Targets::Everything,
        },
        Request::WebNavigate(p) => Targets::Panes(vec![p.pane]),
        Request::WebRead(p) => Targets::Panes(vec![p.pane]),
        Request::WebScreenshot(p) => Targets::Panes(vec![p.pane]),
        Request::WebClick(p) => Targets::Panes(vec![p.pane]),
        Request::WebType(p) => Targets::Panes(vec![p.pane]),
        Request::AppsTree(p) => Targets::Panes(vec![p.pane]),
        Request::AppsCapture(p) => Targets::Panes(vec![p.pane]),
        // A collector reads its panes for as long as it runs; one that
        // names only listed panes needs those, any other every pane.
        Request::CollectCreate(p) => match &p.from {
            Some(f)
                if !f.all
                    && f.session.is_none()
                    && f.window.is_none()
                    && f.command.is_none()
                    && !f.panes.is_empty() =>
            {
                Targets::Panes(f.panes.clone())
            }
            _ => Targets::Everything,
        },
        // Rows come from any pane a collector covers.
        Request::CollectList(_) | Request::CollectQuery(_) | Request::CollectStop(_) => {
            Targets::Everything
        }
        Request::TapsSubscribe(p) if p.every_pane => Targets::Everything,
        Request::TapsSubscribe(p) => Targets::Panes(p.panes.clone()),
        Request::ActionsSubmit(s) => match s.action.as_ref().and_then(|a| a.kind.as_ref()) {
            Some(Kind::SplitPane(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::ResizePane(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::SelectPane(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::ZoomPane(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::SendInput(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::SendKeys(a)) => Targets::Panes(vec![a.pane]),
            Some(Kind::KillPane(a)) => Targets::Panes(vec![a.pane]),
            // Windows and sessions hold panes beyond any pane scope.
            _ => Targets::Everything,
        },
        // A prefix binding is the user's own command: it can act anywhere.
        Request::TerminalMirrors(_)
        | Request::TmuxPrefixKey(_)
        | Request::DrawerToggle(_)
        | Request::DaemonShutdown(_)
        | Request::HostsConnect(_)
        | Request::HostsDisconnect(_)
        | Request::HostsReconnect(_)
        | Request::HostsCall(_)
        | Request::HostsSubscribe(_)
        | Request::HostsWatch(_)
        | Request::HostsInstallPlan(_)
        | Request::HostsInstall(_)
        | Request::HostsUpgrade(_)
        | Request::HostsForward(_)
        | Request::HostsUnforward(_)
        | Request::ForwardOpen(_)
        | Request::ForwardWrite(_)
        | Request::ForwardAck(_)
        | Request::ForwardClose(_) => Targets::Everything,
        _ => Targets::Nothing,
    }
}

/// Whether a call forwarded to a host opens or loads a page at that host's
/// loopback, which the desktop reaches through a forward.
fn opens_loopback(c: &api::HostCall) -> bool {
    let url = match Request::decode(&c.method, &c.payload) {
        Ok(Request::WebOpen(p)) => p.url,
        Ok(Request::WebNavigate(p)) => p.url,
        _ => return false,
    };
    let url = workspace_config::browser::web_address(&url).unwrap_or(url);
    workspace_config::browser::loopback_port(&url).is_some()
}

/// Whether `wanted` is within `current`: a connection may give up
/// authority, never gain it. The reason when it is not.
pub fn within(current: &api::Grant, wanted: &api::Grant) -> Result<(), String> {
    if current.owner {
        return Ok(());
    }
    if wanted.owner {
        return Err("the owner grant is not within this connection's grant".into());
    }
    let have = Grant(current.clone());
    if let Some(m) = wanted.methods.iter().find(|m| {
        // `service.*` is within only if the current grant has the whole
        // service too.
        !have.0.methods.contains(m)
            && !(m.ends_with(".*") && have.0.methods.contains(m))
            && !(!m.ends_with(".*") && have.method_allowed(m))
    }) {
        return Err(format!("`{m}` is not in this connection's grant"));
    }
    if wanted.all_panes && !current.all_panes {
        return Err("every pane is not in this connection's grant".into());
    }
    if !current.all_panes
        && let Some(p) = wanted.panes.iter().find(|p| !current.panes.contains(p))
    {
        return Err(format!("pane %{p} is outside this connection's grant"));
    }
    if let Some(t) = current.expires_unix_ms
        && wanted.expires_unix_ms.is_none_or(|w| w > t)
    {
        return Err("the grant cannot outlast the current one".into());
    }
    if !crate::approvals::level_within(current.level, wanted.level) {
        return Err("the act level is above this connection's".into());
    }
    Ok(())
}

/// The one pane a request is about, if it names exactly one.
pub fn single_pane(r: &Request) -> Option<u32> {
    match targets(r) {
        Targets::Panes(p) if p.len() == 1 => Some(p[0]),
        _ => None,
    }
}

fn forbidden(why: String) -> RpcError {
    RpcError::new(code::FORBIDDEN, why)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A connection's grant.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant(pub api::Grant);

impl Grant {
    /// Everything: the user's own clients on the owner-only socket.
    pub fn owner() -> Grant {
        Grant(api::Grant {
            owner: true,
            ..api::Grant::default()
        })
    }

    pub fn method_allowed(&self, method: &str) -> bool {
        // A lease is how automation sends input: whoever may send input
        // may hold one.
        if method == api::method::LEASES_ACQUIRE || method == api::method::LEASES_RELEASE {
            return self.method_allowed(api::method::TERMINAL_INPUT)
                || self.method_allowed(api::method::ACTIONS_SUBMIT);
        }
        // Reading and driving pages is one capability, never a method
        // listed by name.
        if api::INSPECT_METHODS.contains(&method) {
            return self.method_allowed(api::INSPECT_WEB);
        }
        // So is reaching a host's loopback ports.
        if method == api::method::HOSTS_FORWARD || method == api::method::HOSTS_UNFORWARD {
            return self.method_allowed(api::FORWARD_WEB);
        }
        self.0.owner
            || self.0.methods.iter().any(|m| {
                m == method
                    || m.strip_suffix(".*").is_some_and(|service| {
                        method
                            .strip_prefix(service)
                            .is_some_and(|rest| rest.starts_with('.'))
                    })
            })
    }

    /// Whether this request may run: the method, the panes it names, and
    /// the expiry. The request is already decoded for its own method.
    #[cfg(test)]
    pub fn check(&self, method: &str, request: &Request) -> Result<(), RpcError> {
        self.check_at(method, request, now_ms(), |_| false)
    }

    /// As [`Grant::check`], with the panes the connection created (`owns`)
    /// in its scope too.
    pub fn check_owning(
        &self,
        method: &str,
        request: &Request,
        owns: impl Fn(u32) -> bool,
    ) -> Result<(), RpcError> {
        self.check_at(method, request, now_ms(), owns)
    }

    fn check_at(
        &self,
        method: &str,
        request: &Request,
        now: u64,
        owns: impl Fn(u32) -> bool,
    ) -> Result<(), RpcError> {
        let g = &self.0;
        if g.owner {
            return Ok(());
        }
        if g.expires_unix_ms.is_some_and(|t| now >= t) {
            return Err(forbidden("this connection's grant has expired".into()));
        }
        if !self.method_allowed(method) {
            return Err(forbidden(format!(
                "`{method}` is not in this connection's grant"
            )));
        }
        if let Request::HostsCall(c) = request
            && opens_loopback(c)
            && !self.method_allowed(api::FORWARD_WEB)
        {
            return Err(forbidden(format!(
                "a page at a host's loopback address needs `{}` in this connection's grant",
                api::FORWARD_WEB
            )));
        }
        if g.all_panes {
            return Ok(());
        }
        match targets(request) {
            Targets::Nothing => Ok(()),
            Targets::Panes(panes) => {
                match panes.iter().find(|p| !g.panes.contains(p) && !owns(**p)) {
                    Some(p) => Err(forbidden(format!(
                        "pane %{p} is outside this connection's grant"
                    ))),
                    None => Ok(()),
                }
            }
            Targets::Everything => Err(forbidden(format!(
                "`{method}` reaches beyond this connection's panes"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrow(methods: &[&str], panes: &[u32]) -> Grant {
        Grant(api::Grant {
            owner: false,
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            panes: panes.to_vec(),
            all_panes: false,
            expires_unix_ms: None,
            level: 0,
        })
    }

    fn read(pane: u32) -> Request {
        Request::TerminalRead(api::ReadParams {
            pane,
            ..api::ReadParams::default()
        })
    }

    fn submit(kind: api::action::Kind) -> Request {
        Request::ActionsSubmit(api::SubmitAction {
            action: Some(api::Action { kind: Some(kind) }),
            ..api::SubmitAction::default()
        })
    }

    #[test]
    fn the_owner_may_do_anything() {
        let g = Grant::owner();
        assert!(
            g.check("daemon.shutdown", &Request::DaemonShutdown(api::Empty {}))
                .is_ok()
        );
        assert!(g.check("terminal.read", &read(42)).is_ok());
    }

    #[test]
    fn a_narrow_grant_allows_exactly_its_methods_and_panes() {
        let g = narrow(&["terminal.read", "tmux.*"], &[3]);
        assert!(g.check("terminal.read", &read(3)).is_ok());
        let e = g.check("terminal.read", &read(4)).expect_err("other pane");
        assert_eq!(e.code, code::FORBIDDEN);
        assert!(e.message.contains("%4"), "{e}");
        let topology = Request::TmuxTopology(api::TopologyParams::default());
        assert!(
            g.check("tmux.topology", &topology).is_ok(),
            "service wildcard"
        );
        // `tmux.*` does not reach a service merely starting with "tmux".
        assert!(!g.method_allowed("tmuxfoo.bar"));
        let input = Request::TerminalInput(api::TerminalInput {
            pane: 3,
            ..api::TerminalInput::default()
        });
        let e = g.check("terminal.input", &input).expect_err("method");
        assert!(e.message.contains("terminal.input"), "{e}");
        let e = g
            .check("daemon.shutdown", &Request::DaemonShutdown(api::Empty {}))
            .expect_err("shutdown");
        assert_eq!(e.code, code::FORBIDDEN);
    }

    #[test]
    fn actions_on_its_panes_pass_and_window_actions_need_every_pane() {
        let g = narrow(&["actions.submit"], &[3]);
        let split = submit(api::action::Kind::SplitPane(api::SplitPane {
            pane: 3,
            ..api::SplitPane::default()
        }));
        assert!(g.check("actions.submit", &split).is_ok());
        let kill_other = submit(api::action::Kind::KillPane(api::KillPane { pane: 9 }));
        assert!(g.check("actions.submit", &kill_other).is_err());
        let kill_window = submit(api::action::Kind::KillWindow(api::KillWindow { window: 1 }));
        assert!(g.check("actions.submit", &kill_window).is_err());
        let mut all = g.clone();
        all.0.all_panes = true;
        assert!(all.check("actions.submit", &kill_window).is_ok());
    }

    #[test]
    fn a_grant_narrows_but_never_widens() {
        let owner = Grant::owner().0;
        let read3 = narrow(&["terminal.read", "tmux.*"], &[3]).0;
        assert!(within(&owner, &read3).is_ok());
        assert!(within(&read3, &narrow(&["terminal.read"], &[3]).0).is_ok());
        assert!(within(&read3, &narrow(&["tmux.topology"], &[]).0).is_ok());
        assert!(within(&read3, &narrow(&["terminal.input"], &[3]).0).is_err());
        assert!(within(&read3, &narrow(&["terminal.*"], &[3]).0).is_err());
        assert!(within(&read3, &narrow(&["terminal.read"], &[4]).0).is_err());
        let mut all = narrow(&["terminal.read"], &[]).0;
        all.all_panes = true;
        assert!(within(&read3, &all).is_err());
        assert!(within(&read3, &owner).is_err());
        let mut expiring = read3.clone();
        expiring.expires_unix_ms = Some(10);
        assert!(within(&expiring, &read3).is_err(), "cannot outlast");
        let mut sooner = read3.clone();
        sooner.expires_unix_ms = Some(5);
        assert!(within(&expiring, &sooner).is_ok());
    }

    #[test]
    fn pages_are_read_and_driven_only_under_web_inspect() {
        let read = |pane| {
            Request::WebRead(api::WebRead {
                pane,
                ..api::WebRead::default()
            })
        };
        for m in api::INSPECT_METHODS {
            assert!(!narrow(&[m], &[]).method_allowed(m), "{m} by name");
            assert!(narrow(&[api::INSPECT_WEB], &[]).method_allowed(m), "{m}");
        }
        assert!(!narrow(&["web.open", "web.navigate"], &[]).method_allowed("web.read"));
        let g = narrow(&[api::INSPECT_WEB], &[3]);
        assert!(g.check("web.read", &read(3)).is_ok());
        let e = g.check("web.read", &read(4)).expect_err("other pane");
        assert!(e.message.contains("%4"), "{e}");
    }

    #[test]
    fn a_hosts_loopback_is_reached_only_under_web_forward() {
        let forward = Request::HostsForward(api::HostForward {
            name: "devbox".into(),
            port: 3000,
        });
        let page = |url: &str| {
            Request::HostsCall(api::HostCall {
                name: "devbox".into(),
                method: "web.open".into(),
                payload: Request::WebOpen(api::WebOpen {
                    url: url.into(),
                    ..api::WebOpen::default()
                })
                .encode(),
            })
        };
        let mut agent = narrow(&["hosts.*", "web.open"], &[]);
        agent.0.all_panes = true;
        let e = agent
            .check("hosts.forward", &forward)
            .expect_err("no web.forward");
        assert!(e.message.contains("hosts.forward"), "{e}");
        let e = agent
            .check("hosts.call", &page("localhost:3000"))
            .expect_err("a page at the host's loopback");
        assert!(e.message.contains("web.forward"), "{e}");
        assert!(
            agent
                .check("hosts.call", &page("https://example.com/"))
                .is_ok()
        );
        agent.0.methods.push(api::FORWARD_WEB.into());
        assert!(agent.check("hosts.forward", &forward).is_ok());
        assert!(
            agent
                .check("hosts.call", &page("http://127.0.0.1:3000/"))
                .is_ok()
        );
        assert!(!narrow(&["hosts.forward"], &[]).method_allowed("hosts.forward"));
        assert!(Grant::owner().check("hosts.forward", &forward).is_ok());
    }

    #[test]
    fn panes_a_connection_created_are_in_its_scope() {
        let g = narrow(&["terminal.read"], &[3]);
        let owns = |p: u32| p == 7;
        assert!(g.check_owning("terminal.read", &read(7), owns).is_ok());
        assert!(g.check_owning("terminal.read", &read(8), owns).is_err());
    }

    #[test]
    fn an_expired_grant_refuses_everything() {
        let mut g = narrow(&["terminal.read"], &[3]);
        g.0.expires_unix_ms = Some(1_000);
        assert!(
            g.check_at("terminal.read", &read(3), 999, |_| false)
                .is_ok()
        );
        let e = g
            .check_at("terminal.read", &read(3), 1_000, |_| false)
            .expect_err("expired");
        assert!(e.message.contains("expired"), "{e}");
    }
}
