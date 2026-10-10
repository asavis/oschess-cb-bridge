//! Installing the official Stockfish build (#13, #54, #322).
//!
//! The bridge carries no engine. When the user asks for one, it downloads an
//! official build from Stockfish's GitHub releases, over HTTPS only, checks
//! its size and SHA-256, and unpacks the executable and its licence into
//! `engines\stockfish-<version>` in the data folder. The build is the newest
//! release GitHub's API names ([`LATEST`], [`Build::released`]) once it is a
//! week old, with the size and SHA-256 GitHub gives for its asset, or the
//! build pinned in this release when that lookup fails. The app keeps a build it installed up to date the
//! same way, while the user lets it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{folders, sha256};

/// A processor architecture Stockfish publishes a Windows build for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Arm64,
}

/// An official build: pinned in this release, or named by GitHub's API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    /// The release, as its tag names it after `sf_`.
    pub version: Cow<'static, str>,
    pub arch: Arch,
    /// The release asset, named alike in every release.
    pub asset: &'static str,
    pub size: u64,
    pub sha256: Cow<'static, str>,
    /// The executable inside the archive, under `stockfish/`.
    pub exe: &'static str,
}

/// The builds this release installs: Stockfish 19, published 2026-09-05. Its
/// universal builds choose the processor's instructions themselves, and the
/// NNUE network is inside the executable.
pub const PINNED: [Build; 2] = [
    Build {
        version: Cow::Borrowed("19"),
        arch: Arch::X86_64,
        asset: "stockfish-windows-x86-64-universal.zip",
        size: 81_431_614,
        sha256: Cow::Borrowed("3c8bf1f9ea66a09350a40df4f632288285ac206d99f33ab5842c408fc30b48a7"),
        exe: "stockfish-windows-x86-64-universal.exe",
    },
    Build {
        version: Cow::Borrowed("19"),
        arch: Arch::Arm64,
        asset: "stockfish-windows-arm64-universal.zip",
        size: 80_190_536,
        sha256: Cow::Borrowed("8372ad3f0d7276deb2c70f801f541ec7db463219fc6d9c7592864e542aa4f401"),
        exe: "stockfish-windows-arm64-universal.exe",
    },
];

/// The licence file kept beside the executable.
pub const LICENCE: &str = "Copying.txt";

/// Stockfish's newest release, as GitHub's API answers it. GitHub leaves
/// drafts and pre-releases, such as the `stockfish-dev-…` builds, out of it.
pub const LATEST: &str = "https://api.github.com/repos/official-stockfish/Stockfish/releases/latest";

/// The largest answer [`LATEST`] may give: some 23 kB for Stockfish 19.
pub const MAX_ANSWER: u64 = 1 << 20;

/// The largest build a release may name: Stockfish 19's are some 80 MB.
pub const MAX_SIZE: u64 = 256 << 20;

impl Build {
    /// The pinned build for `arch`.
    pub fn for_arch(arch: Arch) -> &'static Build {
        PINNED.iter().find(|b| b.arch == arch).expect("a build for every architecture")
    }

    /// The build for `arch` of the release `version`, as its tag names it
    /// after `sf_`, with the `size` and SHA-256 that GitHub gives for its
    /// asset, which every release names as the pinned one does (#322). A
    /// version that is not digits and dots, a size of nothing or above
    /// [`MAX_SIZE`], and a digest that is not 64 hexadecimal digits are
    /// refused.
    pub fn released(arch: Arch, version: &str, size: u64, sha256: &str) -> Result<Build, String> {
        if !is_version(version) {
            return Err(format!("{version:?} is not a Stockfish version"));
        }
        if size == 0 || size > MAX_SIZE {
            return Err(format!("Stockfish {version} names a build of {size} bytes"));
        }
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("Stockfish {version} names no SHA-256"));
        }
        let pinned = Build::for_arch(arch);
        Ok(Build {
            version: Cow::Owned(version.into()),
            arch,
            asset: pinned.asset,
            size,
            sha256: Cow::Owned(sha256.to_ascii_lowercase()),
            exe: pinned.exe,
        })
    }

    /// The asset's fixed address.
    pub fn url(&self) -> String {
        format!("https://github.com/official-stockfish/Stockfish/releases/download/sf_{}/{}", self.version, self.asset)
    }

    /// The folder this build installs into, in the data folder `data`.
    pub fn dir(&self, data: &Path) -> PathBuf {
        folder(data, &self.version)
    }

    /// The installed executable.
    pub fn installed(&self, data: &Path) -> PathBuf {
        self.dir(data).join(self.exe)
    }

    /// The size in whole megabytes, as the settings window shows it.
    pub fn megabytes(&self) -> u64 {
        megabytes(self.size)
    }
}

/// `bytes` in whole megabytes, rounded up, as the settings window shows a
/// build's size and its download's progress: a size is never understated,
/// and the progress ends on the size the window named before the download.
pub fn megabytes(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

/// This computer's architecture: an x86-64 bridge emulated on an ARM64
/// Windows installs the ARM64 build, which runs natively.
pub fn machine_arch() -> Arch {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_ARM64;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
        let (mut process, mut native) = (0u16, 0u16);
        // SAFETY: the current process's pseudo handle and two valid out pointers.
        let asked = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
        if asked != 0 && native == IMAGE_FILE_MACHINE_ARM64 {
            return Arch::Arm64;
        }
        Arch::X86_64
    }
    #[cfg(not(windows))]
    {
        if cfg!(target_arch = "aarch64") { Arch::Arm64 } else { Arch::X86_64 }
    }
}

/// Where an installation stands, for the settings window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Bytes downloaded of the asset's size.
    Downloading {
        done: u64,
        total: u64,
    },
    Checking,
    Unpacking,
}

/// How the files arrive: the system's own tools on Windows, stand-ins in tests.
pub trait Transport: Sync {
    /// Downloads `url` to `to`, refusing more than `max_size` bytes.
    fn download(&self, url: &str, to: &Path, max_size: u64) -> Result<(), String>;
    /// Unpacks `members` of the zip archive `zip` under `to`, keeping their paths.
    fn extract(&self, zip: &Path, members: &[String], to: &Path) -> Result<(), String>;
    /// The answer of GitHub's API at `url`, refusing more than `max_size` bytes.
    fn fetch(&self, url: &str, max_size: u64) -> Result<Vec<u8>, String>;
}

/// The folder the build of `version` installs into, in the data folder
/// `data`: `engines\stockfish-<version>`, as [`crate::engines::find`] lists it.
fn folder(data: &Path, version: &str) -> PathBuf {
    data.join("engines").join(format!("stockfish-{version}"))
}

/// The licence of the build of `version` the bridge installed in the data
/// folder `data`. `None` when `version` is not digits and dots, so that a
/// version the settings window sends names no other file.
pub fn licence(data: &Path, version: &str) -> Option<PathBuf> {
    is_version(version).then(|| folder(data, version).join(LICENCE))
}

/// Whether `text` is a release's version: numbers joined by dots, such as
/// `19` or `17.1`.
fn is_version(text: &str) -> bool {
    text.len() <= 16 && text.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether the release `version` is newer than `than`, compared number by
/// number: 19 < 19.1 < 20, and 19.0 is 19. A text that is not a version is
/// newer than nothing, and nothing is newer than it.
pub fn is_newer(version: &str, than: &str) -> bool {
    let numbers = |text: &str| -> Option<Vec<u64>> {
        if !is_version(text) {
            return None;
        }
        let mut numbers: Vec<u64> = text.split('.').map(|n| n.parse().ok()).collect::<Option<_>>()?;
        while numbers.len() > 1 && numbers.last() == Some(&0) {
            numbers.pop();
        }
        Some(numbers)
    };
    numbers(version).zip(numbers(than)).is_some_and(|(version, than)| version > than)
}

/// The version of the build the bridge installed whose executable is `exe`:
/// a file in `engines\stockfish-<version>` in the data folder `data`. `None`
/// for any other engine.
pub fn installed_version(data: &Path, exe: &Path) -> Option<String> {
    let folder = exe.parent()?;
    if folder.parent()? != data.join("engines") {
        return None;
    }
    let version = folder.file_name()?.to_str()?.strip_prefix("stockfish-")?;
    is_version(version).then(|| version.to_string())
}

/// The prefix of a folder being removed, beside the builds.
const REMOVING: &str = ".remove-";

/// Removes the builds the bridge installed in the data folder `data` whose
/// version is older than `kept` (#322), and what an earlier removal left.
/// Each build's folder is renamed aside first: Windows refuses that while its
/// engine runs, and the build then stays whole until a later call. Answers
/// the versions removed and the reasons of those that stayed, which name no
/// path.
pub fn remove_older(data: &Path, kept: &str) -> (Vec<String>, Vec<String>) {
    let engines = data.join("engines");
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    let Ok(read) = std::fs::read_dir(&engines) else { return (removed, failed) };
    // Read whole before anything is renamed into the folder.
    let folders: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    for name in folders {
        if name.starts_with(REMOVING) {
            if let Err(e) = std::fs::remove_dir_all(engines.join(&name)) {
                failed.push(format!("{name}: {e}"));
            }
            continue;
        }
        let Some(version) = name.strip_prefix("stockfish-") else { continue };
        if !is_newer(kept, version) {
            continue;
        }
        let aside = engines.join(format!("{REMOVING}{name}"));
        let _ = std::fs::remove_dir_all(&aside);
        match std::fs::rename(engines.join(&name), &aside) {
            Ok(()) => {
                // What stays is removed by a later call.
                let _ = std::fs::remove_dir_all(&aside);
                removed.push(version.to_string());
            }
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    (removed, failed)
}

/// Whether `build` is installed in the data folder `data`: its executable and
/// its licence are there.
pub fn is_installed(data: &Path, build: &Build) -> bool {
    build.installed(data).is_file() && build.dir(data).join(LICENCE).is_file()
}

/// Installs `build` into the data folder `data` and returns its executable.
/// An installed build is kept as it is, with nothing downloaded. A download
/// that does not match the pinned size and digest is deleted and refused. The
/// new installation replaces a partial one only once it is complete, and a
/// failed step never removes what was there before.
pub fn install(
    data: &Path,
    build: &Build,
    transport: &dyn Transport,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<PathBuf, String> {
    if is_installed(data, build) {
        return Ok(build.installed(data));
    }
    let engines = data.join("engines");
    std::fs::create_dir_all(&engines).map_err(|e| format!("{}: {e}", engines.display()))?;
    let zip = engines.join(format!(".download-{}", build.asset));
    let staging = engines.join(format!(".unpack-stockfish-{}", build.version));
    let clean = || {
        let _ = std::fs::remove_file(&zip);
        let _ = std::fs::remove_dir_all(&staging);
    };
    clean();
    let result = (|| {
        download_with_progress(transport, build, &zip, progress)?;
        progress(Progress::Checking);
        // The length first: a wrong one is refused before any hashing.
        let size = std::fs::metadata(&zip).map_err(|e| e.to_string())?.len();
        if size != build.size {
            return Err(format!("The download does not match the pinned build: {size} bytes, expected {}", build.size));
        }
        let digest = sha256::hex(&sha256::file(&zip).map_err(|e| e.to_string())?);
        if digest != *build.sha256 {
            return Err(format!(
                "The download does not match the pinned build: SHA-256 {digest}, expected {}",
                build.sha256
            ));
        }
        progress(Progress::Unpacking);
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        let members = [format!("stockfish/{}", build.exe), format!("stockfish/{LICENCE}")];
        transport.extract(&zip, &members, &staging)?;
        // Both files are there before anything is installed.
        for name in [build.exe, LICENCE] {
            if !staging.join("stockfish").join(name).is_file() {
                return Err(format!("The archive has no stockfish/{name}"));
            }
        }
        commit(&staging.join("stockfish"), &build.dir(data))?;
        Ok(build.installed(data))
    })();
    clean();
    result
}

/// Puts the unpacked folder `from` in place of `dir`: by renaming, so that a
/// reader sees the old folder or the complete new one. A folder already at
/// `dir`, a partial earlier installation, is moved aside first and restored
/// if the new one cannot take its place; it is removed only after that.
fn commit(from: &Path, dir: &Path) -> Result<(), String> {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("stockfish");
    let aside = dir.with_file_name(format!(".previous-{name}"));
    let _ = std::fs::remove_dir_all(&aside);
    let had = dir.exists();
    if had {
        std::fs::rename(dir, &aside).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(from, dir) {
        if had {
            let _ = std::fs::rename(&aside, dir);
        }
        return Err(format!("{}: {e}", dir.display()));
    }
    if had {
        let _ = std::fs::remove_dir_all(&aside);
    }
    Ok(())
}

/// Downloads while reporting the file's growing size every quarter second.
fn download_with_progress(
    transport: &dyn Transport,
    build: &Build,
    zip: &Path,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<(), String> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let url = build.url();
        let downloading = scope.spawn(move || {
            let result = transport.download(&url, zip, build.size);
            let _ = done_tx.send(());
            result
        });
        progress(Progress::Downloading { done: 0, total: build.size });
        while done_rx.recv_timeout(Duration::from_millis(250)).is_err() {
            let done = std::fs::metadata(zip).map(|m| m.len()).unwrap_or(0);
            progress(Progress::Downloading { done: done.min(build.size), total: build.size });
        }
        downloading.join().unwrap_or_else(|_| Err("the download stopped".into()))
    })
}

/// Whether to offer `build` instead of the chosen engine `current`: it is an
/// older Stockfish, installed by the bridge or named so by its engine or file,
/// and its major version is below the build's.
pub fn offer(build: &Build, current: Option<(&Path, &str)>) -> bool {
    let Some((path, name)) = current else { return false };
    major(build).zip(major_of(path, name)).is_some_and(|(newest, major)| major < newest)
}

/// Whether the engine at `path`, named `name`, is a Stockfish of `build`'s
/// major version or a newer one, its version read as [`offer`] reads it. An
/// engine whose version cannot be read is not.
pub fn is_current(build: &Build, path: &Path, name: &str) -> bool {
    major(build).zip(major_of(path, name)).is_some_and(|(newest, major)| major >= newest)
}

fn major(build: &Build) -> Option<u32> {
    build.version.split('.').next()?.parse().ok()
}

/// The Stockfish major version that an engine's name or its folder names.
fn major_of(path: &Path, name: &str) -> Option<u32> {
    // The folder's name whichever separator the path uses.
    let text = path.to_string_lossy();
    let folder = text.rsplit(['\\', '/']).nth(1).unwrap_or_default();
    [name, folder].iter().find_map(|text| stockfish_major(text))
}

/// The major version in a name such as `Stockfish 17.1`, `stockfish-16` or
/// `Stockfish_15_x64`.
fn stockfish_major(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let rest = lower.strip_prefix("stockfish")?.trim_start_matches([' ', '-', '_']);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The system's own tools: `curl.exe` and `tar.exe`, part of Windows 10 and 11,
/// called by their full paths so that no other program on the path answers.
pub struct System;

/// The tools run outside the Store package, so they are given the paths at
/// which they find the bridge's files ([`folders::outside`], #289).
impl Transport for System {
    fn download(&self, url: &str, to: &Path, max_filesize: u64) -> Result<(), String> {
        let mut curl = system_tool("curl.exe")?;
        curl.args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https"])
            // Bounded: no more than the pinned size, and a transfer that
            // stalls below 1 kB/s for a minute, or runs over half an hour, ends.
            .args(["--max-filesize", &max_filesize.to_string()])
            .args(["--connect-timeout", "30", "--speed-limit", "1024", "--speed-time", "60", "--max-time", "1800"])
            .arg("--output")
            .arg(folders::outside(to))
            .arg(url);
        run(curl, "curl.exe", "The download failed").map(drop)
    }

    fn extract(&self, zip: &Path, members: &[String], to: &Path) -> Result<(), String> {
        let mut tar = system_tool("tar.exe")?;
        tar.arg("-xf").arg(folders::outside(zip)).arg("-C").arg(folders::outside(to)).args(members);
        run(tar, "tar.exe", "Unpacking failed").map(drop)
    }

    fn fetch(&self, url: &str, max_size: u64) -> Result<Vec<u8>, String> {
        let mut curl = system_tool("curl.exe")?;
        curl.args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https"])
            .args(["--header", "Accept: application/vnd.github+json"])
            .args(["--max-filesize", &max_size.to_string()])
            .args(["--connect-timeout", "15", "--max-time", "60"])
            .arg(url);
        let answer = run(curl, "curl.exe", "The lookup failed")?;
        // A server that names no length is cut off by the length read.
        if answer.len() as u64 > max_size {
            return Err(format!("The lookup failed: an answer of more than {max_size} bytes"));
        }
        Ok(answer)
    }
}

/// Runs the tool `name` as `command` and answers what it wrote. A failure is
/// `failed` with the tool's exit status and the reason it gave.
fn run(mut command: std::process::Command, name: &str, failed: &str) -> Result<Vec<u8>, String> {
    use std::process::Stdio;
    let out = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    if out.status.success() { Ok(out.stdout) } else { Err(failure(failed, &out.status.to_string(), &out.stderr)) }
}

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
fn system_tool(name: &str) -> Result<std::process::Command, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?;
    let mut command = std::process::Command::new(PathBuf::from(root).join("System32").join(name));
    command.creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(not(windows))]
fn system_tool(name: &str) -> Result<std::process::Command, String> {
    Err(format!("Installing Stockfish needs Windows ({name})"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Serves `archive` as the download and unpacks `files` as the archive's content.
    struct Fake {
        archive: Vec<u8>,
        files: Vec<(String, Vec<u8>)>,
        asked: Mutex<Vec<String>>,
    }

    impl Transport for Fake {
        fn download(&self, url: &str, to: &Path, max_size: u64) -> Result<(), String> {
            self.asked.lock().unwrap().push(url.to_string());
            assert_eq!(max_size, TEST_BUILD.size);
            std::fs::write(to, &self.archive).map_err(|e| e.to_string())
        }

        fn fetch(&self, url: &str, _max_size: u64) -> Result<Vec<u8>, String> {
            panic!("an installation looks nothing up: {url}")
        }

        fn extract(&self, _zip: &Path, members: &[String], to: &Path) -> Result<(), String> {
            for (name, bytes) in &self.files {
                if members.contains(name) {
                    let path = to.join(name);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, bytes).unwrap();
                }
            }
            Ok(())
        }
    }

    const TEST_BUILD: Build = Build {
        version: Cow::Borrowed("19"),
        arch: Arch::X86_64,
        asset: "test.zip",
        size: 11,
        // SHA-256 of "hello world".
        sha256: Cow::Borrowed("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"),
        exe: "stockfish-test.exe",
    };

    fn fake(archive: &[u8]) -> Fake {
        Fake {
            archive: archive.to_vec(),
            files: vec![
                ("stockfish/stockfish-test.exe".into(), b"MZ".to_vec()),
                (format!("stockfish/{LICENCE}"), b"GPL".to_vec()),
                ("stockfish/src/main.cpp".into(), b"int main".to_vec()),
            ],
            asked: Mutex::new(Vec::new()),
        }
    }

    fn data(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-stockfish-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn installs_a_matching_download_and_keeps_only_the_engine_and_its_licence() {
        let dir = data("ok");
        let fake = fake(b"hello world");
        let mut seen = Vec::new();
        let exe = install(&dir, &TEST_BUILD, &fake, &mut |p| seen.push(p)).unwrap();
        assert_eq!(exe, dir.join("engines/stockfish-19/stockfish-test.exe"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ");
        assert_eq!(std::fs::read(dir.join("engines/stockfish-19").join(LICENCE)).unwrap(), b"GPL");
        let mut left: Vec<String> = std::fs::read_dir(dir.join("engines"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["stockfish-19"], "no download or staging is left");
        assert_eq!(
            fake.asked.lock().unwrap()[..],
            ["https://github.com/official-stockfish/Stockfish/releases/download/sf_19/test.zip"]
        );
        assert_eq!(seen.first(), Some(&Progress::Downloading { done: 0, total: 11 }));
        assert!(seen.ends_with(&[Progress::Checking, Progress::Unpacking]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A version finds the licence installed beside its build, and text that
    /// is not a version finds nothing.
    #[test]
    fn finds_a_licence_by_its_version_only() {
        let dir = data("licence");
        install(&dir, &TEST_BUILD, &fake(b"hello world"), &mut |_| {}).unwrap();
        let path = licence(&dir, &TEST_BUILD.version).unwrap();
        assert_eq!(path, TEST_BUILD.dir(&dir).join(LICENCE));
        assert_eq!(std::fs::read(&path).unwrap(), b"GPL");
        assert_eq!(licence(&dir, "17.1"), Some(dir.join("engines").join("stockfish-17.1").join(LICENCE)));
        for bad in ["", "19/../..", "../19", r"19\..", "/19", "19 ", "dev", "19.", ".19", "1..9"] {
            assert_eq!(licence(&dir, bad), None, "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_download_that_does_not_match_and_leaves_nothing() {
        let dir = data("tampered");
        for archive in [&b"hello World"[..], b"hello world!"] {
            let error = install(&dir, &TEST_BUILD, &fake(archive), &mut |_| {}).unwrap_err();
            assert!(error.contains("does not match"), "{error}");
            assert!(!TEST_BUILD.dir(&dir).exists());
            assert_eq!(std::fs::read_dir(dir.join("engines")).unwrap().count(), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_an_installed_build_and_never_loses_one_to_a_failed_reinstall() {
        let dir = data("again");
        let good = fake(b"hello world");
        let exe = install(&dir, &TEST_BUILD, &good, &mut |_| {}).unwrap();
        // Installed: nothing is downloaded again.
        assert_eq!(install(&dir, &TEST_BUILD, &good, &mut |_| {}).unwrap(), exe);
        assert_eq!(good.asked.lock().unwrap().len(), 1);

        // A partial installation (the licence gone) is replaced whole, and a
        // failed replacement leaves it as it was.
        let folder = TEST_BUILD.dir(&dir);
        std::fs::remove_file(folder.join(LICENCE)).unwrap();
        std::fs::write(folder.join("marker"), b"mine").unwrap();
        let error = install(&dir, &TEST_BUILD, &fake(b"hello World"), &mut |_| {}).unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");
        assert_eq!(std::fs::read(folder.join("marker")).unwrap(), b"mine", "the earlier folder is untouched");
        assert!(folder.join("stockfish-test.exe").is_file());
        install(&dir, &TEST_BUILD, &good, &mut |_| {}).unwrap();
        assert!(is_installed(&dir, &TEST_BUILD));
        assert!(!folder.join("marker").exists(), "the complete installation replaced the partial one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_wrong_length_before_hashing() {
        let dir = data("length");
        let error = install(&dir, &TEST_BUILD, &fake(b"hello world, longer"), &mut |_| {}).unwrap_err();
        assert!(error.contains("19 bytes, expected 11"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_an_archive_without_the_engine() {
        let dir = data("empty");
        let mut empty = fake(b"hello world");
        empty.files.clear();
        let error = install(&dir, &TEST_BUILD, &empty, &mut |_| {}).unwrap_err();
        assert!(error.contains("no stockfish/stockfish-test.exe"), "{error}");
        assert!(!TEST_BUILD.dir(&dir).exists(), "no empty engine folder is left");
        // The engine without its licence is not installed either.
        let mut half = fake(b"hello world");
        half.files.retain(|(name, _)| !name.ends_with(LICENCE));
        let error = install(&dir, &TEST_BUILD, &half, &mut |_| {}).unwrap_err();
        assert!(error.contains(LICENCE), "{error}");
        assert!(!TEST_BUILD.dir(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pins_one_https_build_per_architecture() {
        for arch in [Arch::X86_64, Arch::Arm64] {
            let build = Build::for_arch(arch);
            assert!(
                build.url().starts_with("https://github.com/official-stockfish/Stockfish/releases/download/sf_19/")
            );
            assert_eq!(build.sha256.len(), 64);
            assert!(build.exe.ends_with(".exe"));
        }
        assert_eq!(Build::for_arch(Arch::X86_64).megabytes(), 78);
        // 80,190,536 bytes are 76.48 MB: rounded up to 77, never down to 76.
        assert_eq!(Build::for_arch(Arch::Arm64).megabytes(), 77);
        assert_eq!([0, 1, 1 << 20, (1 << 20) + 1].map(megabytes), [0, 1, 1, 2]);
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
    fn offers_a_build_only_for_an_older_stockfish() {
        let pinned = Build::for_arch(Arch::X86_64);
        let older = [
            (r"C:\Program Files\ChessBase\Engines.x64\Stockfish 17.1\sf.exe", "Stockfish 17.1"),
            (r"C:\x\stockfish-16\stockfish.exe", "stockfish.exe"),
            (r"C:\x\y\engine.exe", "Stockfish_15_x64"),
        ];
        for (path, name) in older {
            assert!(offer(pinned, Some((Path::new(path), name))), "{name}");
        }
        for (path, name) in [
            (r"C:\x\stockfish-19\sf.exe", "Stockfish 19"),
            (r"C:\x\lc0\lc0.exe", "Lc0 v0.31"),
            (r"C:\x\sf.exe", "Stockfish dev"),
        ] {
            assert!(!offer(pinned, Some((Path::new(path), name))), "{name}");
        }
        assert!(!offer(pinned, None));
        // A newer release GitHub named is offered in place of the pinned version.
        let newer = Build::released(Arch::X86_64, "20", 1, &"a".repeat(64)).unwrap();
        assert!(offer(&newer, Some((Path::new(r"C:\x\stockfish-19\sf.exe"), "Stockfish 19"))));
    }

    /// Only a Stockfish whose name or folder names the pinned major version or
    /// a newer one is current; an older one, another engine and a Stockfish of
    /// unknown version are not.
    #[test]
    fn a_stockfish_of_the_pinned_version_or_newer_is_current() {
        let pinned = Build::for_arch(Arch::X86_64);
        for (path, name) in [
            (r"C:\x\stockfish-19\stockfish.exe", "Stockfish 19"),
            (r"C:\CB\Engines.x64\Stockfish 19.1\sf.exe", "sf"),
            (r"C:\x\y\engine.exe", "Stockfish_20_x64"),
        ] {
            assert!(is_current(pinned, Path::new(path), name), "{name}");
        }
        for (path, name) in [
            (r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe", "Stockfish 17.1"),
            (r"C:\x\lc0\lc0.exe", "Lc0 v0.31"),
            (r"C:\x\sf.exe", "Stockfish dev"),
            (r"C:\CB\Engines\stockfish.exe", "stockfish"),
        ] {
            assert!(!is_current(pinned, Path::new(path), name), "{name}");
        }
        let newer = Build::released(Arch::X86_64, "20", 1, &"a".repeat(64)).unwrap();
        assert!(!is_current(&newer, Path::new(r"C:\x\stockfish-19\stockfish.exe"), "Stockfish 19"));
    }

    /// A release GitHub named takes the asset and executable names of the
    /// pinned build for its architecture, and only a version, a size and a
    /// digest that fit (#322).
    #[test]
    fn a_released_build_takes_only_a_version_size_and_digest_that_fit() {
        let digest = "3C8BF1F9EA66A09350A40DF4F632288285AC206D99F33AB5842C408FC30B48A7";
        let build = Build::released(Arch::X86_64, "19", 81_431_614, digest).unwrap();
        assert_eq!(&build, Build::for_arch(Arch::X86_64), "Stockfish 19 as GitHub names it is the pinned build");
        let arm = Build::released(Arch::Arm64, "20.1", 5, &"0".repeat(64)).unwrap();
        assert_eq!((arm.asset, arm.exe), (Build::for_arch(Arch::Arm64).asset, Build::for_arch(Arch::Arm64).exe));
        assert_eq!(
            arm.url(),
            "https://github.com/official-stockfish/Stockfish/releases/download/sf_20.1/stockfish-windows-arm64-universal.zip"
        );
        for version in ["", "dev-20260930", "20/..", "20 ", "20.", "1".repeat(17).as_str()] {
            assert!(Build::released(Arch::X86_64, version, 5, digest).is_err(), "{version:?}");
        }
        for size in [0, MAX_SIZE + 1] {
            assert!(Build::released(Arch::X86_64, "20", size, digest).is_err(), "{size}");
        }
        for sha256 in ["", &digest[1..], &format!("{}g", &digest[1..])] {
            assert!(Build::released(Arch::X86_64, "20", 5, sha256).is_err(), "{sha256:?}");
        }
    }

    #[test]
    fn versions_compare_number_by_number() {
        for (newer, older) in [("20", "19"), ("19.1", "19"), ("19.10", "19.9"), ("100", "99"), ("20", "19.9")] {
            assert!(is_newer(newer, older), "{newer} > {older}");
            assert!(!is_newer(older, newer), "{older} < {newer}");
        }
        for (a, b) in [("19", "19"), ("19.0", "19"), ("19", "19.0.0")] {
            assert!(!is_newer(a, b) && !is_newer(b, a), "{a} = {b}");
        }
        for (a, b) in [("dev", "19"), ("20", "dev"), ("", "19"), ("20", "")] {
            assert!(!is_newer(a, b), "{a:?} against {b:?}");
        }
    }

    /// Only a file in a build folder the bridge installed, in this data
    /// folder, carries a version.
    #[test]
    fn only_a_build_the_bridge_installed_has_an_installed_version() {
        let dir = Path::new("data");
        let engines = dir.join("engines");
        assert_eq!(installed_version(dir, &engines.join("stockfish-19").join("sf.exe")).as_deref(), Some("19"));
        assert_eq!(installed_version(dir, &engines.join("stockfish-17.1").join("a.exe")).as_deref(), Some("17.1"));
        for other in [
            engines.join("stockfish-dev").join("sf.exe"),
            engines.join("lc0").join("lc0.exe"),
            engines.join("sf.exe"),
            engines.join("stockfish-19").join("sub").join("sf.exe"),
            Path::new("elsewhere").join("engines").join("stockfish-19").join("sf.exe"),
            PathBuf::from("sf.exe"),
        ] {
            assert_eq!(installed_version(dir, &other), None, "{}", other.display());
        }
    }

    /// The builds older than the kept one go, with what an earlier removal
    /// left; the kept build, a newer one and other folders stay.
    #[test]
    fn removes_only_older_builds_and_leftovers() {
        let dir = data("older");
        let engines = dir.join("engines");
        for name in ["stockfish-17.1", "stockfish-18", "stockfish-19", "stockfish-20", "lc0", ".remove-stockfish-16"] {
            std::fs::create_dir_all(engines.join(name)).unwrap();
            std::fs::write(engines.join(name).join(LICENCE), b"GPL").unwrap();
        }
        std::fs::write(engines.join(".download-stockfish.zip"), b"partial").unwrap();
        let (mut removed, failed) = remove_older(&dir, "19");
        removed.sort();
        assert_eq!(removed, ["17.1", "18"]);
        assert!(failed.is_empty(), "{failed:?}");
        let mut left: Vec<String> =
            std::fs::read_dir(&engines).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, [".download-stockfish.zip", "lc0", "stockfish-19", "stockfish-20"]);
        assert_eq!(remove_older(&dir, "19"), (vec![], vec![]), "nothing more to remove");
        assert_eq!(remove_older(&dir.join("none"), "19"), (vec![], vec![]), "no engines folder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
