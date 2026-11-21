//! The server store: a Kosmonaut server reached over gRPC, the same graph
//! kept there instead of in this process.
//!
//! Statements go through `kosmonaut.v1.Query/ExecuteQuery` with their
//! parameters as JSON text and come back as the JSON envelope
//! (`{schema, rows, num_rows}`); a write runs in a session
//! (`kosmonaut.v1.Sessions/Begin`, each statement carrying the session in
//! the `kosmonaut-tx-id` header, then `Commit`, or `Rollback` when a step
//! fails). The messages below are the fields of
//! `crates/kosmonaut-server/proto/kosmonaut/v1/messages.proto` (v0.24.0)
//! this client uses, with the same numbers; prost skips the rest.
//!
//! The provider is synchronous and runs on the daemon's blocking threads,
//! so the client has a small runtime of its own and each call waits for
//! its answer, bounded by a timeout. A call the server turns away for now
//! (its rate limit; one memory call is several statements) is sent again
//! after a pause, for a while. A server that cannot be reached, or that
//! refuses the token, is [`Error::Unavailable`].

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Status};

use super::{Params, Rows, Run};
use crate::Error;

/// The longest connecting may take.
const CONNECT: Duration = Duration::from_secs(5);
/// The longest one call may take, connecting included.
const CALL: Duration = Duration::from_secs(30);
/// How long a call is tried again while the server says to come back
/// later (`RESOURCE_EXHAUSTED`: over its rate limit, or its writer busy).
const BUSY: Duration = Duration::from_secs(10);

const EXECUTE: &str = "/kosmonaut.v1.Query/ExecuteQuery";
const BEGIN: &str = "/kosmonaut.v1.Sessions/Begin";
const COMMIT: &str = "/kosmonaut.v1.Sessions/Commit";
const ROLLBACK: &str = "/kosmonaut.v1.Sessions/Rollback";
const LIST_TABLES: &str = "/kosmonaut.v1.Catalog/ListTables";

/// The header that binds a statement to a session.
const TX_ID: &str = "kosmonaut-tx-id";

#[derive(Clone, PartialEq, prost::Message)]
struct SessionId {
    #[prost(string, tag = "1")]
    value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct QueryRequest {
    #[prost(string, tag = "1")]
    statement: String,
    /// Each value is the JSON text of a parameter.
    #[prost(map = "string, string", tag = "3")]
    params: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct QueryResponse {
    /// `{schema, rows, num_rows}`, as `POST /v1/query` answers.
    #[prost(string, tag = "3")]
    json_body: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BeginRequest {
    #[prost(bool, tag = "1")]
    read_only: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
struct BeginResponse {
    #[prost(message, optional, tag = "1")]
    session: Option<SessionId>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct SessionRef {
    #[prost(message, optional, tag = "1")]
    session: Option<SessionId>,
}

/// `CommitResponse` and `RollbackResponse`: nothing in them is used.
#[derive(Clone, PartialEq, prost::Message)]
struct Done {}

#[derive(Clone, PartialEq, prost::Message)]
struct ListTablesRequest {
    #[prost(string, tag = "1")]
    namespace: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TableEntry {
    #[prost(string, tag = "2")]
    name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ListTablesResponse {
    #[prost(message, repeated, tag = "1")]
    tables: Vec<TableEntry>,
}

#[derive(serde::Deserialize)]
struct Body {
    #[serde(default)]
    rows: Rows,
}

/// A Kosmonaut server, by its address: `grpc://host:port` (TLS unless
/// `tls` is false) or `unix:/path` (a socket on this machine).
pub struct Server {
    url: String,
    token_env: Option<String>,
    channel: Channel,
    /// Taken on drop, to shut it down without waiting: the provider may be
    /// dropped on an async thread, where a runtime cannot be dropped.
    runtime: Option<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url)
            .field("token_env", &self.token_env)
            .finish_non_exhaustive()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl Server {
    /// The server at `url`. With `token_env`, each call sends the token in
    /// that environment variable (read at the call, from nowhere else) as
    /// `Authorization: Bearer`; without it, none (a server with
    /// authentication off). Nothing connects until the first call.
    pub fn new(url: &str, tls: bool, token_env: Option<String>) -> Result<Server, Error> {
        let bad = |why: String| Error::Invalid(format!("memory.kosmonaut.url `{url}`: {why}"));
        let target = if url.starts_with("unix:") {
            if tls {
                return Err(bad("a socket takes no TLS".into()));
            }
            url.to_owned()
        } else {
            let rest = url
                .strip_prefix("grpc://")
                .ok_or_else(|| bad("not grpc://host:port or unix:/path".into()))?;
            format!("{}://{rest}", if tls { "https" } else { "http" })
        };
        let mut endpoint = Endpoint::from_shared(target)
            .map_err(|e| bad(e.to_string()))?
            .connect_timeout(CONNECT)
            .timeout(CALL);
        if tls {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .map_err(|e| bad(e.to_string()))?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("kosmonaut-client")
            .enable_all()
            .build()
            .map_err(|e| Error::Io(format!("kosmonaut client: {e}")))?;
        let channel = {
            let _in = runtime.enter();
            endpoint.connect_lazy()
        };
        Ok(Server {
            url: url.to_owned(),
            token_env,
            channel,
            runtime: Some(runtime),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The headers of a call: the token and, inside a session, its id.
    fn headers(&self, session: Option<&str>) -> Result<MetadataMap, Error> {
        let mut r = MetadataMap::new();
        if let Some(name) = &self.token_env {
            let token = std::env::var(name).ok().filter(|t| !t.is_empty());
            let token = token.ok_or_else(|| {
                Error::Unavailable(format!(
                    "kosmonaut server {}: no token: the environment variable {name} is not set",
                    self.url
                ))
            })?;
            let mut v = MetadataValue::try_from(format!("Bearer {token}")).map_err(|_| {
                Error::Unavailable(format!(
                    "kosmonaut server {}: the token in {name} is not one line of text",
                    self.url
                ))
            })?;
            v.set_sensitive(true);
            r.insert("authorization", v);
        }
        if let Some(id) = session {
            let v = MetadataValue::try_from(id)
                .map_err(|_| Error::Io(format!("kosmonaut: session id `{id}`")))?;
            r.insert(TX_ID, v);
        }
        Ok(r)
    }

    /// Run `f` on the client's runtime and wait for it.
    fn wait<T: Send + 'static>(
        &self,
        f: impl Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Error> {
        let Some(rt) = self.runtime.as_ref() else {
            return Err(Error::Unavailable("kosmonaut client stopped".into()));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let _ = tx.send(f.await);
        });
        match rx.recv() {
            Ok(r) => r.map_err(|e| self.failed(&e)),
            Err(_) => Err(Error::Unavailable(format!(
                "kosmonaut server {}: the call was dropped",
                self.url
            ))),
        }
    }

    fn unary<Q, A>(&self, path: &'static str, m: Q, session: Option<&str>) -> Result<A, Error>
    where
        Q: prost::Message + Clone + Send + Sync + 'static,
        A: prost::Message + Default + Send + Sync + 'static,
    {
        let headers = self.headers(session)?;
        let channel = self.channel.clone();
        self.wait(async move {
            let until = tokio::time::Instant::now() + BUSY;
            let mut pause = Duration::from_millis(25);
            loop {
                let mut grpc = tonic::client::Grpc::new(channel.clone());
                grpc.ready()
                    .await
                    .map_err(|e| Status::from_error(Box::new(e)))?;
                let mut req = tonic::Request::new(m.clone());
                *req.metadata_mut() = headers.clone();
                let codec = tonic_prost::ProstCodec::<Q, A>::default();
                match grpc
                    .unary(req, PathAndQuery::from_static(path), codec)
                    .await
                {
                    Err(e)
                        if e.code() == Code::ResourceExhausted
                            && tokio::time::Instant::now() + pause < until =>
                    {
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(Duration::from_secs(1));
                    }
                    r => return r.map(tonic::Response::into_inner),
                }
            }
        })
    }

    /// What a failed call means here. Not reaching the server, a timeout,
    /// or a refused token: memory is unavailable. Anything else is the
    /// statement's own failure.
    fn failed(&self, e: &Status) -> Error {
        let detail = match std::error::Error::source(e) {
            Some(s) if !e.message().contains(&s.to_string()) => format!("{}: {s}", e.message()),
            _ => e.message().to_owned(),
        };
        match e.code() {
            Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
                Error::Unavailable(format!("kosmonaut server {}: {detail}", self.url))
            }
            // A transport failure tonic has no better code for.
            Code::Unknown if std::error::Error::source(e).is_some() => {
                Error::Unavailable(format!("kosmonaut server {}: {detail}", self.url))
            }
            Code::Unauthenticated | Code::PermissionDenied => Error::Unavailable(format!(
                "kosmonaut server {} refused the token: {detail}",
                self.url
            )),
            _ => Error::Io(format!("kosmonaut: {detail}")),
        }
    }

    fn query(&self, cypher: &str, p: &Params, session: Option<&str>) -> Result<Rows, Error> {
        let m = QueryRequest {
            statement: cypher.to_owned(),
            params: p.iter().map(|(k, v)| (k.clone(), v.to_string())).collect(),
        };
        let r: QueryResponse = self.unary(EXECUTE, m, session)?;
        if r.json_body.is_empty() {
            return Ok(Rows::new());
        }
        serde_json::from_str::<Body>(&r.json_body)
            .map(|b| b.rows)
            .map_err(|e| Error::Io(format!("kosmonaut: the server's answer: {e}")))
    }

    pub(super) fn read(&self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        self.query(cypher, p, None)
    }

    /// Run `f` in one session: committed when it succeeds, rolled back
    /// when it fails.
    pub(super) fn write<T>(
        &self,
        f: impl FnOnce(&mut dyn Run) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let begun: BeginResponse = self.unary(BEGIN, BeginRequest { read_only: false }, None)?;
        let id = begun
            .session
            .map(|s| s.value)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Io("kosmonaut: the server began no session".into()))?;
        let at = |id: &str| SessionRef {
            session: Some(SessionId {
                value: id.to_owned(),
            }),
        };
        let mut tx = Tx {
            server: self,
            id: &id,
        };
        match f(&mut tx) {
            Ok(out) => {
                let _: Done = self.unary(COMMIT, at(&id), None)?;
                Ok(out)
            }
            Err(e) => {
                let _: Result<Done, _> = self.unary(ROLLBACK, at(&id), None);
                Err(e)
            }
        }
    }

    pub(super) fn tables(&self) -> Result<BTreeSet<String>, Error> {
        let r: ListTablesResponse = self.unary(
            LIST_TABLES,
            ListTablesRequest {
                namespace: String::new(),
            },
            None,
        )?;
        Ok(r.tables.into_iter().map(|t| t.name).collect())
    }

    pub(super) fn execute(&self, ddl: &str) -> Result<(), Error> {
        self.query(ddl, &Params::new(), None).map(|_| ())
    }
}

struct Tx<'a> {
    server: &'a Server,
    id: &'a str,
}

impl Run for Tx<'_> {
    fn run(&mut self, cypher: &str, p: &Params) -> Result<Rows, Error> {
        self.server.query(cypher, p, Some(self.id))
    }
}
