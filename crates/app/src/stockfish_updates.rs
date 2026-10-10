//! Keeping the Stockfish the bridge installed up to date (#322).
//!
//! While «Update Stockfish automatically» is on, the app asks GitHub's API for
//! Stockfish's newest release ([`stockfish::LATEST`]) a minute after it
//! starts, every six hours after that, and when the user turns the option on.
//! A release counts only once it is a week old ([`SETTLE`]): a build put in
//! place of the real one, should Stockfish's account on GitHub ever be taken
//! over, is then likely to be found and removed before any bridge installs it.
//! When the chosen engine is a build the bridge installed and the release is
//! newer, it installs the release's build, checked against the size and
//! SHA-256 GitHub gives for it, chooses it once the bridge is idle, and
//! removes the older builds it installed. No other engine is replaced: an
//! older Stockfish from elsewhere keeps the engine section's offer. Each
//! lookup also tells the engine section and «Install Stockfish» the newest
//! release ([`Known`]).
//!
//! The desktop runs a look on a thread of its own (`desktop::engine_updates`);
//! what a look does is here, tested on every system.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use bridge::config;
use bridge::stockfish::{self, Arch, Build, Transport};
use bridge::sync::lock;
use serde::Deserialize;

use crate::choices::Choices;
use crate::prefs;

/// How old a release must be before the bridge installs it: a week.
pub const SETTLE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The newest build this run knows of: the pinned one, until a lookup names a
/// newer release.
pub struct Known(Mutex<Option<Build>>);

impl Default for Known {
    fn default() -> Self {
        Self::new()
    }
}

impl Known {
    pub const fn new() -> Self {
        Known(Mutex::new(None))
    }

    /// The newest build for `arch`: the release the last lookup named, when
    /// it is newer than the pinned build.
    pub fn newest(&self, arch: Arch) -> Build {
        let pinned = Build::for_arch(arch);
        match &*lock(&self.0) {
            Some(known) if known.arch == arch && stockfish::is_newer(&known.version, &pinned.version) => known.clone(),
            _ => pinned.clone(),
        }
    }

    /// Asks GitHub's API for Stockfish's newest release, keeps its build for
    /// `arch` when it is old enough at `now` ([`release`]), and answers the
    /// newest build ([`Known::newest`]).
    pub fn look_up(&self, transport: &dyn Transport, arch: Arch, now: SystemTime) -> Result<Build, String> {
        let answer = transport.fetch(stockfish::LATEST, stockfish::MAX_ANSWER)?;
        *lock(&self.0) = Some(release(&answer, arch, now)?);
        Ok(self.newest(arch))
    }
}

/// The part of GitHub's answer about a release that is read.
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    /// When it was published, as `2026-09-05T08:33:17Z`.
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    /// `sha256:` and the hexadecimal digest, which GitHub computes on upload.
    digest: Option<String>,
}

/// The build for `arch` that GitHub's `answer` about a release names: the
/// release is tagged `sf_<version>`, was published at least [`SETTLE`] before
/// `now`, and its asset named as the pinned one is carries a size and a
/// SHA-256 ([`Build::released`]). The download address is the bridge's own
/// ([`Build::url`]), never one from the answer.
pub fn release(answer: &[u8], arch: Arch, now: SystemTime) -> Result<Build, String> {
    let release: Release = serde_json::from_slice(answer).map_err(|e| format!("GitHub's answer: {e}"))?;
    let tag: String = release.tag_name.chars().take(40).collect();
    let version = tag.strip_prefix("sf_").filter(|_| !release.draft && !release.prerelease);
    let version = version.ok_or_else(|| format!("{tag:?} is not a Stockfish release"))?;
    let published = release.published_at.as_deref().and_then(unix_seconds);
    let published = published.ok_or_else(|| format!("Stockfish {version} names no publication time"))?;
    let now = now.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now < published.saturating_add(SETTLE.as_secs()) {
        return Err(format!("Stockfish {version} is less than {} days old", SETTLE.as_secs() / 86_400));
    }
    let name = Build::for_arch(arch).asset;
    let asset = release.assets.iter().find(|a| a.name == name);
    let asset = asset.ok_or_else(|| format!("Stockfish {version} has no {name}"))?;
    let sha256 = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:"));
    let sha256 = sha256.ok_or_else(|| format!("Stockfish {version} names no SHA-256 for {name}"))?;
    Build::released(arch, version, asset.size, sha256)
}

/// The seconds since 1970 of a UTC time written as GitHub writes it,
/// `2026-09-05T08:33:17Z`; `None` for any other text.
fn unix_seconds(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    let marks = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if b.len() != 20 || marks.iter().any(|&(at, mark)| b[at] != mark) {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        let digits = text.get(from..to)?;
        digits.bytes().all(|d| d.is_ascii_digit()).then(|| digits.parse().ok())?
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    // Days from 1970-01-01 to the date, counting years from March so that a
    // leap day ends its year (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let (era, of_era) = (y.div_euclid(400), y.rem_euclid(400));
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_era = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

/// What a look did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// «Update Stockfish automatically» is off: nothing was looked up.
    Off,
    /// The chosen engine is none the bridge installed: nothing is replaced.
    NotOurs,
    /// The chosen build is the newest release's.
    Current,
    /// The newest release's build was installed and chosen; its version.
    Updated(String),
    /// The newest release's build was installed, but the user chose another
    /// engine meanwhile; it stays listed.
    Kept(String),
    /// The newest release's build was installed, but the user turned the
    /// option off while it waited; nothing was chosen, and it stays listed.
    Withdrawn(String),
}

/// What a look works with: the data folder, its `bridge.toml`, the app's
/// engine choices and the newest build known, how files arrive, and the
/// time a release's age is measured at.
pub struct Look<'a> {
    pub data: &'a Path,
    pub config_path: &'a Path,
    pub choices: &'a Choices,
    pub known: &'a Known,
    pub transport: &'a dyn Transport,
    pub arch: Arch,
    pub now: SystemTime,
}

impl Look<'_> {
    /// While «Update Stockfish automatically» is on, looks up the newest
    /// release and, when the chosen engine is an older build the bridge
    /// installed, installs the release's build, waits with `wait` until the
    /// bridge is idle, and chooses the build once `probe` accepts it, unless
    /// the user chose another engine meanwhile ([`Choices::update`]). `wait`
    /// answers false when it gave up, as when the option was turned off; the
    /// option is read again after it. Once the chosen build is the newest,
    /// the older builds the bridge installed go ([`Look::remove_older`]). A
    /// failed lookup or installation changes nothing.
    pub fn run(
        &self,
        wait: impl FnOnce() -> bool,
        probe: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<Outcome, String> {
        if !self.wanted() {
            return Ok(Outcome::Off);
        }
        let newest = self.known.look_up(self.transport, self.arch, self.now)?;
        let chosen = config::load_or_create(self.config_path)?.engine;
        let Some(current) = chosen.as_deref().and_then(|c| stockfish::installed_version(self.data, c)) else {
            return Ok(Outcome::NotOurs);
        };
        if !stockfish::is_newer(&newest.version, &current) {
            self.remove_older();
            return Ok(Outcome::Current);
        }
        let version = newest.version.to_string();
        let install = || stockfish::install(self.data, &newest, self.transport, &mut |_| {});
        if !self.choices.update(self.config_path, install, || wait() && self.wanted(), probe)? {
            return Ok(if self.wanted() { Outcome::Kept(version) } else { Outcome::Withdrawn(version) });
        }
        self.remove_older();
        Ok(Outcome::Updated(version))
    }

    /// Whether «Update Stockfish automatically» is on now.
    fn wanted(&self) -> bool {
        prefs::load(self.data).stockfish_auto_update
    }

    /// Removes the builds the bridge installed that are older than the
    /// chosen one, while the option is on and the chosen engine is a build
    /// the bridge installed. The choice is read when no engine can be chosen
    /// and no build installed ([`Choices::while_settled`]), so that the build
    /// chosen then is never removed; while an installation runs, a later look
    /// removes them.
    fn remove_older(&self) {
        let removal = self.choices.while_settled(|| {
            let chosen = config::load_or_create(self.config_path).ok()?.engine?;
            let kept = stockfish::installed_version(self.data, &chosen)?;
            self.wanted().then(|| stockfish::remove_older(self.data, &kept))
        });
        let Some(Some((removed, failed))) = removal else { return };
        for version in removed {
            bridge::log!("Stockfish update: Stockfish {version} removed");
        }
        for reason in failed {
            bridge::log!("Stockfish update: an older build stays for now: {reason}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // SHA-256 of "hello world".
    const HELLO: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    /// When the releases of these tests were published: 2026-09-05T08:33:17Z.
    const PUBLISHED: u64 = 1_788_597_197;

    /// The tests' time: a day after the release settled.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(PUBLISHED) + SETTLE + Duration::from_secs(86_400)
    }

    /// GitHub's answer about a release, trimmed to what is read and some of
    /// what is not.
    fn answer(tag: &str, assets: &[(&str, u64, Option<&str>)]) -> Vec<u8> {
        let assets: Vec<serde_json::Value> = assets
            .iter()
            .map(|(name, size, digest)| {
                serde_json::json!({
                    "name": name,
                    "size": size,
                    "digest": digest,
                    "browser_download_url": format!("https://example.com/{name}"),
                    "uploader": { "login": "github-actions[bot]" },
                })
            })
            .collect();
        serde_json::json!({
            "tag_name": tag,
            "name": "Stockfish",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-09-05T08:33:17Z",
            "assets": assets,
        })
        .to_string()
        .into_bytes()
    }

    /// Stockfish `version`'s answer, whose x86-64 build is "hello world".
    fn hello(version: &str) -> Vec<u8> {
        let digest = format!("sha256:{HELLO}");
        answer(
            &format!("sf_{version}"),
            &[
                ("stockfish-ubuntu-x86-64-avx2.tar", 5, Some(&digest)),
                ("stockfish-windows-x86-64-universal.zip", 11, Some(&digest)),
            ],
        )
    }

    /// Answers `answer` to a lookup, serves `archive` as the download and
    /// unpacks a build of the x86-64 asset's names.
    struct Fake {
        answer: Result<Vec<u8>, String>,
        archive: Vec<u8>,
        downloads: Mutex<Vec<String>>,
    }

    impl Transport for Fake {
        fn download(&self, url: &str, to: &Path, _max_size: u64) -> Result<(), String> {
            lock(&self.downloads).push(url.to_string());
            std::fs::write(to, &self.archive).map_err(|e| e.to_string())
        }

        fn extract(&self, _zip: &Path, members: &[String], to: &Path) -> Result<(), String> {
            for member in members {
                let path = to.join(member);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"MZ").unwrap();
            }
            Ok(())
        }

        fn fetch(&self, url: &str, max_size: u64) -> Result<Vec<u8>, String> {
            assert_eq!((url, max_size), (stockfish::LATEST, stockfish::MAX_ANSWER));
            self.answer.clone()
        }
    }

    fn fake(answer: Result<Vec<u8>, String>) -> Fake {
        Fake { answer, archive: b"hello world".to_vec(), downloads: Mutex::new(Vec::new()) }
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-app-stockfish-updates-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Puts the bridge's build of `version` in the data folder `dir`, as an
    /// installation leaves it, and answers its executable.
    fn installed(dir: &Path, version: &str) -> PathBuf {
        let build = Build::released(Arch::X86_64, version, 1, &"0".repeat(64)).unwrap();
        for file in [build.installed(dir), build.dir(dir).join(stockfish::LICENCE)] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"MZ").unwrap();
        }
        build.installed(dir)
    }

    fn choose(dir: &Path, engine: &Path) {
        config::update(&dir.join("bridge.toml"), |c| config::Config { engine: Some(engine.into()), ..c.clone() })
            .unwrap();
    }

    fn chosen(dir: &Path) -> Option<PathBuf> {
        config::load_or_create(&dir.join("bridge.toml")).unwrap().engine
    }

    fn builds(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("engines"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Runs a look in `dir` with `transport`, the bridge idle and every build
    /// answering.
    fn look(dir: &Path, choices: &Choices, known: &Known, transport: &Fake) -> Result<Outcome, String> {
        let config_path = dir.join("bridge.toml");
        let look =
            Look { data: dir, config_path: &config_path, choices, known, transport, arch: Arch::X86_64, now: now() };
        look.run(|| true, |_| Ok(()))
    }

    #[test]
    fn reads_the_build_of_this_architecture_from_githubs_answer() {
        let digest = "sha256:8372ad3f0d7276deb2c70f801f541ec7db463219fc6d9c7592864e542aa4f401";
        let stockfish_19 = answer(
            "sf_19",
            &[
                ("stockfish-windows-arm64-universal.zip", 80_190_536, Some(digest)),
                (
                    "stockfish-windows-x86-64-universal.zip",
                    81_431_614,
                    Some("sha256:3c8bf1f9ea66a09350a40df4f632288285ac206d99f33ab5842c408fc30b48a7"),
                ),
            ],
        );
        for arch in [Arch::X86_64, Arch::Arm64] {
            assert_eq!(&release(&stockfish_19, arch, now()).unwrap(), Build::for_arch(arch), "{arch:?}");
        }
        let build = release(&hello("20.1"), Arch::X86_64, now()).unwrap();
        assert_eq!((&*build.version, build.size, &*build.sha256), ("20.1", 11, HELLO));
        assert!(build.url().ends_with("/sf_20.1/stockfish-windows-x86-64-universal.zip"), "the bridge's own address");
    }

    /// A draft, a pre-release, a tag of another form, a missing asset or
    /// digest, and an answer that is no release are refused.
    #[test]
    fn refuses_an_answer_that_does_not_fit() {
        let mut draft: serde_json::Value = serde_json::from_slice(&hello("20")).unwrap();
        draft["draft"] = true.into();
        let mut pre: serde_json::Value = serde_json::from_slice(&hello("20")).unwrap();
        pre["prerelease"] = true.into();
        let refused = [
            draft.to_string().into_bytes(),
            pre.to_string().into_bytes(),
            answer("stockfish-dev-20260930-49ea5ded", &[("stockfish-windows-x86-64-universal.zip", 11, Some(HELLO))]),
            answer("sf_dev", &[("stockfish-windows-x86-64-universal.zip", 11, Some(&format!("sha256:{HELLO}")))]),
            answer("sf_20", &[("stockfish-windows-arm64-universal.zip", 11, Some(&format!("sha256:{HELLO}")))]),
            answer("sf_20", &[("stockfish-windows-x86-64-universal.zip", 11, None)]),
            answer("sf_20", &[("stockfish-windows-x86-64-universal.zip", 11, Some(&format!("sha1:{HELLO}")))]),
            answer("sf_20", &[("stockfish-windows-x86-64-universal.zip", 0, Some(&format!("sha256:{HELLO}")))]),
            b"{\"message\": \"API rate limit exceeded\"}".to_vec(),
            b"not json".to_vec(),
        ];
        for answer in refused {
            assert!(release(&answer, Arch::X86_64, now()).is_err(), "{}", String::from_utf8_lossy(&answer));
        }
    }

    /// A release counts once it is a week old, and not a second before; one
    /// with no readable publication time never does.
    #[test]
    fn a_release_counts_once_it_is_a_week_old() {
        let published = SystemTime::UNIX_EPOCH + Duration::from_secs(PUBLISHED);
        let error = release(&hello("20"), Arch::X86_64, published + SETTLE - Duration::from_secs(1)).unwrap_err();
        assert_eq!(error, "Stockfish 20 is less than 7 days old");
        assert!(release(&hello("20"), Arch::X86_64, published).is_err(), "just published");
        assert!(release(&hello("20"), Arch::X86_64, SystemTime::UNIX_EPOCH).is_err(), "a clock before it");
        assert_eq!(release(&hello("20"), Arch::X86_64, published + SETTLE).unwrap().version, "20");

        let mut undated: serde_json::Value = serde_json::from_slice(&hello("20")).unwrap();
        for when in [serde_json::Value::Null, "2026-09-05 08:33:17".into(), "yesterday".into(), 1_788_597_197.into()] {
            undated["published_at"] = when.clone();
            let answer = undated.to_string().into_bytes();
            assert!(release(&answer, Arch::X86_64, now()).is_err(), "{when}");
        }

        // A young release leaves the newest build known as it was, and
        // replaces nothing.
        let dir = folder("young");
        let exe = installed(&dir, "19");
        choose(&dir, &exe);
        let (choices, known, transport) = (Choices::new(), Known::new(), fake(Ok(hello("20"))));
        let config_path = dir.join("bridge.toml");
        let young = Look {
            data: &dir,
            config_path: &config_path,
            choices: &choices,
            known: &known,
            transport: &transport,
            arch: Arch::X86_64,
            now: published + Duration::from_secs(3_600),
        };
        assert_eq!(young.run(|| true, |_| Ok(())), Err("Stockfish 20 is less than 7 days old".into()));
        assert_eq!(&known.newest(Arch::X86_64), Build::for_arch(Arch::X86_64));
        assert_eq!(chosen(&dir), Some(exe));
        assert!(lock(&transport.downloads).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_githubs_times() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2026-09-05T08:33:17Z"), Some(PUBLISHED));
        assert_eq!(unix_seconds("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(unix_seconds("2024-03-01T00:00:00Z"), Some(1_709_251_200));
        for bad in [
            "",
            "2026-09-05T08:33:17",
            "2026-09-05 08:33:17Z",
            "2026-09-05T08:33:17.5Z",
            "1969-12-31T23:59:59Z",
            "2026-13-05T08:33:17Z",
            "2026-09-00T08:33:17Z",
            "2026-09-05T24:00:00Z",
            "2026-09-05T08:60:00Z",
            "+026-09-05T08:33:17Z",
            "2026-09-05T08:33:1éZ",
        ] {
            assert_eq!(unix_seconds(bad), None, "{bad:?}");
        }
    }

    /// The newest build is the pinned one until a lookup names a newer
    /// release; an older or failed answer leaves it.
    #[test]
    fn the_newest_build_known_is_the_pinned_one_until_a_lookup_names_a_newer() {
        let known = Known::new();
        assert_eq!(&known.newest(Arch::X86_64), Build::for_arch(Arch::X86_64));
        assert!(known.look_up(&fake(Err("offline".into())), Arch::X86_64, now()).is_err());
        assert_eq!(&known.newest(Arch::X86_64), Build::for_arch(Arch::X86_64));
        assert_eq!(&known.look_up(&fake(Ok(hello("18"))), Arch::X86_64, now()).unwrap(), Build::for_arch(Arch::X86_64));
        assert_eq!(known.look_up(&fake(Ok(hello("20"))), Arch::X86_64, now()).unwrap().version, "20");
        assert_eq!(known.newest(Arch::X86_64).version, "20");
        assert_eq!(&known.newest(Arch::Arm64), Build::for_arch(Arch::Arm64), "another architecture's build");
    }

    /// The bridge's older build is replaced: the release's build is
    /// installed and chosen, and the older builds go.
    #[test]
    fn replaces_the_older_build_the_bridge_installed() {
        let dir = folder("replace");
        installed(&dir, "18");
        choose(&dir, &installed(&dir, "19"));
        let (choices, known, transport) = (Choices::new(), Known::new(), fake(Ok(hello("20"))));
        assert_eq!(look(&dir, &choices, &known, &transport), Ok(Outcome::Updated("20".into())));
        let build = known.newest(Arch::X86_64);
        assert_eq!(chosen(&dir), Some(build.installed(&dir)));
        assert_eq!(builds(&dir), ["stockfish-20"]);
        assert_eq!(*lock(&transport.downloads), [build.url()]);
        assert_eq!(look(&dir, &choices, &known, &transport), Ok(Outcome::Current), "then it is current");
        assert_eq!(lock(&transport.downloads).len(), 1, "and nothing downloads again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another engine is never replaced, nor is no engine: the lookup only
    /// tells the engine section the newest release.
    #[test]
    fn replaces_no_other_engine() {
        let dir = folder("other");
        let (choices, known, transport) = (Choices::new(), Known::new(), fake(Ok(hello("20"))));
        assert_eq!(look(&dir, &choices, &known, &transport), Ok(Outcome::NotOurs), "no engine");
        for engine in [r"C:\CB\Engines.x64\Stockfish 17.1\sf.exe", r"C:\x\stockfish-19\stockfish.exe"] {
            choose(&dir, Path::new(engine));
            assert_eq!(look(&dir, &choices, &known, &transport), Ok(Outcome::NotOurs), "{engine}");
        }
        installed(&dir, "18");
        assert_eq!(look(&dir, &choices, &known, &transport), Ok(Outcome::NotOurs));
        assert_eq!(builds(&dir), ["stockfish-18"], "a build not chosen stays too");
        assert!(lock(&transport.downloads).is_empty());
        assert_eq!(known.newest(Arch::X86_64).version, "20");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lookup that fails, and a download that does not match GitHub's
    /// digest, change nothing.
    #[test]
    fn a_failed_lookup_or_download_changes_nothing() {
        let dir = folder("failed");
        let exe = installed(&dir, "19");
        choose(&dir, &exe);
        let choices = Choices::new();
        let offline = look(&dir, &choices, &Known::new(), &fake(Err("The lookup failed (exit code: 6)".into())));
        assert_eq!(offline, Err("The lookup failed (exit code: 6)".into()));
        let mut tampered = fake(Ok(hello("20")));
        tampered.archive = b"hello World".to_vec();
        let error = look(&dir, &choices, &Known::new(), &tampered).unwrap_err();
        assert!(error.contains("SHA-256"), "{error}");
        assert_eq!(chosen(&dir), Some(exe));
        assert_eq!(builds(&dir), ["stockfish-19"]);
        assert!(!choices.installing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the option off, nothing is looked up; turned off while an update
    /// waits, the update chooses nothing and removes nothing (#322 review).
    #[test]
    fn the_option_turned_off_stops_a_look_and_a_waiting_update() {
        let dir = folder("off");
        let exe = installed(&dir, "19");
        choose(&dir, &exe);
        let config_path = dir.join("bridge.toml");
        let (choices, known) = (Choices::new(), Known::new());
        prefs::update(&dir, |p| p.stockfish_auto_update = false).unwrap();
        let offline = fake(Err("no lookup may run".into()));
        assert_eq!(look(&dir, &choices, &known, &offline), Ok(Outcome::Off));

        prefs::update(&dir, |p| p.stockfish_auto_update = true).unwrap();
        installed(&dir, "18");
        let transport = fake(Ok(hello("20")));
        let look = Look {
            data: &dir,
            config_path: &config_path,
            choices: &choices,
            known: &known,
            transport: &transport,
            arch: Arch::X86_64,
            now: now(),
        };
        let off_while_waiting = || {
            prefs::update(&dir, |p| p.stockfish_auto_update = false).unwrap();
            true
        };
        assert_eq!(look.run(off_while_waiting, |_| Ok(())), Ok(Outcome::Withdrawn("20".into())));
        assert_eq!(chosen(&dir), Some(exe.clone()));
        assert_eq!(builds(&dir), ["stockfish-18", "stockfish-19", "stockfish-20"]);
        // A wait that gives up by itself chooses nothing either.
        prefs::update(&dir, |p| p.stockfish_auto_update = true).unwrap();
        assert_eq!(look.run(|| false, |_| Ok(())), Ok(Outcome::Kept("20".into())));
        assert_eq!(chosen(&dir), Some(exe));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The removal reads the choice it keeps when it runs: an older build
    /// the user chose after the update stays, with every newer one, and a
    /// choice of another engine removes nothing (#322 review).
    #[test]
    fn the_removal_keeps_the_build_chosen_when_it_runs() {
        let dir = folder("removal");
        let config_path = dir.join("bridge.toml");
        let older = installed(&dir, "18");
        installed(&dir, "17");
        installed(&dir, "19");
        installed(&dir, "20");
        let (choices, known, transport) = (Choices::new(), Known::new(), fake(Ok(hello("20"))));
        let look = Look {
            data: &dir,
            config_path: &config_path,
            choices: &choices,
            known: &known,
            transport: &transport,
            arch: Arch::X86_64,
            now: now(),
        };
        choose(&dir, Path::new(r"C:\lc0\lc0.exe"));
        look.remove_older();
        assert_eq!(builds(&dir), ["stockfish-17", "stockfish-18", "stockfish-19", "stockfish-20"], "another engine");
        choices.choose(&config_path, older.clone(), |_| Ok::<(), String>(())).unwrap();
        look.remove_older();
        assert_eq!(builds(&dir), ["stockfish-18", "stockfish-19", "stockfish-20"]);
        assert!(older.is_file(), "the chosen build stays");
        // Not while an installation runs; the option off removes nothing.
        let during = choices.install(
            &config_path,
            || {
                choose(&dir, &installed(&dir, "20"));
                look.remove_older();
                assert_eq!(builds(&dir), ["stockfish-18", "stockfish-19", "stockfish-20"]);
                Ok(older.clone())
            },
            |_| Ok(()),
        );
        assert_eq!(during, Ok(true));
        choose(&dir, &installed(&dir, "20"));
        prefs::update(&dir, |p| p.stockfish_auto_update = false).unwrap();
        look.remove_older();
        assert_eq!(builds(&dir), ["stockfish-18", "stockfish-19", "stockfish-20"]);
        prefs::update(&dir, |p| p.stockfish_auto_update = true).unwrap();
        look.remove_older();
        assert_eq!(builds(&dir), ["stockfish-20"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The choice waits until the bridge is idle, and one the user makes
    /// meanwhile stands; the new build stays listed, and the older one too.
    #[test]
    fn a_choice_made_while_the_update_waits_stands() {
        let dir = folder("waits");
        let exe = installed(&dir, "19");
        choose(&dir, &exe);
        let config_path = dir.join("bridge.toml");
        let (choices, known, transport) = (Choices::new(), Known::new(), fake(Ok(hello("20"))));
        let look = Look {
            data: &dir,
            config_path: &config_path,
            choices: &choices,
            known: &known,
            transport: &transport,
            arch: Arch::X86_64,
            now: now(),
        };
        let wait = || {
            assert_eq!(chosen(&dir), Some(exe.clone()), "nothing is chosen before the bridge is idle");
            choices.choose(&config_path, PathBuf::from(r"C:\lc0\lc0.exe"), |_| Ok::<(), String>(())).unwrap();
            true
        };
        assert_eq!(look.run(wait, |_| Ok(())), Ok(Outcome::Kept("20".into())));
        assert_eq!(chosen(&dir), Some(PathBuf::from(r"C:\lc0\lc0.exe")));
        assert_eq!(builds(&dir), ["stockfish-19", "stockfish-20"]);
        // A build that does not answer is not chosen either.
        choose(&dir, &exe);
        let refused = look.run(|| true, |_| Err("no uciok".into()));
        assert_eq!(refused, Err("no uciok".into()));
        assert_eq!(chosen(&dir), Some(exe));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
