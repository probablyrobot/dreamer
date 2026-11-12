//! Handshake decisions, free of I/O.

use crate::records::{Hello, Reject, RejectReason, Welcome};
use crate::{Limits, PROTOCOL_MAJOR, PROTOCOL_MINOR};

/// What one side says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub build_family: String,
    pub software_version: String,
    pub role: String,
    pub features: Vec<String>,
    pub limits: Limits,
    pub pid: u32,
}

impl Identity {
    /// This build, in the given role.
    pub fn current(role: &str) -> Identity {
        Identity {
            build_family: terminal_core::family::build_family(),
            software_version: env!("CARGO_PKG_VERSION").to_owned(),
            role: role.to_owned(),
            features: Vec::new(),
            limits: Limits::default(),
            pid: std::process::id(),
        }
    }
}

pub fn hello_for(id: &Identity) -> Hello {
    Hello {
        build_family: id.build_family.clone(),
        software_version: id.software_version.clone(),
        role: id.role.clone(),
        features: id.features.clone(),
        max_frame: id.limits.max_frame,
        max_message: id.limits.max_message,
        pid: id.pid,
    }
}

/// The agreed parameters of an accepted connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub limits: Limits,
    /// Optional features both sides advertised.
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Accept(Negotiated),
    Reject(Reject),
}

fn reject(server: &Identity, reason: RejectReason, message: String) -> Verdict {
    Verdict::Reject(Reject {
        reason: reason as i32,
        message,
        software_version: server.software_version.clone(),
        build_family: server.build_family.clone(),
        protocol_major: u32::from(PROTOCOL_MAJOR),
        protocol_minor: u32::from(PROTOCOL_MINOR),
        pid: server.pid,
    })
}

/// Decide on a peer's protocol version alone, from its preamble. Runs
/// before the peer's `Hello` is decoded.
pub fn evaluate_preamble(server: &Identity, major: u16, minor: u16) -> Option<Verdict> {
    (major != PROTOCOL_MAJOR).then(|| {
        reject(
            server,
            RejectReason::IncompatibleProtocol,
            format!(
                "protocol {major}.{minor} is not compatible with this daemon's \
                 {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}; stop it with `dream shutdown` and start \
                 the one from this installation with `dream ensure-running`"
            ),
        )
    })
}

/// Decide on a decoded `Hello`. The build family must match exactly: a
/// matching protocol or snapshot format number alone is not enough
/// (spec 11.4).
pub fn evaluate(server: &Identity, hello: &Hello) -> Verdict {
    if hello.build_family != server.build_family {
        return reject(
            server,
            RejectReason::IncompatibleBuild,
            format!(
                "client build family {} differs from this daemon's {} (daemon {}, pid {}); \
                 stop it with `dream shutdown` and start the one from this installation with `dream ensure-running`",
                hello.build_family, server.build_family, server.software_version, server.pid
            ),
        );
    }
    let features = server
        .features
        .iter()
        .filter(|f| hello.features.contains(f))
        .cloned()
        .collect();
    Verdict::Accept(Negotiated {
        limits: server.limits.negotiate(hello.max_frame, hello.max_message),
        features,
    })
}

/// The `Welcome` an accepting server sends.
pub fn welcome(
    server: &Identity,
    n: &Negotiated,
    instance: &str,
    epoch: u64,
    host: &str,
    workspace: &str,
) -> Welcome {
    Welcome {
        build_family: server.build_family.clone(),
        software_version: server.software_version.clone(),
        features: n.features.clone(),
        max_frame: n.limits.max_frame,
        max_message: n.limits.max_message,
        instance: instance.to_owned(),
        epoch,
        host: host.to_owned(),
        pid: server.pid,
        workspace: workspace.to_owned(),
    }
}
