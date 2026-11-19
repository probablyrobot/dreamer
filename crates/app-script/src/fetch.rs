//! `fetch` for a script app: HTTP and HTTPS requests to the hosts its
//! manifest lists (`[net] hosts`), under the `net.fetch` capability.
//!
//! This is the one place in product code that opens network connections
//! of its own (`cargo xtask policy` allows `ureq` here and nowhere else).
//! Every request, and every redirect it follows, is checked against the
//! listed hosts before a connection is made; anything else is refused.
//! Requests run on a thread of their own, never on the app's, with a time
//! limit for the whole exchange (redirects and the body included) and a
//! limit on the body's size.
//!
//! The client is `ureq` with rustls (the `ring` provider) and Mozilla's
//! root certificates compiled in (`webpki-roots`); see
//! `docs/dependency-baseline.md`. No proxy from the environment is used,
//! no cookies are kept, and no response is cached.

use std::time::{Duration, Instant};

use app_model::NetGrant;
use app_model::manifest::NetHost;
use ureq::http;

/// The most redirects one request follows (as the Fetch standard does).
const MAX_REDIRECTS: usize = 20;

/// Headers an app may not set: the connection's own, and the host.
const FORBIDDEN_HEADERS: [&str; 9] = [
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "transfer-encoding",
    "upgrade",
];

/// The methods a request may use.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// What an app's `fetch` may reach: whether its grant holds `net.fetch`
/// (as the host learns it from the daemon), and the hosts its manifest
/// lists.
#[derive(Debug, Clone)]
pub(crate) struct Net {
    grant: NetGrant,
    hosts: Vec<NetHost>,
}

impl Net {
    pub(crate) fn new(grant: NetGrant, hosts: &[String]) -> Net {
        Net {
            grant,
            // Validation refused a manifest with a host that does not
            // parse; one that still does not reaches nothing.
            hosts: hosts
                .iter()
                .filter_map(|h| NetHost::parse(h).ok())
                .collect(),
        }
    }

    /// `url`, if `fetch` may reach it; why not otherwise.
    pub(crate) fn check(&self, url: &str) -> Result<http::Uri, String> {
        self.grant.check()?;
        let uri: http::Uri = url
            .parse()
            .map_err(|_| format!("fetch: `{url}` is not a URL"))?;
        let Target { scheme, host, port } =
            target(&uri).ok_or_else(|| format!("fetch: `{url}` is not an http or https URL"))?;
        if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
            return Err(format!(
                "fetch: `{url}` has a user name or password; send an Authorization header instead"
            ));
        }
        if self.hosts.iter().any(|h| h.allows(&scheme, &host, port)) {
            Ok(uri)
        } else {
            Err(format!(
                "fetch: {host}:{port} is not among the hosts this app's manifest lists ([net] hosts)"
            ))
        }
    }
}

/// A URL's scheme, host, and port.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    scheme: String,
    host: String,
    port: u16,
}

fn target(uri: &http::Uri) -> Option<Target> {
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let own = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let auth = uri.authority()?;
    let host = auth.host().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(Target {
        scheme,
        host,
        port: auth.port_u16().unwrap_or(own),
    })
}

/// What a request does with a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Redirect {
    /// Follow it, if it is to a listed host.
    Follow,
    /// Fail.
    Error,
    /// Answer with the redirect itself.
    Manual,
}

/// A request an app made, checked.
#[derive(Debug)]
pub(crate) struct Request {
    uri: http::Uri,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    redirect: Redirect,
}

impl Request {
    /// Check what the app gave `fetch`: the URL against `net`, the method,
    /// the headers (a JSON array of `[name, value]`), and the body against
    /// `size`.
    pub(crate) fn new(
        net: &Net,
        url: &str,
        method: &str,
        headers: &str,
        body: Option<String>,
        redirect: &str,
        size: usize,
    ) -> Result<Request, String> {
        let uri = net.check(url)?;
        let method = method.to_ascii_uppercase();
        if !METHODS.contains(&method.as_str()) {
            return Err(format!(
                "fetch: `{method}` is not a method fetch sends ({})",
                METHODS.join(", ")
            ));
        }
        let headers: Vec<(String, String)> = serde_json::from_str(headers)
            .map_err(|_| "fetch: the headers are not name and value strings")?;
        for (name, value) in &headers {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err()
                || http::HeaderValue::from_str(value).is_err()
            {
                return Err(format!("fetch: `{name}` is not a header name and value"));
            }
            let lower = name.to_ascii_lowercase();
            if FORBIDDEN_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
                return Err(format!(
                    "fetch: the `{lower}` header is the connection's own"
                ));
            }
        }
        if body.is_some() && (method == "GET" || method == "HEAD") {
            return Err(format!("fetch: a {method} request has no body"));
        }
        if body.as_ref().is_some_and(|b| b.len() > size) {
            return Err(format!("fetch: the body is larger than {size} bytes"));
        }
        let redirect = match redirect {
            "follow" => Redirect::Follow,
            "error" => Redirect::Error,
            "manual" => Redirect::Manual,
            other => {
                return Err(format!(
                    "fetch: `redirect` is \"follow\", \"error\", or \"manual\", not \"{other}\""
                ));
            }
        };
        Ok(Request {
            uri,
            method,
            headers,
            body,
            redirect,
        })
    }
}

/// A response, its body read whole.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub status: u16,
    pub status_text: String,
    pub url: String,
    pub redirected: bool,
    pub headers: Vec<(String, String)>,
    /// The body as text (UTF-8, any invalid sequence replaced).
    pub body: String,
}

impl Fetched {
    /// Everything but the body, as JSON for the app's side.
    pub(crate) fn head_json(&self) -> String {
        serde_json::json!({
            "status": self.status,
            "statusText": self.status_text,
            "url": self.url,
            "redirected": self.redirected,
            "headers": self.headers,
        })
        .to_string()
    }
}

/// An app's HTTP client.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    agent: ureq::Agent,
    net: Net,
    time: Duration,
    size: usize,
}

impl Client {
    /// A client for `net`, each request limited to `time` and its body to
    /// `size` bytes.
    pub(crate) fn new(net: Net, time: Duration, size: usize) -> Client {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .user_agent(format!("Dreamer/{}", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(time))
            .build()
            .into();
        Client {
            agent,
            net,
            time,
            size,
        }
    }

    pub(crate) fn net(&self) -> &Net {
        &self.net
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// Send `req` and read its answer, following redirects to listed
    /// hosts. Blocks: run it off the app's thread.
    pub(crate) fn fetch(&self, req: Request) -> Result<Fetched, String> {
        let deadline = Instant::now() + self.time;
        let Request {
            mut uri,
            mut method,
            mut headers,
            mut body,
            redirect,
        } = req;
        let mut redirected = false;
        for hop in 0..=MAX_REDIRECTS {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(self.timed_out());
            }
            let mut b = http::Request::builder().method(method.as_str()).uri(&uri);
            for (n, v) in &headers {
                b = b.header(n.as_str(), v.as_str());
            }
            let sent = match &body {
                Some(text) => {
                    let r = b.body(text.as_bytes()).map_err(|e| format!("fetch: {e}"))?;
                    self.agent.run(
                        self.agent
                            .configure_request(r)
                            .timeout_global(Some(left))
                            .build(),
                    )
                }
                None => {
                    let r = b.body(()).map_err(|e| format!("fetch: {e}"))?;
                    self.agent.run(
                        self.agent
                            .configure_request(r)
                            .timeout_global(Some(left))
                            .build(),
                    )
                }
            };
            let mut resp = sent.map_err(|e| self.error(&e))?;
            let status = resp.status();
            let location = resp
                .headers()
                .get(http::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .map(str::to_owned);
            if let Some(loc) = location
                && redirect != Redirect::Manual
                && matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
            {
                if redirect == Redirect::Error {
                    return Err("fetch: the response redirects, and `redirect` is \"error\"".into());
                }
                if hop == MAX_REDIRECTS {
                    return Err(format!("fetch: more than {MAX_REDIRECTS} redirects"));
                }
                let next = resolve(&uri, &loc)
                    .ok_or_else(|| format!("fetch: the redirect to `{loc}` is not a URL"))?;
                let next = self.net.check(&next).map_err(|e| {
                    format!(
                        "fetch: a redirect was refused: {}",
                        e.trim_start_matches("fetch: ")
                    )
                })?;
                // As the Fetch standard: a 303, or a POST answered with a
                // 301 or 302, goes on as a GET without its body.
                let as_get = (status.as_u16() == 303 && method != "HEAD")
                    || (matches!(status.as_u16(), 301 | 302) && method == "POST");
                if as_get {
                    method = "GET".into();
                    body = None;
                    headers.retain(|(n, _)| {
                        !matches!(
                            n.to_ascii_lowercase().as_str(),
                            "content-type"
                                | "content-encoding"
                                | "content-language"
                                | "content-location"
                        )
                    });
                }
                // Credentials go to the origin they were given for only.
                if target(&next) != target(&uri) {
                    headers.retain(|(n, _)| {
                        !matches!(n.to_ascii_lowercase().as_str(), "authorization" | "cookie")
                    });
                }
                uri = next;
                redirected = true;
                continue;
            }
            let bytes = resp
                .body_mut()
                .with_config()
                .limit(self.size as u64)
                .read_to_vec()
                .map_err(|e| self.error(&e))?;
            let headers = resp
                .headers()
                .iter()
                .map(|(n, v)| {
                    (
                        n.as_str().to_owned(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            return Ok(Fetched {
                status: status.as_u16(),
                status_text: status.canonical_reason().unwrap_or("").to_owned(),
                url: uri.to_string(),
                redirected,
                headers,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Err(format!("fetch: more than {MAX_REDIRECTS} redirects"))
    }

    fn timed_out(&self) -> String {
        format!(
            "fetch: no complete answer within {} ms",
            self.time.as_millis()
        )
    }

    fn error(&self, e: &ureq::Error) -> String {
        match e {
            ureq::Error::Timeout(_) => self.timed_out(),
            ureq::Error::BodyExceedsLimit(_) => {
                format!("fetch: the response is larger than {} bytes", self.size)
            }
            e => format!("fetch: {e}"),
        }
    }
}

/// `location` (a redirect's `Location`) against the URL it answered.
fn resolve(base: &http::Uri, location: &str) -> Option<String> {
    let loc = location.split('#').next().unwrap_or("");
    let scheme = base.scheme_str()?;
    let auth = base.authority()?.as_str();
    let has_scheme = loc.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && !s.contains('/')
    });
    let url = if has_scheme {
        loc.to_owned()
    } else if let Some(rest) = loc.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if loc.starts_with('/') {
        format!("{scheme}://{auth}{loc}")
    } else if loc.starts_with('?') {
        format!("{scheme}://{auth}{}{loc}", base.path())
    } else {
        let path = base.path();
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        let dir = if dir.is_empty() { "/" } else { dir };
        format!("{scheme}://{auth}{dir}{loc}")
    };
    url.parse::<http::Uri>().ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_model::manifest::NET_FETCH;

    fn net(hosts: &[&str]) -> Net {
        Net::new(
            NetGrant::declared(&[NET_FETCH.to_owned()]),
            &hosts.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn only_listed_hosts_on_their_ports_are_reached() {
        let n = net(&["api.example.com", "127.0.0.1:8080", "[::1]:3000"]);
        for ok in [
            "https://api.example.com/v1?q=1",
            "http://API.example.com./x",
            "http://127.0.0.1:8080/",
            "http://[::1]:3000/x",
        ] {
            assert!(n.check(ok).is_ok(), "{ok}: {:?}", n.check(ok));
        }
        for (bad, why) in [
            ("https://example.com/", "not among the hosts"),
            ("https://api.example.com:8443/", "not among the hosts"),
            ("https://api.example.com.evil.com/", "not among the hosts"),
            ("http://127.0.0.1/", "not among the hosts"),
            ("http://127.0.0.1:8081/", "not among the hosts"),
            ("ftp://api.example.com/", "not an http or https URL"),
            ("/relative", "not an http or https URL"),
            ("https://user:pw@api.example.com/", "user name or password"),
            ("not a url", "is not a URL"),
        ] {
            let e = n.check(bad).expect_err(bad);
            assert!(e.contains(why), "{bad}: {e}");
        }
        // The grant decides, whatever the manifest lists: not known yet,
        // a grant without `net.fetch`, and one with it.
        let grant = NetGrant::default();
        let hosts = Net::new(grant.clone(), &["api.example.com".to_owned()]);
        let url = "https://api.example.com/";
        let e = hosts.check(url).expect_err("unknown");
        assert!(e.contains("not known yet"), "{e}");
        grant.set(Some(&api::Grant {
            methods: vec!["tmux.topology".into()],
            ..api::Grant::default()
        }));
        let e = hosts.check(url).expect_err("refused");
        assert!(e.contains("grant does not include `net.fetch`"), "{e}");
        grant.set(None);
        assert!(hosts.check(url).is_err(), "the daemon did not say");
        grant.set(Some(&api::Grant {
            methods: vec!["net.fetch".into()],
            ..api::Grant::default()
        }));
        assert!(hosts.check(url).is_ok());
    }

    #[test]
    fn a_request_is_checked_before_it_is_sent() {
        let n = net(&["127.0.0.1:8080"]);
        let url = "http://127.0.0.1:8080/";
        let req = |method: &str, headers: &str, body: Option<&str>, redirect: &str| {
            Request::new(
                &n,
                url,
                method,
                headers,
                body.map(str::to_owned),
                redirect,
                10,
            )
        };
        assert!(
            req(
                "post",
                r#"[["content-type","text/plain"]]"#,
                Some("hi"),
                "follow"
            )
            .is_ok()
        );
        for (r, why) in [
            (req("CONNECT", "[]", None, "follow"), "not a method"),
            (req("GET", "[]", Some("x"), "follow"), "has no body"),
            (
                req("POST", "[]", Some("01234567890"), "follow"),
                "larger than 10 bytes",
            ),
            (
                req("GET", r#"[["Host","evil.com"]]"#, None, "follow"),
                "connection's own",
            ),
            (
                req("GET", r#"[["bad name","x"]]"#, None, "follow"),
                "not a header",
            ),
            (req("GET", "[]", None, "sometimes"), "`redirect` is"),
        ] {
            let e = r.expect_err(why);
            assert!(e.contains(why), "{why}: {e}");
        }
    }

    #[test]
    fn redirects_resolve_against_the_url_they_answer() {
        let base: http::Uri = "http://h:1/a/b?x=1".parse().expect("uri");
        let r = |l: &str| resolve(&base, l);
        assert_eq!(r("https://o/p").as_deref(), Some("https://o/p"));
        assert_eq!(r("//o:2/p").as_deref(), Some("http://o:2/p"));
        assert_eq!(r("/p#frag").as_deref(), Some("http://h:1/p"));
        assert_eq!(r("c").as_deref(), Some("http://h:1/a/c"));
        assert_eq!(r("?y=2").as_deref(), Some("http://h:1/a/b?y=2"));
    }
}
