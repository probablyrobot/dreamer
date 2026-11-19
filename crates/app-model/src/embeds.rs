//! `terminal` and `image` nodes, as the app host fills them in.
//!
//! A `terminal` node is bound to a tmux pane: the pane's screen never
//! passes through the app or its tree. The host marks a node `refused`
//! when its pane is outside the app's grant, so every renderer shows the
//! refusal instead; the daemon refuses a published tree that shows such a
//! pane unmarked. To draw the pane in its own terminal, the host follows
//! each pane shown with `terminal.read` ([`Follower`]), under the app's
//! grant; the desktop follows it itself.
//!
//! An `image` node names a PNG or JPEG file in the app's package. The host
//! resolves it there, refuses a path that leaves the package (also through
//! a link), a file that is too large, or one that is not a PNG or JPEG,
//! and fills in the file and its size from its header. It never decodes
//! the image.
//!
//! A `web` node names one of the app's pages ([`web_page`]): a file in
//! the package's `web/` directory, served on the app's own origin, or a
//! URL on that origin or its allow-list. The host fills in the page's
//! address, the origin that gets the bridge, the directory, and the
//! allow-list, which the desktop holds the page to.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use view_tree::image::{HEADER_BYTES, MAX_BYTES, MAX_SIDE, sniff, src_problem};
use view_tree::{Frame, Kind, Node};

/// How often a followed pane is read again.
const READ_EVERY: Duration = Duration::from_millis(250);

/// What the host knows of a pane a `terminal` node shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Its screen, one line per row, and the cursor's row.
    Lines { lines: Vec<String>, cursor_y: u32 },
    /// The app may not read it: why.
    Refused(String),
    /// It could not be read (it is gone, or not mirrored): why.
    Unavailable(String),
}

/// The embeds' state, kept with the model.
#[derive(Debug, Clone, Default)]
pub struct Embeds {
    /// Each followed pane's screen, or why it is not shown.
    pub panes: BTreeMap<u32, Screen>,
    /// The grant the host's connection holds, once known.
    pub grant: Option<api::Grant>,
    /// The package directory images are read from.
    pub dir: Option<PathBuf>,
    /// Where the app's web pages come from.
    pub site: Option<Site>,
}

/// What `web` nodes need of the manifest: the app's id, its declared
/// local `url`, the origins its pages may navigate to, and whether they
/// may save downloads (`web.download` in its capabilities).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Site {
    pub app: String,
    pub url: Option<String>,
    pub allow: Vec<String>,
    pub download: bool,
}

impl Site {
    pub fn of(m: &crate::manifest::Manifest) -> Site {
        let web = m.web.clone().unwrap_or_default();
        Site {
            app: m.id.clone(),
            url: web.url,
            allow: web
                .allow
                .iter()
                .filter_map(|a| view_tree::web::origin(a))
                .collect(),
            download: m.downloads(),
        }
    }
}

/// A `web` node's page, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub url: String,
    /// The app's own origin.
    pub origin: String,
    /// The file in the package's `web/` directory, for a page from there.
    pub file: Option<String>,
}

/// The page `src` names for the app `site` describes: a URL on the app's
/// origin or its allow-list as it is; else a path in the package's `web/`
/// directory (`index.html` when empty), on the app's origin (under its
/// declared `url`, if it has one).
pub fn web_page(site: &Site, src: &str) -> Result<Page, String> {
    let origin = view_tree::web::app_origin(&site.app, site.url.as_deref())
        .ok_or("the app's `web.url` is not an address")?;
    if src.contains("://") {
        let o = view_tree::web::origin(src).ok_or_else(|| format!("`{src}` is not an address"))?;
        if o != origin && !site.allow.contains(&o) {
            return Err(format!(
                "`{src}` is not on the app's origin ({origin}) or its `web.allow`"
            ));
        }
        return Ok(Page {
            url: src.to_owned(),
            origin,
            file: None,
        });
    }
    let src = if src.is_empty() { "index.html" } else { src };
    let path = src.split(['?', '#']).next().unwrap_or_default();
    let plain = !path.is_empty()
        && !src.starts_with('~')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain {
        return Err(format!(
            "`{src}` is neither an address nor a page in the package's `web/` directory"
        ));
    }
    let url = match site.url.as_deref() {
        Some(u) => format!("{}/{src}", u.trim_end_matches('/')),
        None => format!("{origin}/{src}"),
    };
    Ok(Page {
        url,
        origin,
        file: site.url.is_none().then(|| path.to_owned()),
    })
}

/// The pages a tree manifest's `web` nodes name in the package at `dir`
/// are there: for `--check`, which cannot see a script's or process's.
pub fn check_web(dir: &Path, m: &crate::manifest::Manifest) -> Result<(), String> {
    let site = Site::of(m);
    let pages = m
        .nodes
        .iter()
        .filter(|(_, n)| n.kind == crate::manifest::NodeType::Web);
    for (key, n) in pages {
        let at = |e: String| format!("nodes.{key}.src: {e}");
        let page = web_page(&site, n.src.as_deref().unwrap_or_default()).map_err(at)?;
        if let Some(f) = &page.file {
            web_file(dir, f).map_err(at)?;
        }
    }
    Ok(())
}

/// The package's `web/` directory under `dir`, once `file` is found in
/// it (and not through a link out of it).
pub fn web_file(dir: &Path, file: &str) -> Result<PathBuf, String> {
    let root = dir
        .join("web")
        .canonicalize()
        .map_err(|_| "the package has no `web/` directory".to_owned())?;
    let found = root
        .join(file)
        .canonicalize()
        .map_err(|_| format!("no file `web/{file}` in the app's package"))?;
    if !found.starts_with(&root) || !found.is_file() {
        return Err(format!("`web/{file}` is not a file in the app's package"));
    }
    Ok(root)
}

/// Whether `grant` lets an app read pane `pane`'s content.
pub fn may_read(grant: &api::Grant, pane: u32) -> bool {
    let method = grant
        .methods
        .iter()
        .any(|m| m == api::method::TERMINAL_READ || m == "terminal.*");
    grant.owner || (method && (grant.all_panes || grant.panes.contains(&pane)))
}

impl Embeds {
    /// Why pane `pane` may not be shown, if it may not.
    fn refusal(&self, pane: u32) -> Option<String> {
        if let Some(Screen::Refused(why)) = self.panes.get(&pane) {
            return Some(why.clone());
        }
        self.grant
            .as_ref()
            .filter(|g| !may_read(g, pane))
            .map(|_| format!("pane %{pane} is outside the app's grant"))
    }

    /// Fill in `frame`'s `terminal` and `image` nodes: refusals, and each
    /// image's file and size.
    pub fn mark(&self, frame: &mut Frame) {
        self.mark_node(&mut frame.root);
    }

    fn mark_node(&self, n: &mut Node) {
        match &mut n.kind {
            Kind::Terminal { pane, refused } => {
                if let Some(why) = self.refusal(*pane) {
                    *refused = why;
                }
            }
            Kind::Image {
                src,
                path,
                width,
                height,
                refused,
            } => {
                *path = String::new();
                (*width, *height) = (0, 0);
                if let Some(dir) = &self.dir {
                    match resolve_image(dir, src) {
                        Ok((p, w, h)) => {
                            *path = p.display().to_string();
                            (*width, *height) = (w, h);
                            refused.clear();
                        }
                        Err(why) => *refused = why,
                    }
                } else if let Some(why) = src_problem(src) {
                    *refused = why.to_owned();
                }
            }
            Kind::Web { src, page, refused } => {
                let Some(site) = &self.site else { return };
                let found = web_page(site, src).and_then(|p| {
                    let root = match (&p.file, &self.dir) {
                        (Some(f), Some(d)) => Some(web_file(d, f)?),
                        _ => None,
                    };
                    Ok((p, root))
                });
                **page = view_tree::web::Resolved::default();
                match found {
                    Ok((p, root)) => {
                        **page = view_tree::web::Resolved {
                            url: p.url,
                            origin: p.origin,
                            app: site.app.clone(),
                            dir: root.map(|r| r.display().to_string()).unwrap_or_default(),
                            allow: site.allow.clone(),
                            download: site.download,
                        };
                        refused.clear();
                    }
                    Err(why) => *refused = why,
                }
            }
            _ => {}
        }
        for c in &mut n.children {
            self.mark_node(c);
        }
    }
}

/// Every pane a `terminal` node in `frame` names, refused or not, each
/// once: the host reads each, so the daemon says why one is refused.
pub fn panes_named(frame: &Frame) -> Vec<u32> {
    fn go(n: &Node, out: &mut Vec<u32>) {
        if let Kind::Terminal { pane, .. } = n.kind
            && !out.contains(&pane)
        {
            out.push(pane);
        }
        for c in n.shown() {
            go(c, out);
        }
    }
    let mut out = Vec::new();
    go(&frame.root, &mut out);
    out
}

/// The file `src` names in the package at `dir`, and its width and
/// height; or why it may not be shown.
pub fn resolve_image(dir: &Path, src: &str) -> Result<(PathBuf, u32, u32), String> {
    if let Some(why) = src_problem(src) {
        return Err(why.to_owned());
    }
    let root = dir
        .canonicalize()
        .map_err(|e| format!("the package directory: {e}"))?;
    let file = root
        .join(src)
        .canonicalize()
        .map_err(|_| format!("no file `{src}` in the app's package"))?;
    // A link may not lead out of the package.
    if !file.starts_with(&root) {
        return Err(format!("`{src}` leads out of the app's package"));
    }
    let meta = std::fs::metadata(&file).map_err(|e| format!("`{src}`: {e}"))?;
    if !meta.is_file() {
        return Err(format!("`{src}` is not a file"));
    }
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "`{src}` is larger than {} MiB",
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(&file)
        .and_then(|f| f.take(HEADER_BYTES as u64).read_to_end(&mut head))
        .map_err(|e| format!("`{src}`: {e}"))?;
    let (_, w, h) = sniff(&head).ok_or_else(|| format!("`{src}` is not a PNG or JPEG"))?;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!(
            "`{src}` is {w}x{h}; an image is 1 to {MAX_SIDE} pixels on a side"
        ));
    }
    Ok((file, w, h))
}

/// Reads the panes the tree shows, for the app host's own terminal, on
/// the host's connection (so under the app's grant). A pane the daemon
/// refuses is not read again until it is wanted anew.
#[derive(Debug)]
pub(crate) struct Follower {
    wanted: tokio::sync::watch::Sender<Vec<u32>>,
    screens: tokio::sync::mpsc::UnboundedReceiver<(u32, Screen)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Follower {
    pub(crate) fn start(client: Arc<client::Client>) -> Follower {
        let (wanted, rx) = tokio::sync::watch::channel(Vec::new());
        let (tx, screens) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(follow(client, rx, tx));
        Follower {
            wanted,
            screens,
            task,
        }
    }

    /// Follow these panes (and no others).
    pub(crate) fn want(&self, panes: Vec<u32>) {
        self.wanted.send_if_modified(|w| {
            let changed = *w != panes;
            *w = panes;
            changed
        });
    }

    /// The next pane whose screen changed; `None` once the connection is
    /// gone.
    pub(crate) async fn next(&mut self) -> Option<(u32, Screen)> {
        self.screens.recv().await
    }
}

async fn follow(
    client: Arc<client::Client>,
    mut wanted: tokio::sync::watch::Receiver<Vec<u32>>,
    out: tokio::sync::mpsc::UnboundedSender<(u32, Screen)>,
) {
    let mut last: BTreeMap<u32, Screen> = BTreeMap::new();
    let mut tick = tokio::time::interval(READ_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = wanted.changed() => if r.is_err() { return },
            _ = tick.tick() => {}
        }
        let panes = wanted.borrow_and_update().clone();
        last.retain(|p, _| panes.contains(p));
        for pane in panes {
            if matches!(last.get(&pane), Some(Screen::Refused(_))) {
                continue;
            }
            let read = client
                .call(api::Request::TerminalRead(api::ReadParams {
                    pane,
                    history: None,
                    endpoint: None,
                }))
                .await;
            let screen = match read {
                Ok(api::Response::TerminalRead(t)) => Screen::Lines {
                    lines: t.lines,
                    cursor_y: t.cursor_y,
                },
                Ok(_) => continue,
                Err(client::ClientError::Rpc(e)) if e.code == api::code::FORBIDDEN => {
                    Screen::Refused(e.message)
                }
                Err(client::ClientError::Rpc(e)) => Screen::Unavailable(e.message),
                Err(_) => return,
            };
            if last.get(&pane) != Some(&screen) {
                last.insert(pane, screen.clone());
                if out.send((pane, screen)).is_err() {
                    return;
                }
            }
        }
    }
}

/// The grant the connection holds, if the daemon says.
pub(crate) async fn grant_of(client: &client::Client) -> Option<api::Grant> {
    match client
        .call(api::Request::GrantsCurrent(api::Empty {}))
        .await
    {
        Ok(api::Response::GrantsCurrent(g)) => Some(g),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_on_the_apps_own_origin_or_an_allowed_one() {
        let site = Site {
            app: "local.web".into(),
            url: None,
            allow: vec!["https://docs.example".into()],
            download: false,
        };
        let p = web_page(&site, "").expect("the entry");
        assert_eq!(p.url, "dreamer-app://local.web/index.html");
        assert_eq!(p.origin, "dreamer-app://local.web");
        assert_eq!(p.file.as_deref(), Some("index.html"));
        let p = web_page(&site, "https://docs.example/a?b").expect("allowed");
        assert_eq!((p.url.as_str(), p.file), ("https://docs.example/a?b", None));
        assert!(web_page(&site, "https://other.example/").is_err());
        assert!(web_page(&site, "/etc/passwd").is_err());
        // A declared dev server is the app's origin, pages under it.
        let dev = Site {
            url: Some("http://localhost:5173/".into()),
            ..site
        };
        let p = web_page(&dev, "app.html#x").expect("dev");
        assert_eq!(p.url, "http://localhost:5173/app.html#x");
        assert_eq!((p.origin.as_str(), p.file), ("http://localhost:5173", None));
    }

    fn grant(panes: &[u32], all: bool) -> api::Grant {
        api::Grant {
            methods: vec!["terminal.read".into()],
            panes: panes.to_vec(),
            all_panes: all,
            ..api::Grant::default()
        }
    }

    fn tree(nodes: Vec<Node>) -> Frame {
        let mut root = Node::new("main", Kind::Row);
        root.children = nodes;
        Frame {
            title: "T".into(),
            status: String::new(),
            focus: None,
            root,
        }
    }

    fn term(key: &str, pane: u32) -> Node {
        Node::new(
            key,
            Kind::Terminal {
                pane,
                refused: String::new(),
            },
        )
    }

    fn image(key: &str, src: &str) -> Node {
        Node::new(
            key,
            Kind::Image {
                src: src.into(),
                path: String::new(),
                width: 0,
                height: 0,
                refused: String::new(),
            },
        )
    }

    fn refused(f: &Frame, key: &str) -> String {
        match &f.root.find(key).expect("node").kind {
            Kind::Terminal { refused, .. } | Kind::Image { refused, .. } => refused.clone(),
            _ => panic!("not an embed"),
        }
    }

    #[test]
    fn a_pane_outside_the_grant_is_marked_refused() {
        let mut f = tree(vec![term("a", 1), term("b", 2), term("c", 1)]);
        let mut e = Embeds::default();
        e.mark(&mut f);
        assert_eq!(refused(&f, "b"), "", "no grant known yet");
        e.grant = Some(grant(&[1], false));
        e.mark(&mut f);
        assert_eq!(refused(&f, "a"), "");
        assert_eq!(refused(&f, "b"), "pane %2 is outside the app's grant");
        assert_eq!(f.panes(), vec![1]);
        assert_eq!(panes_named(&f), vec![1, 2]);
        // Every pane, but not the method: refused.
        e.grant = Some(api::Grant {
            methods: vec!["tmux.topology".into()],
            ..grant(&[], true)
        });
        let mut g = tree(vec![term("a", 1)]);
        e.mark(&mut g);
        assert!(!refused(&g, "a").is_empty());
        // The daemon's refusal of a read is drawn too.
        let mut e = Embeds {
            grant: Some(grant(&[], true)),
            ..Embeds::default()
        };
        e.panes.insert(1, Screen::Refused("denied".into()));
        let mut h = tree(vec![term("a", 1)]);
        e.mark(&mut h);
        assert_eq!(refused(&h, "a"), "denied");
    }

    #[test]
    fn an_image_is_read_from_the_package_only() {
        let root = std::env::temp_dir().join(format!("embeds-{}", std::process::id()));
        let pkg = root.join("pkg");
        std::fs::create_dir_all(pkg.join("img")).expect("dirs");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(3u32.to_be_bytes());
        png.extend(2u32.to_be_bytes());
        std::fs::write(pkg.join("img/a.png"), &png).expect("png");
        std::fs::write(root.join("outside.png"), &png).expect("png");
        std::fs::write(pkg.join("fake.png"), b"not an image").expect("fake");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside.png"), pkg.join("link.png")).expect("link");

        let (path, w, h) = resolve_image(&pkg, "img/a.png").expect("in the package");
        assert_eq!((w, h), (3, 2));
        assert!(path.ends_with("img/a.png"));
        assert!(
            resolve_image(&pkg, "../outside.png")
                .expect_err("escapes")
                .contains("may not leave")
        );
        #[cfg(unix)]
        assert!(
            resolve_image(&pkg, "link.png")
                .expect_err("a link out")
                .contains("leads out")
        );
        assert!(
            resolve_image(&pkg, "fake.png")
                .expect_err("fake")
                .contains("not a PNG or JPEG")
        );
        assert!(
            resolve_image(&pkg, "none.png")
                .expect_err("missing")
                .contains("no file")
        );

        let e = Embeds {
            dir: Some(pkg.clone()),
            ..Embeds::default()
        };
        let mut f = tree(vec![
            image("ok", "img/a.png"),
            image("out", "../outside.png"),
        ]);
        e.mark(&mut f);
        assert_eq!(refused(&f, "ok"), "");
        assert!(matches!(
            &f.root.find("ok").expect("ok").kind,
            Kind::Image { width: 3, height: 2, path, .. } if !path.is_empty()
        ));
        assert!(!refused(&f, "out").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
