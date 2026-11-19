//! Durable memory (`memory.*`, see `docs/memory.md`): the API on the
//! provider `memory:` in `dreamer.yaml` names, applied live.
//!
//! - `files`: the user scope lives in `<config dir>/memory/`, the host
//!   scope in `<state dir>/memory/host/`, each session's in
//!   `<state dir>/memory/sessions/<id>/`, and a project's in
//!   `<project root>/.dreamer/memory/`.
//! - `kosmonaut` (a build with the `kosmonaut` feature): one database for
//!   the user, `<data dir>/memory.kosmonaut` unless the settings name
//!   another, opened on first use.
//!
//! `memory.import` and `memory.export` copy between `files` and the
//! provider in force. Calls do I/O, so they run off the event loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use api::{RpcError, code};
use workspace_config::{KosmonautSettings, MemoryProvider, MemorySettings};

use crate::log;

#[cfg(feature = "kosmonaut")]
mod openai;

#[derive(Debug)]
pub struct Memory {
    files: Arc<memory::Files>,
    current: RwLock<(MemorySettings, Arc<dyn memory::Provider>)>,
}

fn build(s: &MemorySettings, files: &Arc<memory::Files>) -> Arc<dyn memory::Provider> {
    match s.provider {
        MemoryProvider::Files => files.clone(),
        MemoryProvider::Kosmonaut => kosmonaut(&s.kosmonaut),
    }
}

#[cfg(feature = "kosmonaut")]
fn kosmonaut(k: &KosmonautSettings) -> Arc<dyn memory::Provider> {
    use memory::embed::{Command, Embedder, Lazy};
    use workspace_config::EmbedderSettings;
    let path = k
        .path
        .clone()
        .unwrap_or_else(|| platform::data_dir().join("memory.kosmonaut"));
    let embedder: Option<Arc<dyn Embedder>> = match &k.embedder {
        None => None,
        Some(EmbedderSettings::Static { path }) => {
            Some(Arc::new(Lazy::static_vectors(path.clone())))
        }
        Some(EmbedderSettings::Local {
            command,
            dimension,
            timeout,
        }) => match Command::new(command.clone(), *dimension, *timeout) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                log::warn(format!("memory embedder: {e}; recall runs without vectors"));
                None
            }
        },
        Some(EmbedderSettings::OpenAi {
            model,
            key_env,
            url,
            dimension,
            timeout,
        }) => {
            match openai::OpenAi::new(model.clone(), key_env.clone(), url, *dimension, *timeout) {
                Ok(o) => Some(Arc::new(o)),
                Err(e) => {
                    log::warn(format!("memory embedder: {e}; recall runs without vectors"));
                    None
                }
            }
        }
    };
    Arc::new(memory::Kosmonaut::new(path, embedder))
}

#[cfg(not(feature = "kosmonaut"))]
fn kosmonaut(_: &KosmonautSettings) -> Arc<dyn memory::Provider> {
    log::warn("memory provider kosmonaut: this dreamerd was built without the kosmonaut feature");
    Arc::new(Unavailable)
}

/// The `kosmonaut` provider in a build without it: every call says so.
#[cfg(not(feature = "kosmonaut"))]
#[derive(Debug)]
struct Unavailable;

#[cfg(not(feature = "kosmonaut"))]
impl Unavailable {
    fn error<T>() -> Result<T, memory::Error> {
        Err(memory::Error::Invalid(
            "memory.provider is kosmonaut, but this dreamerd was built without the kosmonaut \
             feature: set memory.provider: files, or use a build that has it"
                .into(),
        ))
    }
}

#[cfg(not(feature = "kosmonaut"))]
impl memory::Provider for Unavailable {
    fn name(&self) -> &'static str {
        "kosmonaut"
    }
    fn remember(
        &self,
        _: &memory::Context,
        _: memory::New,
    ) -> Result<memory::Memory, memory::Error> {
        Unavailable::error()
    }
    fn list(
        &self,
        _: &memory::Context,
        _: Option<memory::Scope>,
        _: Option<memory::Kind>,
    ) -> Result<memory::Found, memory::Error> {
        Unavailable::error()
    }
    fn get(&self, _: &memory::Context, _: &str) -> Result<memory::Memory, memory::Error> {
        Unavailable::error()
    }
    fn update(
        &self,
        _: &memory::Context,
        _: &str,
        _: memory::Change,
    ) -> Result<memory::Memory, memory::Error> {
        Unavailable::error()
    }
    fn forget(&self, _: &memory::Context, _: &str) -> Result<memory::Memory, memory::Error> {
        Unavailable::error()
    }
    fn put(
        &self,
        _: &memory::Context,
        _: &memory::Memory,
    ) -> Result<memory::Memory, memory::Error> {
        Unavailable::error()
    }
}

impl Memory {
    pub fn new(state_dir: &Path, settings: &MemorySettings) -> Memory {
        let files = Arc::new(memory::Files::new(
            workspace_config::config_dir().map(|d| d.join("memory")),
            state_dir.join("memory").join("host"),
            state_dir.join("memory").join("sessions"),
        ));
        let provider = build(settings, &files);
        Memory {
            files,
            current: RwLock::new((settings.clone(), provider)),
        }
    }

    /// Use the provider `settings` name from the next call on.
    pub fn configure(&self, settings: &MemorySettings) {
        let mut cur = self.current.write().unwrap_or_else(|e| e.into_inner());
        if cur.0 != *settings {
            *cur = (settings.clone(), build(settings, &self.files));
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

    /// The index an agent session in `cwd` starts with: every memory of
    /// the scopes it reaches (user, host, its project, and `session`),
    /// bounded. `None` when there are none or they cannot be read.
    pub async fn index(&self, cwd: &str, session: &str) -> Option<String> {
        let cx = memory::Context {
            cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
            session: (!session.is_empty()).then(|| session.to_owned()),
        };
        match self.run(move |m| m.list(&cx, None, None)).await {
            Ok(found) => memory::index(&found.items),
            Err(e) => {
                log::warn(format!("memory index for session {session}: {}", e.message));
                None
            }
        }
    }

    pub async fn forget(&self, p: api::MemoryRef) -> Result<api::MemoryItem, RpcError> {
        let cx = context(p.context);
        self.run(move |m| m.forget(&cx, &p.id)).await.map(item)
    }

    /// `memory.import`: from `files` into the provider in force.
    pub async fn import(&self, p: api::MemoryTransfer) -> Result<api::MemoryTransferred, RpcError> {
        self.transfer(p, true).await
    }

    /// `memory.export`: from the provider in force into `files`.
    pub async fn export(&self, p: api::MemoryTransfer) -> Result<api::MemoryTransferred, RpcError> {
        self.transfer(p, false).await
    }

    async fn transfer(
        &self,
        p: api::MemoryTransfer,
        import: bool,
    ) -> Result<api::MemoryTransferred, RpcError> {
        let cx = context(p.context);
        let scope = scope(p.scope)?;
        let current = self.provider();
        if current.name() == "files" {
            return Err(invalid(
                "the provider in force is files: import and export copy between files and \
                 another provider (memory.provider)"
                    .into(),
            ));
        }
        let files: Arc<dyn memory::Provider> = self.files.clone();
        let (from, to) = if import {
            (files, current)
        } else {
            (current, files)
        };
        let names = (from.name(), to.name());
        let moved = tokio::task::spawn_blocking(move || {
            memory::transfer(from.as_ref(), to.as_ref(), &cx, scope)
        })
        .await
        .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
        .map_err(error)?;
        log::info(format!(
            "memory: copied {} from {} to {}",
            moved.ids.len(),
            names.0,
            names.1
        ));
        Ok(api::MemoryTransferred {
            ids: moved.ids,
            problems: moved.problems.into_iter().map(problem).collect(),
            from: names.0.to_owned(),
            to: names.1.to_owned(),
        })
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
        problems: f.problems.into_iter().map(problem).collect(),
        provider: provider.to_owned(),
    }
}

fn problem(p: memory::Problem) -> api::MemoryProblem {
    api::MemoryProblem {
        path: p.path.display().to_string(),
        message: p.message,
    }
}
