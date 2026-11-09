use std::{
    fs,
    future::{Future, poll_fn},
    io::Write,
    process::{Child, ChildStdin, Command, Stdio},
    task::Poll,
    time::Duration,
};

use isponsorblocktv::setup::{PromptIo, SetupError};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_isponsorblocktvrust"))
}

#[test]
fn help_and_version_need_no_configuration_or_python() {
    for flag in ["--help", "--version"] {
        let result = binary().arg(flag).env("PATH", "").output().unwrap();
        assert!(result.status.success());
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(output.contains(if flag == "--version" {
            "2.11.0"
        } else {
            "setup-cli"
        }));
        assert!(!output.contains("self update"));
    }
}

#[test]
fn piped_setup_saves_only_after_explicit_confirmation() {
    let data = tempfile::tempdir().unwrap();
    let mut child = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Proxy, devices, key, categories, whitelist, minimum, reporting, mute,
    // skip, autoplay, URL, then explicit save.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"n\nn\nn\nsponsor hook\nn\nn\ny\nn\nn\ny\n\ny\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(data.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(
        json["skip_categories"],
        serde_json::json!(["sponsor", "hook"])
    );
    assert_eq!(json["skip_count_tracking"], true);
    assert_eq!(json["mute_ads"], false);
    assert_eq!(json["skip_ads"], false);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Config saved to"));
}

#[test]
fn eof_leaves_existing_configuration_untouched() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("config.json");
    let original = b"{\"future\":18446744073709551617}";
    fs::write(&path, original).unwrap();
    let output = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn malformed_configuration_is_not_replaced() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("config.json");
    fs::write(&path, b"{invalid").unwrap();
    let result = binary()
        .args(["--data", data.path().to_str().unwrap(), "setup-cli"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(fs::read(path).unwrap(), b"{invalid");
}

#[test]
fn invalid_arguments_exit_two() {
    assert_eq!(
        binary()
            .arg("--not-an-option")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn legacy_precedence_and_noninteractive_terminal_are_preserved() {
    let data = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--setup"],
        vec!["--setup-cli", "--setup"],
        vec!["setup"],
    ] {
        let output = binary()
            .arg("--data")
            .arg(data.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Use setup-cli in a non-interactive terminal")
        );
    }
    for arguments in [
        vec!["--setup-cli"],
        vec!["--setup", "--setup-cli", "setup-cli"],
    ] {
        let output = binary()
            .arg("--data")
            .arg(data.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(130));
    }
    assert!(!data.path().join("config.json").exists());
    let output = binary()
        .arg("--data")
        .arg(data.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "default command validates service configuration"
    );
    assert_eq!(
        binary()
            .env("PYAPP", "1")
            .arg("self")
            .arg("update")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn invalid_log_levels_fail_before_setup_even_with_debug_without_exposing_values() {
    for arguments in [vec!["setup-cli"], vec!["--debug", "setup-cli"]] {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("config.json");
        let original = br#"{"log_level":"sensitive-invalid-level"}"#;
        fs::write(&path, original).unwrap();
        let output = binary()
            .arg("--data")
            .arg(data.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("configuration field log_level:"));
        assert!(!error.contains("sensitive-invalid-level"));
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

// A subprocess permits a real global subscriber without affecting concurrent
// tests. It uses the same prepared-app/filter seam as the binary, with injected
// loopback endpoints rather than production services.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_logging_child_entry() {
    use clap::Parser;
    use isponsorblocktv::{
        AppContext, PreparedApp,
        cli::Cli,
        config::{Environment, Platform},
        http::{Endpoints, HttpClients},
        run_app,
    };

    let Ok(arguments) = std::env::var("ISPBTV_LOGGING_TEST_ARGUMENTS") else {
        return;
    };
    let arguments: Vec<String> = serde_json::from_str(&arguments).unwrap();
    let cli = Cli::try_parse_from(arguments).unwrap();
    let http_tracing = cli.http_tracing;
    let data = cli.data.clone().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base: url::Url = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let app = PreparedApp::load(
        cli,
        AppContext {
            environment: Environment {
                platform: Platform::Linux,
                data_dir: None,
                docker: false,
                home: None,
                xdg_data_home: None,
                local_app_data: None,
            },
            endpoints: Endpoints {
                lounge: base.clone(),
                youtube: base.clone(),
                sponsorblock: base.clone(),
            },
            shutdown,
            prompt: None,
        },
    )
    .unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(app.log_level().filter())
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    // Run after the snapshot's on-disk source becomes invalid: startup must
    // neither reload it nor depend on it after selecting the logging level.
    fs::write(data.join("config.json"), b"{invalid-after-preparation").unwrap();
    tokio::spawn(async {
        tracing::error!(target: "isponsorblocktv::logging_contract", "severity probe");
        tracing::warn!(target: "isponsorblocktv::logging_contract", "severity probe");
        tracing::info!(target: "isponsorblocktv::logging_contract", "severity probe");
        tracing::debug!(target: "isponsorblocktv::logging_contract", "severity probe");
        tracing::trace!(target: "isponsorblocktv::logging_contract", "severity probe");
        tracing::error!(target: "reqwest::logging_contract", "dependency probe");
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(10), async {
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
                assert!(request.len() < 8192);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let clients = HttpClients::new(false, http_tracing).unwrap();
        let request = clients.cloud.request(reqwest::Method::GET, base);
        assert_eq!(clients.cloud.execute(request).await.unwrap().status, 200);
        server.await.unwrap();
        run_app(app).await.unwrap();
    })
    .await
    .expect("loopback logging scenario must finish");
}

#[test]
fn configured_levels_filter_runtime_children_and_opt_in_http_traces() {
    // Expected severities are independent literal thresholds, not filter strings.
    for (level, debug, expected) in [
        (None, false, &["ERROR", "WARN", "INFO"][..]),
        (Some("off"), false, &[][..]),
        (Some("error"), false, &["ERROR"][..]),
        (Some("warn"), false, &["ERROR", "WARN"][..]),
        (Some("info"), false, &["ERROR", "WARN", "INFO"][..]),
        (
            Some("debug"),
            false,
            &["ERROR", "WARN", "INFO", "DEBUG"][..],
        ),
        (
            Some("trace"),
            false,
            &["ERROR", "WARN", "INFO", "DEBUG", "TRACE"][..],
        ),
        (Some("off"), true, &["ERROR", "WARN", "INFO", "DEBUG"][..]),
        (Some("trace"), true, &["ERROR", "WARN", "INFO", "DEBUG"][..]),
    ] {
        for http_tracing in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let mut config = serde_json::json!({
                "devices": [{"screen_id": "synthetic-screen"}]
            });
            if let Some(level) = level {
                config["log_level"] = serde_json::json!(level);
            }
            fs::write(
                data.path().join("config.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .unwrap();
            let mut arguments = vec![
                "isponsorblocktvrust".to_owned(),
                "--data".to_owned(),
                data.path().to_str().unwrap().to_owned(),
            ];
            if debug {
                arguments.push("--debug".into());
            }
            if http_tracing {
                arguments.push("--http-tracing".into());
            }
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "configured_logging_child_entry", "--nocapture"])
                .env(
                    "ISPBTV_LOGGING_TEST_ARGUMENTS",
                    serde_json::to_string(&arguments).unwrap(),
                )
                .env("RUST_LOG", "trace")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let logs = String::from_utf8(output.stderr).unwrap();
            assert!(output.status.success(), "{logs}");
            let actual: Vec<_> = logs
                .lines()
                .filter(|line| line.contains("isponsorblocktv::logging_contract:"))
                .map(|line| line.split_whitespace().next().unwrap())
                .collect();
            assert_eq!(actual, expected, "level={level:?}, debug={debug}");
            assert!(!logs.contains("reqwest::logging_contract:"));
            let runtime_logged = logs.lines().any(|line| {
                line.contains("isponsorblocktv::runtime:")
                    && line.split_whitespace().next() == Some("INFO")
            });
            let actor_logged = logs.lines().any(|line| {
                line.contains("isponsorblocktv::runtime::device:")
                    && line.split_whitespace().next() == Some("INFO")
            });
            assert_eq!(runtime_logged, expected.contains(&"INFO"));
            assert_eq!(actor_logged, expected.contains(&"INFO"));
            let http_logged = logs.lines().any(|line| {
                line.contains("isponsorblocktv::http:")
                    && line.split_whitespace().next() == Some("DEBUG")
                    && line.contains("status=200")
            });
            assert_eq!(http_logged, http_tracing && expected.contains(&"DEBUG"));
        }
    }
}

// Keep stdin outside Child: wait() otherwise closes it before waiting, which
// would turn this regression into an EOF test and hide a blocked-worker join.
struct OpenStdinChild {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Drop for OpenStdinChild {
    fn drop(&mut self) {
        // Also runs on assertion failure or timeout. Kill/reap before releasing
        // stdin so even the failure path cannot strand the test subprocess.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn native_stdin_cancellation_child_entry() {
    let Ok(address) = std::env::var("ISPBTV_NATIVE_STDIN_TEST_CONTROL") else {
        return;
    };
    let address: std::net::SocketAddr = address.parse().unwrap();
    assert!(address.ip().is_loopback());
    let mut control = TcpStream::connect(address).await.unwrap();
    let shutdown = CancellationToken::new();
    let mut prompt = PromptIo::stdio().unwrap();

    // A real line proves the native input worker has started and read the pipe,
    // rather than only cancelling a token before the worker ever runs.
    assert_eq!(
        prompt.read_line("", &shutdown).await.unwrap(),
        "native-worker-started"
    );
    {
        let next_line = prompt.read_line("", &shutdown);
        tokio::pin!(next_line);
        poll_fn(|cx| {
            assert!(
                next_line.as_mut().poll(cx).is_pending(),
                "open, empty stdin must leave the next read pending"
            );
            Poll::Ready(())
        })
        .await;
        control.write_all(b"reading").await.unwrap();

        let mut command = [0; 6];
        control.read_exact(&mut command).await.unwrap();
        assert_eq!(&command, b"cancel");
        shutdown.cancel();
        assert!(matches!(next_line.await, Err(SetupError::Interrupted)));
    }

    // This synchronous Drop must join both native workers. A timeout wrapped
    // around read_line alone would miss a hang here, especially on Windows.
    drop(prompt);
    control.write_all(b"joined").await.unwrap();
}

#[tokio::test]
async fn native_stdin_cancellation_joins_workers_while_pipe_stays_open() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_stdin_cancellation_child_entry",
            "--nocapture",
        ])
        .env(
            "ISPBTV_NATIVE_STDIN_TEST_CONTROL",
            listener.local_addr().unwrap().to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = OpenStdinChild { child, stdin: None };
    child.stdin = child.child.stdin.take();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"native-worker-started\n")
        .unwrap();

    timeout(Duration::from_secs(10), async {
        let (mut control, _) = listener.accept().await.unwrap();
        let mut ready = [0; 7];
        control.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"reading");
        // Cancellation travels over a different transport; stdin remains open
        // and receives neither EOF nor a Ctrl+C byte.
        control.write_all(b"cancel").await.unwrap();
        let mut cleanup = [0; 6];
        control.read_exact(&mut cleanup).await.unwrap();
        assert_eq!(&cleanup, b"joined");

        // std::process has no async wait. Poll only for process completion;
        // readiness and cancellation above use explicit socket handshakes.
        let mut poll_exit = tokio::time::interval(Duration::from_millis(10));
        loop {
            if let Some(status) = child.child.try_wait().unwrap() {
                assert!(status.success(), "native stdin helper failed: {status}");
                break;
            }
            poll_exit.tick().await;
        }
        assert!(child.stdin.is_some(), "stdin must stay owned through exit");
    })
    .await
    .expect("native stdin cancellation or worker cleanup exceeded 10 seconds");
}
