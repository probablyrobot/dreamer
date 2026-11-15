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
        Request::TerminalMirrors(_)
        | Request::DaemonShutdown(_)
        | Request::HostsConnect(_)
        | Request::HostsDisconnect(_)
        | Request::HostsReconnect(_)
        | Request::HostsCall(_)
        | Request::HostsSubscribe(_)
        | Request::HostsWatch(_)
        | Request::HostsInstallPlan(_)
        | Request::HostsInstall(_)
        | Request::HostsUpgrade(_) => Targets::Everything,
        _ => Targets::Nothing,
    }
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
    pub fn check(&self, method: &str, request: &Request) -> Result<(), RpcError> {
        self.check_at(method, request, now_ms())
    }

    fn check_at(&self, method: &str, request: &Request, now: u64) -> Result<(), RpcError> {
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
        if g.all_panes {
            return Ok(());
        }
        match targets(request) {
            Targets::Nothing => Ok(()),
            Targets::Panes(panes) => match panes.iter().find(|p| !g.panes.contains(p)) {
                Some(p) => Err(forbidden(format!(
                    "pane %{p} is outside this connection's grant"
                ))),
                None => Ok(()),
            },
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
    fn an_expired_grant_refuses_everything() {
        let mut g = narrow(&["terminal.read"], &[3]);
        g.0.expires_unix_ms = Some(1_000);
        assert!(g.check_at("terminal.read", &read(3), 999).is_ok());
        let e = g
            .check_at("terminal.read", &read(3), 1_000)
            .expect_err("expired");
        assert!(e.message.contains("expired"), "{e}");
    }
}
