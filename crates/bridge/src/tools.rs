//! The system's own tools the bridge runs: `curl.exe` and `tar.exe`, part of
//! Windows 10 and 11, called by their full paths so that no other program on
//! the path answers. Stockfish's installation downloads and unpacks with them
//! (#13, #322), and the app's failure reports are posted with `curl.exe`
//! (#333), so that the bridge carries no HTTP client of its own.

use std::path::Path;

use crate::folders;

/// Posts the file `body` to `url` with `headers`, through `curl.exe` over
/// HTTPS only, and answers the HTTP status of the answer. `Err` when no
/// answer came: no network, no such host, or a time-out. The tool runs
/// outside the Store package, so it is given the path at which it finds the
/// file ([`folders::outside`], #289).
pub fn post(url: &str, headers: &[String], body: &Path) -> Result<u16, String> {
    let mut curl = system_tool("curl.exe")?;
    curl.args(["--silent", "--show-error", "--proto", "=https", "--request", "POST"]).args([
        "--connect-timeout",
        "15",
        "--max-time",
        "60",
    ]);
    for header in headers {
        curl.arg("--header").arg(header);
    }
    let mut data = std::ffi::OsString::from("@");
    data.push(folders::outside(body));
    curl.arg("--data-binary").arg(data).args(["--output", if cfg!(windows) { "NUL" } else { "/dev/null" }]);
    curl.args(["--write-out", "%{http_code}"]).arg(url);
    let written = run(curl, "curl.exe", "The report was not sent", 16)?;
    status(&written)
}

/// The status `curl --write-out %{http_code}` wrote; `000` is no answer.
fn status(written: &[u8]) -> Result<u16, String> {
    match String::from_utf8_lossy(written).trim().parse() {
        Ok(0) | Err(_) => Err("The report was not sent: no answer".to_string()),
        Ok(code) => Ok(code),
    }
}

/// Runs the tool `name` as `command` and answers what it wrote, at most
/// `limit` bytes: a tool that writes more is stopped, so that nothing it
/// writes is held beyond that. A failure is `failed` with the tool's exit
/// status and the reason it gave.
pub fn run(mut command: std::process::Command, name: &str, failed: &str, limit: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{name}: no output"));
    };
    // The error output is read beside the output, so that neither pipe
    // stalls the tool, and only its end is kept.
    let errors = std::thread::spawn(move || {
        let (mut tail, mut chunk) = (Vec::new(), [0u8; 8192]);
        while let Ok(n @ 1..) = stderr.read(&mut chunk) {
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > ERRORS {
                tail.drain(..tail.len() - ERRORS);
            }
        }
        tail
    });
    let mut out = Vec::new();
    let read = stdout.take(limit.saturating_add(1)).read_to_end(&mut out);
    if out.len() as u64 > limit {
        let _ = child.kill();
        let _ = child.wait();
        let _ = errors.join();
        return Err(format!("{failed}: more than {limit} bytes"));
    }
    let status = child.wait().map_err(|e| format!("{name}: {e}"))?;
    let stderr = errors.join().unwrap_or_default();
    read.map_err(|e| format!("{name}: {e}"))?;
    if status.success() { Ok(out) } else { Err(failure(failed, &status.to_string(), &stderr)) }
}

/// The most a tool that should write nothing, `curl.exe` writing to a file
/// or `tar.exe` unpacking, may write before it is stopped.
pub const QUIET: u64 = 1 << 20;

/// The most of a tool's error output kept: its end, where the reason is.
const ERRORS: usize = 64 << 10;

/// `failed` with the tool's exit status `status` and the last line of its
/// error output `stderr`, such as curl's "(23) Failure writing output to
/// destination", at most [`REASON`] characters of it.
fn failure(failed: &str, status: &str, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let reason = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or_default();
    let reason: String = reason.chars().take(REASON).collect();
    if reason.is_empty() { format!("{failed} ({status})") } else { format!("{failed} ({status}): {reason}") }
}

/// The most of a tool's reason a failure repeats.
const REASON: usize = 300;

#[cfg(windows)]
pub fn system_tool(name: &str) -> Result<std::process::Command, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?;
    let mut command = std::process::Command::new(std::path::PathBuf::from(root).join("System32").join(name));
    command.creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(not(windows))]
pub fn system_tool(name: &str) -> Result<std::process::Command, String> {
    Err(format!("{name} is a tool of Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool's output is read up to the limit and no further: one that
    /// writes more, without end here, is stopped and refused (#322). Its
    /// error output's last line is the reason, however long that output is.
    #[cfg(unix)]
    #[test]
    fn a_tool_is_read_up_to_its_limit_and_stopped_beyond_it() {
        let sh = |script: &str| {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", script]);
            command
        };
        assert_eq!(run(sh("printf hello"), "sh", "The lookup failed", 5), Ok(b"hello".to_vec()));
        assert_eq!(
            run(sh("printf hello"), "sh", "The lookup failed", 4),
            Err("The lookup failed: more than 4 bytes".into())
        );
        let endless = run(sh("yes"), "sh", "The lookup failed", 1 << 20);
        assert_eq!(endless, Err(format!("The lookup failed: more than {} bytes", 1 << 20)));
        // Endless error output stalls nothing, and is cut to its end.
        let noisy = run(
            sh("head -c 300000 /dev/zero | tr '\\0' x >&2; echo >&2; echo 'curl: (6) no host' >&2; exit 6"),
            "sh",
            "The lookup failed",
            16,
        );
        assert_eq!(noisy, Err("The lookup failed (exit status: 6): curl: (6) no host".into()));
        assert!(
            run(std::process::Command::new("/nonexistent/tool"), "tool", "x", 1).unwrap_err().starts_with("tool: ")
        );
    }

    /// A failed tool's own reason reaches the message, not its exit code
    /// alone (#289).
    #[test]
    fn a_failure_names_the_tools_reason() {
        let curl = b"Warning: Failed to open the file C:\\x\\.download.zip: No such file or directory\r\n\
                     curl: (23) client returned ERROR on write of 1369 bytes\r\n\r\n";
        assert_eq!(
            failure("The download failed", "exit code: 23", curl),
            "The download failed (exit code: 23): curl: (23) client returned ERROR on write of 1369 bytes"
        );
        assert_eq!(failure("Unpacking failed", "exit code: 1", b" \n"), "Unpacking failed (exit code: 1)");
        let long = failure("The download failed", "exit code: 6", "é".repeat(1000).as_bytes());
        assert_eq!(long, format!("The download failed (exit code: 6): {}", "é".repeat(REASON)));
        assert!(failure("x", "y", b"\xff\xfe broken").ends_with("broken"), "text that is not UTF-8 is kept readable");
    }

    #[test]
    fn the_status_curl_wrote() {
        assert_eq!(status(b"200"), Ok(200));
        assert_eq!(status(b"429\n"), Ok(429));
        assert!(status(b"000").is_err(), "no answer");
        assert!(status(b"").is_err());
    }
}
