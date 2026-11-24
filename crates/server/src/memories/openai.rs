//! The `openai` embedder (`memory.embedder.kind: openai`):
//! texts to vectors through OpenAI's embeddings API, `POST <url>/embeddings`
//! with `{model, input: [...]}`, answered with `{data: [{index, embedding}]}`.
//!
//! Besides app `fetch`, this is the one product file that uses the HTTP
//! client (`cargo xtask policy` allows `ureq` here). It runs only when the
//! user configured it, and posts only to the API base the settings name:
//! `https`, or plain `http` to this machine (settings refuse anything
//! else). The key is read from the environment variable `key_env` names,
//! at each call, and from nowhere else; it goes in the `Authorization`
//! header and is never logged or put in an error. Texts go in batches of
//! [`BATCH`], each request limited to the configured timeout. No proxy
//! from the environment is used.

use std::time::Duration;

use memory::Error;
use memory::embed::Embedder;

/// The most texts one request carries.
pub(crate) const BATCH: usize = 96;

/// The largest answer read: 96 vectors of 3072 numbers, as JSON, with
/// room to spare.
const MAX_ANSWER: u64 = 64 << 20;

/// The longest part of an API error message put in a problem.
const MAX_MESSAGE: usize = 300;

#[derive(Debug)]
pub(crate) struct OpenAi {
    /// `openai:<model>`, kept beside every vector.
    model: String,
    api_model: String,
    key_env: String,
    endpoint: String,
    dimension: usize,
    /// Asked for (`dimensions`) when the settings set one.
    dimensions: Option<usize>,
    timeout: Duration,
    agent: ureq::Agent,
    /// Reads an environment variable: the process's own, except in tests.
    env: fn(&str) -> Option<std::ffi::OsString>,
}

/// The vector length of OpenAI's embedding models, for a model the
/// settings give no `dimension` for.
fn known_dimension(model: &str) -> Option<usize> {
    match model {
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        _ => None,
    }
}

impl OpenAi {
    /// An embedder for `model` at `url` (an API base such as
    /// `https://api.openai.com/v1`). Nothing is sent and the key is not
    /// read until the first call.
    pub(crate) fn new(
        model: String,
        key_env: String,
        url: &str,
        dimension: Option<usize>,
        timeout: Duration,
    ) -> Result<OpenAi, Error> {
        let size = dimension
            .or_else(|| known_dimension(&model))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "openai embedder: the vector length of `{model}` is not known; set \
                 memory.embedder.dimension"
                ))
            })?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .user_agent(format!("Dreamer/{}", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(timeout))
            .build()
            .into();
        Ok(OpenAi {
            model: format!("openai:{model}"),
            api_model: model,
            key_env,
            endpoint: format!("{}/embeddings", url.trim_end_matches('/')),
            dimension: size,
            dimensions: dimension,
            timeout,
            agent,
            env: |name| std::env::var_os(name),
        })
    }

    fn fail(&self, m: impl std::fmt::Display) -> Error {
        Error::Io(format!("openai embedder ({}): {m}", self.endpoint))
    }

    /// The key, from the environment variable the settings name only.
    fn key(&self) -> Result<String, Error> {
        match (self.env)(&self.key_env).map(std::ffi::OsString::into_string) {
            Some(Ok(k)) if !k.trim().is_empty() => Ok(k.trim().to_owned()),
            Some(Err(_)) => Err(Error::Invalid(format!(
                "openai embedder: the environment variable {} (key_env) is not text",
                self.key_env
            ))),
            _ => Err(Error::Invalid(format!(
                "openai embedder: the environment variable {} (key_env) is not set in \
                 dreamerd's environment",
                self.key_env
            ))),
        }
    }

    /// One request: a vector for each of `input`, in order.
    fn request(&self, key: &str, input: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        let mut body = serde_json::json!({
            "model": self.api_model,
            "input": input,
            "encoding_format": "float",
        });
        if let Some(d) = self.dimensions {
            body["dimensions"] = d.into();
        }
        let sent = self
            .agent
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes());
        let mut resp = match sent {
            Ok(r) => r,
            Err(ureq::Error::Timeout(_)) => {
                return Err(self.fail(format!("no answer within {:?}", self.timeout)));
            }
            Err(e) => return Err(self.fail(format!("cannot reach the API: {e}"))),
        };
        let status = resp.status().as_u16();
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::Timeout(_) => {
                    self.fail(format!("no answer within {:?}", self.timeout))
                }
                e => self.fail(format!("its answer: {e}")),
            })?;
        if matches!(status, 401 | 403) {
            // The API's message quotes part of the key: left out.
            return Err(self.fail(format!(
                "the API refused the key in {} (HTTP {status})",
                self.key_env
            )));
        }
        let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        if !(200..300).contains(&status) {
            let said = answer["error"]["message"].as_str().unwrap_or_default();
            let said: String = said.replace(key, "***").chars().take(MAX_MESSAGE).collect();
            return Err(self.fail(if said.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {said}")
            }));
        }
        let mut got: Vec<(usize, Vec<f32>)> = Vec::with_capacity(input.len());
        for item in answer["data"].as_array().into_iter().flatten() {
            let index = item["index"].as_u64().and_then(|i| usize::try_from(i).ok());
            let vector: Option<Vec<f32>> = item["embedding"]
                .as_array()
                .and_then(|v| v.iter().map(|x| x.as_f64().map(|x| x as f32)).collect());
            match (index, vector) {
                (Some(i), Some(v)) => got.push((i, v)),
                _ => return Err(self.fail("its answer is not a list of embeddings")),
            }
        }
        got.sort_by_key(|(i, _)| *i);
        let in_order = got.iter().enumerate().all(|(n, (i, _))| n == *i);
        if got.len() != input.len()
            || !in_order
            || got.iter().any(|(_, v)| v.len() != self.dimension)
        {
            return Err(self.fail(format!(
                "expected {} vectors of {} numbers",
                input.len(),
                self.dimension
            )));
        }
        Ok(got.into_iter().map(|(_, v)| v).collect())
    }
}

impl Embedder for OpenAi {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        // The API refuses an empty input; an empty text gets zeros.
        let mut out = vec![vec![0f32; self.dimension]; texts.len()];
        let todo: Vec<usize> = (0..texts.len())
            .filter(|&i| !texts[i].trim().is_empty())
            .collect();
        if todo.is_empty() {
            return Ok(out);
        }
        let key = self.key()?;
        for batch in todo.chunks(BATCH) {
            let input: Vec<&str> = batch.iter().map(|&i| texts[i]).collect();
            for (&i, v) in batch.iter().zip(self.request(&key, &input)?) {
                out[i] = v;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A stand-in for the API on the loopback: each request's
    /// `Authorization` header and JSON body are kept, and `answer` gives
    /// the status and body to send back (or `None` to never answer).
    struct StandIn {
        url: String,
        seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    fn stand_in(
        answer: impl Fn(&serde_json::Value) -> Option<(u16, String)> + Send + 'static,
    ) -> StandIn {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut reader = BufReader::new(conn.try_clone().expect("clone"));
                let (mut auth, mut length) = (String::new(), 0);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':') {
                        match n.to_ascii_lowercase().as_str() {
                            "authorization" => auth = v.trim().to_owned(),
                            "content-length" => length = v.trim().parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let reply = answer(&body);
                kept.lock().expect("seen").push((auth, body));
                let Some((status, text)) = reply else {
                    // Hold the connection open without answering.
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(30));
                        drop(conn);
                    });
                    continue;
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        StandIn { url, seen }
    }

    /// Answers each input with `[its length, its index in the batch]`,
    /// listed last to first so the order comes from `index`.
    fn lengths(body: &serde_json::Value) -> Option<(u16, String)> {
        let input = body["input"].as_array()?;
        let data: Vec<serde_json::Value> = input
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| {
                serde_json::json!({
                    "object": "embedding",
                    "index": i,
                    "embedding": [t.as_str().unwrap_or_default().len(), i],
                })
            })
            .collect();
        Some((
            200,
            serde_json::json!({"object": "list", "data": data}).to_string(),
        ))
    }

    /// The environment the tests' embedders see.
    fn env(name: &str) -> Option<std::ffi::OsString> {
        match name {
            "TEST_KEY" => Some("sk-test-key".into()),
            "TEST_BLANK" => Some(" ".into()),
            _ => None,
        }
    }

    fn embedder(url: &str, key_env: &str, timeout: Duration) -> OpenAi {
        let mut e =
            OpenAi::new("test-model".into(), key_env.into(), url, Some(2), timeout).expect("new");
        e.env = env;
        e
    }

    #[test]
    fn texts_go_in_batches_with_the_key_and_come_back_in_order() {
        let api = stand_in(lengths);
        let e = embedder(&api.url, "TEST_KEY", Duration::from_secs(10));
        assert_eq!(e.model(), "openai:test-model");
        let texts: Vec<String> = (0..201).map(|i| "x".repeat(i % 7 + 1)).collect();
        let mut refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        refs[5] = "  ";
        let got = e.embed(&refs).expect("embed");
        assert_eq!(got.len(), 201);
        assert_eq!(got[0], vec![1.0, 0.0]);
        assert_eq!(got[5], vec![0.0, 0.0], "an empty text is not sent");
        // The last of 200 texts sent: the eighth of the third batch.
        assert_eq!(got[200], vec![(200 % 7 + 1) as f32, 7.0]);
        let seen = api.seen.lock().expect("seen");
        let sizes: Vec<usize> = seen
            .iter()
            .map(|(_, b)| b["input"].as_array().map_or(0, Vec::len))
            .collect();
        assert_eq!(sizes, vec![96, 96, 8]);
        for (auth, body) in seen.iter() {
            assert_eq!(auth, "Bearer sk-test-key");
            assert_eq!(body["model"], "test-model");
            assert_eq!(body["dimensions"], 2);
        }
        drop(seen);
        assert_eq!(e.embed(&[""]).expect("nothing to send"), vec![vec![0.0; 2]]);
        assert_eq!(api.seen.lock().expect("seen").len(), 3);
    }

    #[test]
    fn a_missing_key_a_refused_key_and_no_answer_are_clear_problems() {
        let api = stand_in(lengths);
        for unset in ["TEST_UNSET", "TEST_BLANK"] {
            let e = embedder(&api.url, unset, Duration::from_secs(10));
            let err = e.embed(&["tabs"]).expect_err("no key").to_string();
            assert!(err.contains(unset), "{err}");
            assert!(err.contains("not set"), "{err}");
        }
        assert!(api.seen.lock().expect("seen").is_empty(), "nothing sent");

        let refused = stand_in(|_| {
            Some((
                401,
                r#"{"error":{"message":"Incorrect API key provided: sk-test-key"}}"#.into(),
            ))
        });
        let e = embedder(&refused.url, "TEST_KEY", Duration::from_secs(10));
        let err = e.embed(&["tabs"]).expect_err("refused").to_string();
        assert!(
            err.contains("refused the key in TEST_KEY (HTTP 401)"),
            "{err}"
        );
        assert!(!err.contains("sk-test"), "{err}");

        let broken = stand_in(|_| {
            Some((
                500,
                r#"{"error":{"message":"overloaded, key sk-test-key"}}"#.into(),
            ))
        });
        let e = embedder(&broken.url, "TEST_KEY", Duration::from_secs(10));
        let err = e.embed(&["tabs"]).expect_err("500").to_string();
        assert!(err.contains("HTTP 500: overloaded"), "{err}");
        assert!(!err.contains("sk-test"), "{err}");

        let short = stand_in(|_| Some((200, r#"{"data":[]}"#.into())));
        let e = embedder(&short.url, "TEST_KEY", Duration::from_secs(10));
        let err = e.embed(&["tabs"]).expect_err("no vectors").to_string();
        assert!(err.contains("expected 1 vectors of 2 numbers"), "{err}");

        // Nothing listens on a port that was just freed.
        let port = TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("port")
            .port();
        let e = embedder(
            &format!("http://127.0.0.1:{port}/v1"),
            "TEST_KEY",
            Duration::from_secs(10),
        );
        let err = e.embed(&["tabs"]).expect_err("unreachable").to_string();
        assert!(err.contains("cannot reach the API"), "{err}");

        let silent = stand_in(|_| None);
        let e = embedder(&silent.url, "TEST_KEY", Duration::from_secs(1));
        let started = std::time::Instant::now();
        let err = e.embed(&["tabs"]).expect_err("timeout").to_string();
        assert!(err.contains("no answer within 1s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_model_of_unknown_length_needs_a_dimension() {
        let small = OpenAi::new(
            "text-embedding-3-small".into(),
            "K".into(),
            "https://api.openai.com/v1",
            None,
            Duration::from_secs(1),
        )
        .expect("known");
        assert_eq!(small.dimension(), 1536);
        assert_eq!(small.endpoint, "https://api.openai.com/v1/embeddings");
        assert!(
            OpenAi::new(
                "mystery".into(),
                "K".into(),
                "https://x/v1",
                None,
                Duration::from_secs(1)
            )
            .is_err()
        );
    }
}
