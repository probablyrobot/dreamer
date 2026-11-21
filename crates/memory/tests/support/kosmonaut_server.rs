//! A Kosmonaut server of a test's own: the `kosmonaut-server` program
//! `DREAMER_KOSMONAUT_SERVER` names (built from the pinned source with its
//! `grpc` feature), on a free loopback port, with a new database in a
//! temporary directory. Authentication is off unless asked for; then the
//! server has no users, so it refuses every token. Stopped and removed on
//! drop. Without the variable, tests that need a server say so and pass.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct TestServer {
    pub url: String,
    port: u16,
    dir: PathBuf,
    program: PathBuf,
    child: Option<Child>,
}

impl TestServer {
    pub fn start(name: &str, auth: bool) -> Option<TestServer> {
        let Some(program) = std::env::var_os("DREAMER_KOSMONAUT_SERVER") else {
            eprintln!("{name}: skipped, DREAMER_KOSMONAUT_SERVER is not set");
            return None;
        };
        let program = PathBuf::from(program);
        assert!(
            program.is_file(),
            "DREAMER_KOSMONAUT_SERVER={} is not a program",
            program.display()
        );
        let dir = std::env::temp_dir().join(format!("drm-kserver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("server dir");
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("a free port")
            .port();
        let auth = if auth {
            format!(
                "auth:\n  mode: required\n  hmac_secret_file: {}\n",
                dir.join("secret").display()
            )
        } else {
            "auth:\n  mode: disabled\n".to_owned()
        };
        std::fs::write(
            dir.join("server.yaml"),
            format!(
                "listeners:\n  - addr: 127.0.0.1:{port}\ndatabase:\n  path: {}\n{auth}\
                 logging:\n  level: warn\n",
                dir.join("data").display()
            ),
        )
        .expect("server config");
        let mut s = TestServer {
            url: format!("grpc://127.0.0.1:{port}"),
            port,
            dir,
            program,
            child: None,
        };
        s.restart();
        Some(s)
    }

    /// Start it again on the same port and database.
    pub fn restart(&mut self) {
        assert!(self.child.is_none(), "already running");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("server.log"))
            .expect("server log");
        let mut child = Command::new(&self.program)
            .arg("--config")
            .arg(self.dir.join("server.yaml"))
            // Its gRPC access log goes to `./logs` unless this says where.
            .env("KOSMONAUT_LOGS_DIR", self.dir.join("logs"))
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("kosmonaut-server");
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default();
                panic!("kosmonaut-server exited ({status}):\n{log}");
            }
            assert!(Instant::now() < deadline, "kosmonaut-server never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child = Some(child);
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
