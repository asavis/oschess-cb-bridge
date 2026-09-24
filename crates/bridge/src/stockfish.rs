//! Installing the official Stockfish build on the user's request (#13, #54).
//!
//! The bridge carries no engine. When the user asks for one, it downloads the
//! build pinned in this release from Stockfish's GitHub releases, over HTTPS
//! only, checks its size and SHA-256 against the pinned values, and unpacks the
//! executable and its licence into `engines\stockfish-<version>` in the data
//! folder. Nothing is downloaded or checked in the background; a later bridge
//! release that pins a newer build offers it, and the user decides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sha256;

/// A processor architecture Stockfish publishes a Windows build for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Arm64,
}

/// A pinned official build.
#[derive(Debug, PartialEq, Eq)]
pub struct Build {
    /// The release, as its tag names it after `sf_`.
    pub version: &'static str,
    pub arch: Arch,
    /// The release asset.
    pub asset: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// The executable inside the archive, under `stockfish/`.
    pub exe: &'static str,
}

/// The builds this release installs: Stockfish 19, published 2026-09-05. Its
/// universal builds choose the processor's instructions themselves, and the
/// NNUE network is inside the executable.
pub const PINNED: [Build; 2] = [
    Build {
        version: "19",
        arch: Arch::X86_64,
        asset: "stockfish-windows-x86-64-universal.zip",
        size: 81_431_614,
        sha256: "3c8bf1f9ea66a09350a40df4f632288285ac206d99f33ab5842c408fc30b48a7",
        exe: "stockfish-windows-x86-64-universal.exe",
    },
    Build {
        version: "19",
        arch: Arch::Arm64,
        asset: "stockfish-windows-arm64-universal.zip",
        size: 80_190_536,
        sha256: "8372ad3f0d7276deb2c70f801f541ec7db463219fc6d9c7592864e542aa4f401",
        exe: "stockfish-windows-arm64-universal.exe",
    },
];

/// The licence file kept beside the executable.
pub const LICENCE: &str = "Copying.txt";

impl Build {
    /// The pinned build for `arch`.
    pub fn for_arch(arch: Arch) -> &'static Build {
        PINNED.iter().find(|b| b.arch == arch).expect("a build for every architecture")
    }

    /// The asset's fixed address.
    pub fn url(&self) -> String {
        format!("https://github.com/official-stockfish/Stockfish/releases/download/sf_{}/{}", self.version, self.asset)
    }

    /// The folder this build installs into, in the data folder `data`.
    pub fn dir(&self, data: &Path) -> PathBuf {
        data.join("engines").join(format!("stockfish-{}", self.version))
    }

    /// The installed executable.
    pub fn installed(&self, data: &Path) -> PathBuf {
        self.dir(data).join(self.exe)
    }

    /// The size in whole megabytes, as the settings window shows it.
    pub fn megabytes(&self) -> u64 {
        self.size.div_ceil(1 << 20)
    }
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
    /// Downloads `url` to `to`.
    fn download(&self, url: &str, to: &Path) -> Result<(), String>;
    /// Unpacks `members` of the zip archive `zip` under `to`, keeping their paths.
    fn extract(&self, zip: &Path, members: &[String], to: &Path) -> Result<(), String>;
}

/// Installs `build` into the data folder `data` and returns its executable.
/// A download that does not match the pinned size and digest is deleted and
/// refused; a failed step leaves no partial installation behind.
pub fn install(
    data: &Path,
    build: &Build,
    transport: &dyn Transport,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<PathBuf, String> {
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
        let size = std::fs::metadata(&zip).map_err(|e| e.to_string())?.len();
        let digest = sha256::hex(&sha256::file(&zip).map_err(|e| e.to_string())?);
        if size != build.size || digest != build.sha256 {
            return Err(format!(
                "The download does not match the pinned build: {size} bytes, SHA-256 {digest}; expected {} bytes, {}",
                build.size, build.sha256
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
        let dir = build.dir(data);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for name in [build.exe, LICENCE] {
            let moved = std::fs::rename(staging.join("stockfish").join(name), dir.join(name));
            if let Err(e) = moved {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!("{}: {e}", dir.join(name).display()));
            }
        }
        Ok(build.installed(data))
    })();
    clean();
    result
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
            let result = transport.download(&url, zip);
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

/// The pinned build to offer instead of the chosen engine `current`: when it is
/// an older Stockfish, installed by the bridge or named so by its engine or
/// file, and its major version is below the pinned one. `None` otherwise.
pub fn offer(current: Option<(&Path, &str)>) -> Option<&'static Build> {
    let build = Build::for_arch(machine_arch());
    let pinned: u32 = build.version.split('.').next()?.parse().ok()?;
    let (path, name) = current?;
    // The folder's name whichever separator the path uses.
    let text = path.to_string_lossy();
    let folder = text.rsplit(['\\', '/']).nth(1).unwrap_or_default();
    let major = [name, folder].iter().find_map(|text| stockfish_major(text))?;
    (major < pinned).then_some(build)
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

impl Transport for System {
    fn download(&self, url: &str, to: &Path) -> Result<(), String> {
        let status = system_tool("curl.exe")?
            .args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https"])
            .arg("--output")
            .arg(to)
            .arg(url)
            .status()
            .map_err(|e| format!("curl.exe: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("The download failed ({status})")) }
    }

    fn extract(&self, zip: &Path, members: &[String], to: &Path) -> Result<(), String> {
        let status = system_tool("tar.exe")?
            .arg("-xf")
            .arg(zip)
            .arg("-C")
            .arg(to)
            .args(members)
            .status()
            .map_err(|e| format!("tar.exe: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("Unpacking failed ({status})")) }
    }
}

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
        fn download(&self, url: &str, to: &Path) -> Result<(), String> {
            self.asked.lock().unwrap().push(url.to_string());
            std::fs::write(to, &self.archive).map_err(|e| e.to_string())
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
        version: "19",
        arch: Arch::X86_64,
        asset: "test.zip",
        size: 11,
        // SHA-256 of "hello world".
        sha256: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
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
    }

    #[test]
    fn offers_the_pinned_build_only_for_an_older_stockfish() {
        let older = [
            (r"C:\Program Files\ChessBase\Engines.x64\Stockfish 17.1\sf.exe", "Stockfish 17.1"),
            (r"C:\x\stockfish-16\stockfish.exe", "stockfish.exe"),
            (r"C:\x\y\engine.exe", "Stockfish_15_x64"),
        ];
        for (path, name) in older {
            assert!(offer(Some((Path::new(path), name))).is_some(), "{name}");
        }
        for (path, name) in [
            (r"C:\x\stockfish-19\sf.exe", "Stockfish 19"),
            (r"C:\x\lc0\lc0.exe", "Lc0 v0.31"),
            (r"C:\x\sf.exe", "Stockfish dev"),
        ] {
            assert!(offer(Some((Path::new(path), name))).is_none(), "{name}");
        }
        assert!(offer(None).is_none());
    }
}
