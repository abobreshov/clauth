use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct RunningServer(Child);

impl Drop for RunningServer {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn api_serve_removes_its_socket_on_sigterm() {
    let home = tempfile::tempdir().expect("sandbox home");
    let socket = home.path().join(".tollgate/api.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_tollgate"))
        .args(["api", "serve", "--listen", "127.0.0.1:0"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start local API");
    let mut server = RunningServer(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        assert_eq!(server.0.try_wait().expect("poll server"), None);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists(), "local API socket did not appear");

    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(server.0.id().to_string())
        .status()
        .expect("signal local API");
    assert!(signal.success());
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let mut status = None;
    while status.is_none() && Instant::now() < exit_deadline {
        status = server.0.try_wait().expect("poll server after SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = status.expect("local API exits on SIGTERM");
    assert!(status.success() || status.signal() == Some(15));
    assert!(!socket.exists(), "local API left its socket after SIGTERM");
}

#[test]
fn api_serve_preserves_inherited_sigterm_ignore() {
    let home = tempfile::tempdir().expect("sandbox home");
    let socket = home.path().join(".tollgate/api.sock");
    let child = Command::new("/bin/sh")
        .args([
            "-c",
            "trap '' TERM; exec \"$1\" api serve --listen 127.0.0.1:0",
            "sh",
            env!("CARGO_BIN_EXE_tollgate"),
        ])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start local API with SIGTERM ignored");
    let mut server = RunningServer(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        assert_eq!(server.0.try_wait().expect("poll server"), None);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists(), "local API socket did not appear");

    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(server.0.id().to_string())
        .status()
        .expect("signal local API");
    assert!(signal.success());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.0.try_wait().expect("poll ignored signal"), None);
    assert!(socket.exists());
}

#[test]
fn embedded_local_api_removes_its_socket_on_sigterm() {
    let home = tempfile::tempdir().expect("sandbox home");
    let data_dir = home.path().join(".tollgate");
    std::fs::create_dir(&data_dir).expect("data directory");
    std::fs::write(
        data_dir.join("profiles.toml"),
        "profiles = []\n[local_api]\nlisten = \"127.0.0.1:0\"\n",
    )
    .expect("daemon config");
    let socket = data_dir.join("api.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_tollgate"))
        .arg("daemon")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("TOLLGATE_NO_UPDATE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start daemon");
    let mut daemon = RunningServer(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() && Instant::now() < deadline {
        assert_eq!(daemon.0.try_wait().expect("poll daemon"), None);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists(), "embedded local API socket did not appear");

    let signal = Command::new("kill")
        .arg("-TERM")
        .arg(daemon.0.id().to_string())
        .status()
        .expect("signal daemon");
    assert!(signal.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = None;
    while status.is_none() && Instant::now() < deadline {
        status = daemon.0.try_wait().expect("poll daemon after SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(status.expect("daemon exits on SIGTERM").signal(), Some(15));
    assert!(!socket.exists(), "embedded local API left its socket");
}
