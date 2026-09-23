//! `cbtool bridge`: the bridge in a console, started by the same code as
//! `oschess-bridge`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// A bridge process, stopped when the test ends however it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A fresh data folder for the bridge, and its settings when `port` is given.
fn home(name: &str, port: Option<u16>) -> PathBuf {
    let home = std::env::temp_dir().join(format!("cbtool-bridge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    if let Some(port) = port {
        std::fs::write(home.join("bridge.toml"), format!("port = {port}\n")).unwrap();
    }
    home
}

#[test]
fn bridge_serves_in_the_console_and_prints_the_pairing_link() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let home = home("serve", Some(port));
    let mut bridge = Running(
        Command::new(env!("CARGO_BIN_EXE_cbtool"))
            .args(["bridge", "--show-token", "--database", "missing.2cbh"])
            .env("OSCHESS_BRIDGE_HOME", &home)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut lines = Vec::new();
    for line in BufReader::new(bridge.0.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        let last = line.starts_with("pairing link: ");
        lines.push(line);
        if last {
            break;
        }
    }
    let link = lines.last().and_then(|l| l.strip_prefix("pairing link: ")).unwrap_or_else(|| panic!("{lines:?}"));
    let token = std::fs::read_to_string(home.join("token")).unwrap().trim().to_string();
    assert_eq!(link, format!("https://oschess.org/library?source=chessbase#cb-bridge={token}&port={port}"));
    assert!(lines[0].starts_with("oschess bridge ") && lines[0].contains(&format!(":{port}")), "{lines:?}");
    assert!(lines.iter().any(|l| l.ends_with("[missing] missing")), "{lines:?}");

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let request = format!(
        "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Origin: https://oschess.org\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains(r#""api":1"#), "{answer}");
    drop(bridge);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bridge_refuses_an_unknown_option_with_its_usage() {
    let home = home("usage", None);
    let out = Command::new(env!("CARGO_BIN_EXE_cbtool"))
        .args(["bridge", "--port", "1"])
        .env("OSCHESS_BRIDGE_HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown option --port"), "{err}");
    assert!(err.contains("usage: cbtool bridge [--database <path>]..."), "{err}");
    assert!(!home.join("bridge.toml").exists() && !home.join("token").exists(), "an option error creates nothing");
    let _ = std::fs::remove_dir_all(&home);
}
