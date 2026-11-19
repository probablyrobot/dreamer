//! Embedders: text to vectors, for a provider that can recall by meaning
//! as well as by words. None is used unless the settings name one.
//!
//! - [`StaticVectors`]: a table of word vectors read from a file (the
//!   word2vec text format that fastText and GloVe use, and that a
//!   model2vec model can be exported to). A text's vector is the mean of
//!   its words' vectors. No model runs and nothing leaves the machine.
//! - [`Command`]: a program the user names that runs a local model. It
//!   reads a JSON array of texts on standard input and writes a JSON array
//!   of vectors, one per text, on standard output.
//!
//! Vectors from different models are never compared: each embedder names
//! its [`model`](Embedder::model), and a provider keeps that name beside
//! every vector it stores.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::Error;
use crate::search::words;

/// Turns texts into vectors of one dimension.
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// Names the model, so vectors from another one are never compared
    /// with these: `static:<file name>` or `local:<program>`.
    fn model(&self) -> &str;

    /// One vector per text, in order, each [`dimension`](Self::dimension)
    /// long. A text the model has nothing for gets zeros.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error>;

    fn dimension(&self) -> usize;
}

/// Cosine similarity, or 0 when either vector is all zeros.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Word vectors from a file.
#[derive(Debug)]
pub struct StaticVectors {
    model: String,
    dimension: usize,
    words: HashMap<String, Vec<f32>>,
}

/// The largest vector file read, so a wrong path cannot fill memory.
const MAX_FILE: u64 = 1 << 30;

impl StaticVectors {
    /// Read `path`: one word per line followed by its numbers, separated
    /// by spaces; a first line of two numbers (count, dimension) is a
    /// header and skipped. Words are matched in lower case.
    pub fn load(path: &Path) -> Result<StaticVectors, Error> {
        let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
        let size = std::fs::metadata(path).map_err(io)?.len();
        if size > MAX_FILE {
            return Err(Error::Invalid(format!(
                "{}: {size} bytes is more than a vector file may hold",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(path).map_err(io)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        StaticVectors::parse(&format!("static:{name}"), &text)
            .map_err(|m| Error::Invalid(format!("{}: {m}", path.display())))
    }

    /// The same as [`load`](Self::load), from text.
    pub fn parse(model: &str, text: &str) -> Result<StaticVectors, String> {
        let mut words = HashMap::new();
        let mut dimension = 0;
        for (i, line) in text.lines().enumerate() {
            let mut parts = line.split_whitespace();
            let Some(word) = parts.next() else { continue };
            let numbers: Result<Vec<f32>, _> = parts.map(str::parse::<f32>).collect();
            let Ok(v) = numbers else {
                return Err(format!("line {}: not a word and its numbers", i + 1));
            };
            if i == 0 && v.len() == 1 && word.parse::<u64>().is_ok() {
                continue;
            }
            if v.is_empty() {
                return Err(format!("line {}: `{word}` has no numbers", i + 1));
            }
            if dimension == 0 {
                dimension = v.len();
            } else if v.len() != dimension {
                return Err(format!(
                    "line {}: `{word}` has {} numbers, the others {dimension}",
                    i + 1,
                    v.len()
                ));
            }
            words.entry(word.to_lowercase()).or_insert(v);
        }
        if words.is_empty() {
            return Err("no word vectors".into());
        }
        Ok(StaticVectors {
            model: model.to_owned(),
            dimension,
            words,
        })
    }
}

impl Embedder for StaticVectors {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut sum = vec![0f32; self.dimension];
                let mut n = 0u32;
                for w in words(t) {
                    if let Some(v) = self.words.get(&w) {
                        for (s, x) in sum.iter_mut().zip(v) {
                            *s += x;
                        }
                        n += 1;
                    }
                }
                if n > 0 {
                    for s in &mut sum {
                        *s /= n as f32;
                    }
                }
                sum
            })
            .collect())
    }
}

/// An embedder made on first use, so a large vector file is read when
/// memory is first asked for, off the daemon's event loop, and not when
/// the settings are read. A failure to make it is kept and returned by
/// every call.
pub struct Lazy {
    model: String,
    make: Box<dyn Fn() -> Result<Box<dyn Embedder>, Error> + Send + Sync>,
    made: std::sync::OnceLock<Result<Box<dyn Embedder>, Error>>,
}

impl std::fmt::Debug for Lazy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lazy")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl Lazy {
    /// `model` must be the name the made embedder gives.
    pub fn new(
        model: String,
        make: impl Fn() -> Result<Box<dyn Embedder>, Error> + Send + Sync + 'static,
    ) -> Lazy {
        Lazy {
            model,
            make: Box::new(make),
            made: std::sync::OnceLock::new(),
        }
    }

    /// Word vectors from `path`, read on first use.
    pub fn static_vectors(path: PathBuf) -> Lazy {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Lazy::new(format!("static:{name}"), move || {
            StaticVectors::load(&path).map(|v| Box::new(v) as Box<dyn Embedder>)
        })
    }

    fn get(&self) -> Result<&dyn Embedder, Error> {
        match self.made.get_or_init(|| (self.make)()) {
            Ok(e) => Ok(e.as_ref()),
            Err(e) => Err(e.clone()),
        }
    }
}

impl Embedder for Lazy {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> usize {
        self.get().map_or(0, Embedder::dimension)
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        self.get()?.embed(texts)
    }
}

/// A local model behind a program: `argv[0]` with the rest as arguments,
/// run once per call. It gets a JSON array of strings on standard input
/// and must print a JSON array of number arrays, one per string, all
/// `dimension` long, and exit 0 within the timeout.
#[derive(Debug)]
pub struct Command {
    model: String,
    argv: Vec<String>,
    dimension: usize,
    timeout: Duration,
}

impl Command {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    pub fn new(argv: Vec<String>, dimension: usize, timeout: Duration) -> Result<Command, Error> {
        let program = argv
            .first()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| Error::Invalid("a local embedder needs a command".into()))?;
        if dimension == 0 {
            return Err(Error::Invalid(
                "a local embedder needs its dimension".into(),
            ));
        }
        let name = PathBuf::from(program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.clone());
        Ok(Command {
            model: format!("local:{name}"),
            argv,
            dimension,
            timeout,
        })
    }
}

impl Embedder for Command {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let fail = |m: String| Error::Io(format!("embedder `{}`: {m}", self.argv[0]));
        let input = serde_json::to_vec(texts).map_err(|e| fail(e.to_string()))?;
        let mut child = std::process::Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| fail(e.to_string()))?;
        // Written from a thread, so a program that prints before it has
        // read everything cannot dead-lock with us.
        let mut stdin = child.stdin.take().ok_or_else(|| fail("no stdin".into()))?;
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| fail("no stdout".into()))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let r = std::io::Read::read_to_end(&mut stdout, &mut out).map(|_| out);
            let _ = tx.send(r);
        });
        let out = match rx.recv_timeout(self.timeout) {
            Ok(r) => r.map_err(|e| fail(e.to_string()))?,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!("no answer within {:?}", self.timeout)));
            }
        };
        let _ = writer.join();
        let status = child.wait().map_err(|e| fail(e.to_string()))?;
        if !status.success() {
            return Err(fail(format!("exited with {status}")));
        }
        let vectors: Vec<Vec<f32>> =
            serde_json::from_slice(&out).map_err(|e| fail(format!("its answer: {e}")))?;
        if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != self.dimension) {
            return Err(fail(format!(
                "expected {} vectors of {} numbers",
                texts.len(),
                self.dimension
            )));
        }
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_vectors_average_the_words_they_know() {
        let v = StaticVectors::parse("static:t", "3 2\ntabs 1 0\nSpaces 0 1\nindent 1 1\n")
            .expect("parse");
        assert_eq!(v.dimension(), 2);
        let got = v
            .embed(&["Tabs and spaces", "nothing known", ""])
            .expect("embed");
        assert_eq!(got, vec![vec![0.5, 0.5], vec![0.0, 0.0], vec![0.0, 0.0]]);
        assert!((cosine(&got[0], &[1.0, 1.0]) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&got[1], &[1.0, 1.0]), 0.0);
        assert!(StaticVectors::parse("m", "a 1 2\nb 1\n").is_err());
        assert!(StaticVectors::parse("m", "a x\n").is_err());
        assert!(StaticVectors::parse("m", "").is_err());
    }

    #[test]
    fn a_lazy_embedder_reads_its_file_on_first_use() {
        let dir = std::env::temp_dir().join(format!("dreamer-embed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("words.vec");
        let lazy = Lazy::static_vectors(path.clone());
        assert_eq!(lazy.model(), "static:words.vec");
        // Written after the embedder was made: nothing was read yet.
        std::fs::write(&path, "tabs 1 0\n").expect("write");
        assert_eq!(lazy.dimension(), 2);
        assert_eq!(lazy.embed(&["tabs"]).expect("embed"), vec![vec![1.0, 0.0]]);
        let missing = Lazy::static_vectors(dir.join("none.vec"));
        assert!(missing.embed(&["tabs"]).is_err());
        assert_eq!(missing.dimension(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_embedder_speaks_json_and_is_checked() {
        let sh = |script: &str, dim| {
            Command::new(
                vec!["sh".into(), "-c".into(), script.into()],
                dim,
                Duration::from_secs(10),
            )
            .expect("command")
        };
        let ok = sh("cat >/dev/null; printf '[[1,0],[0,1]]'", 2);
        assert_eq!(ok.model(), "local:sh");
        assert_eq!(
            ok.embed(&["a", "b"]).expect("embed"),
            vec![vec![1.0, 0.0], vec![0.0, 1.0]]
        );
        assert!(
            sh("cat >/dev/null; printf '[[1,0]]'", 2)
                .embed(&["a", "b"])
                .is_err()
        );
        assert!(sh("cat >/dev/null; exit 3", 2).embed(&["a"]).is_err());
        assert!(Command::new(vec![], 2, Duration::from_secs(1)).is_err());
    }
}
