//! Where `temper serve`, started as a real process in an empty home, keeps
//! its one local database: in `TURSO_URL` when that is set, otherwise in its
//! data folder (`TEMPER_DATA_DIR`, or `$HOME/.local/share/temper` without
//! one). A server given its own data folder must never open the database
//! every other server on the machine shares by default, and keeps the Genesis
//! apps it installs in that folder too.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A started-and-stopped server's folders and what it printed.
struct Started {
    _dir: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    named: PathBuf,
    stdout: String,
}

impl Started {
    /// The database every server shares when given no folder of its own.
    fn shared_store(&self) -> PathBuf {
        self.home.join(".local/share/temper/agents.db")
    }

    fn data_store(&self) -> PathBuf {
        self.data.join("agents.db")
    }

    fn reported(&self, store: &Path) -> bool {
        self.stdout
            .contains(&format!("Storage: turso (file:{})", store.display()))
    }
}

/// Start `temper serve` in an empty home with `TEMPER_DATA_DIR` (`data`)
/// and `TURSO_URL` (`named`) set or not, wait until it listens, and stop it.
fn start(with_data_dir: bool, with_turso_url: bool) -> Started {
    start_and(with_data_dir, with_turso_url, &[], |_| {})
}

/// [`start`] with more variables, calling `while_up` with the server's port
/// once it listens.
fn start_and(
    with_data_dir: bool,
    with_turso_url: bool,
    variables: &[(&str, &str)],
    while_up: impl FnOnce(u16),
) -> Started {
    let dir = tempfile::tempdir().expect("temporary directory");
    let home = dir.path().join("home");
    let cwd = dir.path().join("cwd");
    let data = dir.path().join("data");
    let named = dir.path().join("named").join("store.db");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::create_dir_all(&cwd).expect("working directory");
    std::fs::create_dir_all(named.parent().expect("named parent")).expect("named directory");
    let stdout_path = dir.path().join("stdout.txt");
    let stderr_path = dir.path().join("stderr.txt");

    let mut command = Command::new(env!("CARGO_BIN_EXE_temper"));
    command
        .args(["serve", "--port", "0", "--no-observe"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .envs(variables.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("stderr file"),
        ));
    if with_data_dir {
        command.env("TEMPER_DATA_DIR", &data);
    }
    if with_turso_url {
        command.env("TURSO_URL", format!("file:{}", named.display()));
    }
    let mut process = Process(command.spawn().expect("start temper serve"));

    // A loaded workspace test run starts the server slowly; a server that
    // fails to start exits, and one that hangs runs into the deadline.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !read(&stdout_path)
        .lines()
        .any(|line| line.starts_with("Listening on "))
    {
        if let Some(status) = process.0.try_wait().expect("read child status") {
            panic!(
                "temper serve exited with {status} before listening:\n{}",
                read(&stderr_path)
            );
        }
        assert!(
            Instant::now() < deadline,
            "temper serve was not listening in time:\n{}",
            read(&stderr_path)
        );
        thread::sleep(Duration::from_millis(50));
    }
    let port = read(&stdout_path)
        .lines()
        .find_map(|line| line.strip_prefix("Listening on "))
        .and_then(|address| address.rsplit(':').next())
        .and_then(|port| port.trim().parse().ok())
        .expect("the listening line names a port");
    while_up(port);
    drop(process);

    let stdout = read(&stdout_path);
    Started {
        _dir: dir,
        home,
        data,
        named,
        stdout,
    }
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned()
}

const KEY: &str = "tmpr_serve-store-location-test-key";

/// Grant the key's operator `install_app`, then ask to install a pinned app
/// from a registry nobody listens on. The install makes its cache folder
/// before it fetches, so the fetch fails after the folder is made.
fn install_from_a_dead_registry(port: u16) {
    let base = format!("http://127.0.0.1:{port}");
    let when = "principal has agent_type && principal.agent_type == \"operator\" \
                && principal has agentTypeVerified && principal.agentTypeVerified == true";
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let client = reqwest::Client::new();
        let post = |path: &str, body: serde_json::Value| {
            client
                .post(format!("{base}{path}"))
                .bearer_auth(KEY)
                .header("X-Tenant-Id", "default")
                .json(&body)
                .send()
        };
        let granted = post(
            "/api/tenants/default/policies/create",
            serde_json::json!({
                "policy_id": "test-install-app",
                "cedar_text": format!(
                    "permit(principal is Agent, action == Action::\"install_app\", resource is App) when {{ {when} }};"
                ),
            }),
        )
        .await
        .expect("grant install_app");
        assert_eq!(granted.status(), 201, "{}", granted.text().await.unwrap_or_default());
        let installed = post(
            "/api/genesis/apps/install",
            serde_json::json!({
                "tenant": "default",
                "app_ref": "acme/notes@0123456789abcdef0123456789abcdef01234567",
                "follow_policy": "pinned",
            }),
        )
        .await
        .expect("ask to install");
        let status = installed.status();
        let answer = installed.text().await.unwrap_or_default();
        assert!(
            answer.contains("Genesis bundle fetch failed"),
            "the install did not reach the fetch: {status} {answer}"
        );
    });
}

#[test]
fn a_data_dir_without_turso_url_keeps_the_store_in_the_data_dir() {
    let started = start_and(
        true,
        false,
        &[
            ("TEMPER_API_KEY", KEY),
            ("TEMPER_GENESIS_REGISTRY_URL", "http://127.0.0.1:9"),
        ],
        install_from_a_dead_registry,
    );
    assert!(
        started.reported(&started.data_store()),
        "{}",
        started.stdout
    );
    assert!(started.data_store().is_file());
    assert!(
        !started.shared_store().exists(),
        "the server opened the shared store"
    );
    assert!(
        started.data.join("genesis-app-cache").is_dir(),
        "the Genesis app cache is not in the data folder"
    );
    assert!(
        !started
            .home
            .join(".local/share/temper/genesis-app-cache")
            .exists(),
        "the Genesis app cache went to the shared folder"
    );
}

#[test]
fn turso_url_alone_names_the_store() {
    let started = start(false, true);
    assert!(started.reported(&started.named), "{}", started.stdout);
    assert!(started.named.is_file());
    assert!(!started.shared_store().exists());
}

#[test]
fn turso_url_wins_over_the_data_dir() {
    let started = start(true, true);
    assert!(started.reported(&started.named), "{}", started.stdout);
    assert!(started.named.is_file());
    assert!(!started.data_store().exists());
    assert!(!started.shared_store().exists());
}

#[test]
fn neither_keeps_the_store_under_home() {
    let started = start(false, false);
    assert!(
        started.reported(&started.shared_store()),
        "{}",
        started.stdout
    );
    assert!(started.shared_store().is_file());
}
