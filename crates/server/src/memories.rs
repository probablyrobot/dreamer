//! Durable memory (`memory.*`, see `docs/memory.md`): the API on the
//! provider `memory:` in `dreamer.yaml` names, applied live. The user
//! scope lives in `<config dir>/memory/`, the host scope in
//! `<state dir>/memory/host/`, each session's in
//! `<state dir>/memory/sessions/<id>/`, and a project's in
//! `<project root>/.dreamer/memory/`. Calls do file I/O, so they run off
//! the event loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use api::{RpcError, code};
use workspace_config::{MemoryProvider, MemorySettings};

use crate::log;

#[derive(Debug)]
pub struct Memory {
    user: Option<PathBuf>,
    host: PathBuf,
    sessions: PathBuf,
    current: RwLock<(MemoryProvider, Arc<dyn memory::Provider>)>,
}

fn build(
    p: MemoryProvider,
    user: Option<PathBuf>,
    host: PathBuf,
    sessions: PathBuf,
) -> Arc<dyn memory::Provider> {
    match p {
        MemoryProvider::Files => Arc::new(memory::Files::new(user, host, sessions)),
    }
}

impl Memory {
    pub fn new(state_dir: &Path, settings: &MemorySettings) -> Memory {
        let user = workspace_config::config_dir().map(|d| d.join("memory"));
        let host = state_dir.join("memory").join("host");
        let sessions = state_dir.join("memory").join("sessions");
        let provider = build(
            settings.provider,
            user.clone(),
            host.clone(),
            sessions.clone(),
        );
        Memory {
            user,
            host,
            sessions,
            current: RwLock::new((settings.provider, provider)),
        }
    }

    /// Use the provider `settings` name from the next call on.
    pub fn configure(&self, settings: &MemorySettings) {
        let mut cur = self.current.write().unwrap_or_else(|e| e.into_inner());
        if cur.0 != settings.provider {
            *cur = (
                settings.provider,
                build(
                    settings.provider,
                    self.user.clone(),
                    self.host.clone(),
                    self.sessions.clone(),
                ),
            );
            log::info(format!(
                "configuration applied: memory provider {}",
                settings.provider.name()
            ));
        }
    }

    fn provider(&self) -> Arc<dyn memory::Provider> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn memory::Provider) -> Result<T, memory::Error> + Send + 'static,
    ) -> Result<T, RpcError> {
        let p = self.provider();
        tokio::task::spawn_blocking(move || f(p.as_ref()))
            .await
            .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
            .map_err(error)
    }

    pub async fn recall(&self, p: api::MemoryRecall) -> Result<api::MemoryItems, RpcError> {
        let cx = context(p.context);
        let scope = scope(p.scope)?;
        let limit = p.limit.map(|l| l as usize);
        let name = self.provider().name();
        let found = self
            .run(move |m| m.recall(&cx, &p.query, scope, limit))
            .await?;
        Ok(items(found, name))
    }

    pub async fn remember(&self, p: api::MemoryRemember) -> Result<api::MemoryItem, RpcError> {
        let cx = context(p.context);
        let new = memory::New {
            text: p.text,
            kind: kind(p.kind)?.ok_or_else(|| {
                invalid("give a kind: user, feedback, project, or reference".into())
            })?,
            scope: scope(p.scope)?
                .ok_or_else(|| invalid("give a scope: user, host, project, or session".into()))?,
            title: p.title,
            description: p.description,
            links: p.links,
        };
        self.run(move |m| m.remember(&cx, new)).await.map(item)
    }

    pub async fn list(&self, p: api::MemoryListQuery) -> Result<api::MemoryItems, RpcError> {
        let cx = context(p.context);
        let (scope, kind) = (scope(p.scope)?, kind(p.kind)?);
        let name = self.provider().name();
        let found = self.run(move |m| m.list(&cx, scope, kind)).await?;
        Ok(items(found, name))
    }

    pub async fn get(&self, p: api::MemoryRef) -> Result<api::MemoryItem, RpcError> {
        let cx = context(p.context);
        self.run(move |m| m.get(&cx, &p.id)).await.map(item)
    }

    pub async fn update(&self, p: api::MemoryUpdate) -> Result<api::MemoryItem, RpcError> {
        let cx = context(p.context);
        let change = memory::Change {
            text: p.text,
            kind: kind(p.kind)?,
            title: p.title,
            description: p.description,
            links: if p.clear_links {
                Some(Vec::new())
            } else {
                (!p.links.is_empty()).then_some(p.links)
            },
        };
        self.run(move |m| m.update(&cx, &p.id, change))
            .await
            .map(item)
    }

    pub async fn forget(&self, p: api::MemoryRef) -> Result<api::MemoryItem, RpcError> {
        let cx = context(p.context);
        self.run(move |m| m.forget(&cx, &p.id)).await.map(item)
    }
}

fn invalid(m: String) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, m)
}

fn error(e: memory::Error) -> RpcError {
    let c = match &e {
        memory::Error::NotFound(_) => code::NOT_FOUND,
        memory::Error::Invalid(_) => code::INVALID_PARAMS,
        memory::Error::Malformed(_) => code::PRECONDITION,
        memory::Error::Io(_) => code::INTERNAL,
    };
    RpcError::new(c, e.to_string())
}

fn context(c: Option<api::MemoryContext>) -> memory::Context {
    let c = c.unwrap_or_default();
    memory::Context {
        cwd: c.cwd.filter(|d| !d.is_empty()).map(PathBuf::from),
        session: c.session.filter(|s| !s.is_empty()),
    }
}

fn scope(n: i32) -> Result<Option<memory::Scope>, RpcError> {
    use api::MemoryScope as S;
    match S::try_from(n) {
        Ok(S::Unspecified) => Ok(None),
        Ok(S::User) => Ok(Some(memory::Scope::User)),
        Ok(S::Host) => Ok(Some(memory::Scope::Host)),
        Ok(S::Project) => Ok(Some(memory::Scope::Project)),
        Ok(S::Session) => Ok(Some(memory::Scope::Session)),
        Err(_) => Err(invalid(format!("{n} is not a memory scope"))),
    }
}

fn kind(n: i32) -> Result<Option<memory::Kind>, RpcError> {
    use api::MemoryKind as K;
    match K::try_from(n) {
        Ok(K::Unspecified) => Ok(None),
        Ok(K::User) => Ok(Some(memory::Kind::User)),
        Ok(K::Feedback) => Ok(Some(memory::Kind::Feedback)),
        Ok(K::Project) => Ok(Some(memory::Kind::Project)),
        Ok(K::Reference) => Ok(Some(memory::Kind::Reference)),
        Err(_) => Err(invalid(format!("{n} is not a memory kind"))),
    }
}

fn item(m: memory::Memory) -> api::MemoryItem {
    let scope = match m.scope {
        memory::Scope::User => api::MemoryScope::User,
        memory::Scope::Host => api::MemoryScope::Host,
        memory::Scope::Project => api::MemoryScope::Project,
        memory::Scope::Session => api::MemoryScope::Session,
    };
    let kind = match m.kind {
        memory::Kind::User => api::MemoryKind::User,
        memory::Kind::Feedback => api::MemoryKind::Feedback,
        memory::Kind::Project => api::MemoryKind::Project,
        memory::Kind::Reference => api::MemoryKind::Reference,
    };
    api::MemoryItem {
        id: m.id,
        scope: scope as i32,
        kind: kind as i32,
        title: m.title,
        description: m.description,
        text: m.text,
        links: m.links,
        created_unix_ms: m.created_unix_ms,
        updated_unix_ms: m.updated_unix_ms,
        path: m.path.map(|p| p.display().to_string()),
        score: m.score,
    }
}

fn items(f: memory::Found, provider: &str) -> api::MemoryItems {
    api::MemoryItems {
        items: f.items.into_iter().map(item).collect(),
        problems: f
            .problems
            .into_iter()
            .map(|p| api::MemoryProblem {
                path: p.path.display().to_string(),
                message: p.message,
            })
            .collect(),
        provider: provider.to_owned(),
    }
}
