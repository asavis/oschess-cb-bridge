//! Starting the bridge, shared by `oschess-bridge`, `cbtool bridge` and the
//! Windows application (`crates/app`): the settings, the port, the pairing
//! token and the databases served.

use std::fmt;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::access::{DEFAULT_ORIGINS, Policy};
use crate::api::App;
use crate::catalog::Catalog;
use crate::engine::Engine;
use crate::fetch::System;
use crate::sources::Sources;
use crate::{config, documents, folders, log, pairing, server, token};

/// The options every way of starting the bridge takes.
pub const OPTIONS: &str = "[--database <path>]... [--show-token] [--new-token]

  --database <path>   serve this database too (repeatable)
  --show-token        show the pairing link, which contains the token
  --new-token         replace the pairing token; paired browsers must pair again

The databases are those of ChessBase's database window (DBItems.cbini in
Documents\\ChessBase; OSCHESS_BRIDGE_DOCUMENTS names another Documents folder),
then those of bridge.toml, then --database.

Settings live in bridge.toml in the data folder (OSCHESS_BRIDGE_HOME, else
%APPDATA%\\oschess-bridge on Windows, ~/.config/oschess-bridge elsewhere).";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub databases: Vec<PathBuf>,
    pub show_token: bool,
    pub new_token: bool,
    /// The version the bridge reports in its status: the Windows app's own,
    /// which its release carries. `None` reports this library's version, as
    /// `oschess-bridge` and `cbtool bridge` do.
    pub version: Option<&'static str>,
    /// How long a start tries again while another listener holds its port:
    /// the Windows app's wait, as after a restart the process before may
    /// still hold the port for a moment. Zero, the default, fails at once, as
    /// `oschess-bridge` and `cbtool bridge` do.
    pub port_wait: Duration,
}

impl Options {
    /// Reads the options that follow the program name.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut options = Options::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--database" => options.databases.push(PathBuf::from(args.next().ok_or("--database needs a path")?)),
                "--show-token" => options.show_token = true,
                "--new-token" => options.new_token = true,
                _ => return Err(format!("unknown option {arg}")),
            }
        }
        Ok(options)
    }
}

/// The data folder, or an error that says how to set one.
pub fn data_dir() -> Result<PathBuf, String> {
    folders::data_dir().ok_or_else(|| "no data folder: set OSCHESS_BRIDGE_HOME".to_string())
}

/// A bridge ready to serve: its port bound and its token read.
pub struct Bridge {
    pub listeners: Vec<TcpListener>,
    pub app: Arc<App>,
    pub port: u16,
    pub token: String,
    /// The pairing link. It contains the token: it is shown only on request and
    /// never logged.
    pub link: String,
    /// No token existed before this start: the bridge was just installed, or
    /// its data folder was removed.
    pub first_run: bool,
}

/// How often a start that waits for its port tries again.
const PORT_RETRY: Duration = Duration::from_millis(250);

/// Prepares the bridge whose data folder is `dir`, creating the folder, its
/// `bridge.toml` and the token when they are missing. From now on the log
/// goes to `bridge.log` there too, and it records how the start went: in one
/// line, however often it tried again for its port (#185).
pub fn prepare(dir: &Path, options: &Options) -> Result<Bridge, Failure> {
    // First, so that a start that fails is in the file.
    log::open(dir);
    let started = Instant::now();
    let tried = loop {
        match ready(dir, options) {
            Err(Failure { cause: Cause::PortInUse(_), .. }) if started.elapsed() < options.port_wait => {
                std::thread::sleep(PORT_RETRY);
            }
            tried => break tried,
        }
    };
    match tried {
        Ok(bridge) => {
            crate::log!("the bridge {} starts on port {}", bridge.app.version, bridge.port);
            Ok(bridge)
        }
        Err(failure) => {
            crate::log!("the bridge cannot start: {}", failure.logged);
            Err(failure)
        }
    }
}

/// Why a start failed: what stopped it, for the app to tell apart (#185),
/// the message the console and the app show, and the log's, which names no
/// path and quotes nothing of `bridge.toml`.
#[derive(Debug)]
pub struct Failure {
    pub cause: Cause,
    shown: String,
    logged: String,
}

/// What stopped a start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Another listener holds this port, the one `bridge.toml` names: a
    /// bridge that runs already, or for a moment the one before a restart.
    PortInUse(u16),
    /// Anything else, with the port `bridge.toml` names; `None` when the file
    /// cannot be read.
    Other { port: Option<u16> },
}

impl Failure {
    /// A message that quotes nothing of the user's, shown and logged alike.
    fn plain(cause: Cause, message: String) -> Failure {
        Failure { cause, logged: message.clone(), shown: message }
    }
}

/// The message shown.
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.shown)
    }
}

/// [`prepare`], tried once, its failure kept whole for the log.
fn ready(dir: &Path, options: &Options) -> Result<Bridge, Failure> {
    let first_run = !token::exists(dir);
    let config_path = dir.join(config::FILE_NAME);
    let config = config::load(&config_path).map_err(|e| Failure {
        cause: Cause::Other { port: None },
        shown: e.to_string(),
        logged: e.logged(),
    })?;
    let (origins, web) = allowed(&config)?;
    // The port first: a second instance stops here, before it could replace the
    // token the running one still accepts.
    let listeners = server::bind(config.port).map_err(|e| {
        let cause = match e.kind() {
            std::io::ErrorKind::AddrInUse => Cause::PortInUse(config.port),
            _ => Cause::Other { port: Some(config.port) },
        };
        Failure::plain(cause, format!("port {}: {e}", config.port))
    })?;
    let token =
        if options.new_token { token::replace(dir) } else { token::load_or_create(dir) }.map_err(|e| Failure {
            cause: Cause::Other { port: Some(config.port) },
            shown: format!("pairing token in {}: {e}", dir.display()),
            logged: format!("the pairing token: {e}"),
        })?;
    // The port, the origins and the site are read once, above. The databases
    // and the engine follow the file while the bridge runs, through one
    // reader of it, so that a file that cannot be read is logged once
    // (#175).
    let file = Arc::new(config::Watched::new(config_path));
    let sources = Sources {
        chessbase: documents::chessbase_folder(),
        config: Some(Arc::clone(&file)),
        fixed: options.databases.clone(),
    };
    let link = pairing::link(web, &token, config.port);
    let version = options.version.unwrap_or(env!("CARGO_PKG_VERSION"));
    let policy = Policy { port: config.port, origins, token: token.clone() };
    let app = setup(dir, version, policy, sources, Engine::from_config_file(file));
    Ok(Bridge { listeners, app: Arc::new(app), port: config.port, token, link, first_run })
}

/// The app of a bridge whose data folder is `dir`, serving the databases of
/// `sources` under `policy` with `engine`: its indexes kept where that data
/// folder keeps them, and swept of what the list no longer uses. Every start
/// sets its bridge up here, and so do the bridges `cbtool profile` times,
/// which serve one database with the engine they are given (#191).
pub fn setup(dir: &Path, version: &'static str, policy: Policy, sources: Sources, engine: Engine) -> App {
    let app = App { engine, ..App::new(version, policy, Catalog::with_sources(sources, Arc::new(System))) };
    app.catalog.use_data_dir(dir);
    // The indexes are kept apart from the data folder where it roams (#147).
    // Those kept in it before are rebuilt anyway, for the index's new
    // version, so they go, once.
    let before = dir.join("index");
    if app.catalog.explorer.dir().is_some_and(|index| index != before) {
        crate::indexdir::sweep_moved(&before, |name| {
            crate::explorer::is_index_file(name) || crate::search::heads::entry_id(name).is_some()
        });
    }
    app.catalog.sweep_indexes();
    app
}

/// The origins the bridge serves under `config`, oschess's own first, and the
/// site its pairing link opens: `web` without a trailing slash, which must be
/// one of them, since the link carries the token there. The one rule for a
/// start and for a pairing read without one (#183).
fn allowed(config: &config::Config) -> Result<(Vec<String>, &str), Failure> {
    let mut origins: Vec<String> = DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect();
    origins.extend(config.origins.iter().cloned());
    let web = config.web.trim_end_matches('/');
    if !origins.iter().any(|o| o == web) {
        return Err(Failure {
            cause: Cause::Other { port: Some(config.port) },
            shown: format!("web = \"{}\" in bridge.toml is not an allowed origin", config.web),
            logged: "web in bridge.toml is not an allowed origin".into(),
        });
    }
    Ok((origins, web))
}

/// What a browser pairs with: the token, and the pairing link that carries
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub token: String,
    /// It contains the token: it is shown only on request and never logged.
    pub link: String,
}

/// The pairing of the bridge whose data folder is `dir`, read while it does
/// not serve: the stored token and the link a start would make with it. A
/// site that is not an allowed origin gets no link, as a start refuses it.
/// The token is never created here: the start that creates it is the first
/// run (#183).
pub fn pairing(dir: &Path) -> Result<Pairing, String> {
    let config = config::load_or_create(&dir.join(config::FILE_NAME))?;
    let (_, web) = allowed(&config).map_err(|f| f.shown)?;
    let token = token::load(dir)
        .map_err(|e| format!("pairing token in {}: {e}", dir.display()))?
        .ok_or("no pairing token yet: the bridge makes it when it first starts")?;
    Ok(Pairing { link: pairing::link(web, &token, config.port), token })
}

/// The oschess site the pairing link of the bridge in `dir` opens, under the
/// rule of [`pairing`], for a page that carries no token.
pub fn site(dir: &Path) -> Result<String, String> {
    let config = config::load_or_create(&dir.join(config::FILE_NAME))?;
    allowed(&config).map(|(_, web)| web.to_string()).map_err(|f| f.shown)
}

/// The message for an option error: the error, then how to call `program`.
pub fn usage(program: &str, error: &str) -> String {
    format!("{error}\n\nusage: {program} {OPTIONS}")
}

/// Runs the bridge in a console until the process ends.
pub fn console(program: &str, args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = Options::parse(args).map_err(|e| usage(program, &e))?;
    let bridge = prepare(&data_dir()?, &options).map_err(|e| e.to_string())?;
    serve_console(bridge, options.show_token)
}

/// Serves `bridge` until the process ends, after printing where it listens,
/// the databases it serves and, with `show_token`, the pairing link.
pub fn serve_console(bridge: Bridge, show_token: bool) -> Result<(), String> {
    for l in &bridge.listeners {
        if let Ok(addr) = l.local_addr() {
            println!("oschess bridge {} on http://{addr}", bridge.app.version);
        }
    }
    for e in bridge.app.catalog.entries() {
        println!("  {} [{}] {}", e.id, e.state().name(), e.name);
    }
    if show_token {
        println!("pairing link: {}", bridge.link);
    }
    crate::explorer::keeper::start(&bridge.app);
    server::serve(bridge.listeners, bridge.app).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn options() {
        assert_eq!(Options::parse(args(&[])).unwrap(), Options::default());
        let o = Options::parse(args(&["--database", "a b.2cbh", "--show-token", "--database", "c", "--new-token"]));
        assert_eq!(
            o.unwrap(),
            Options {
                databases: vec!["a b.2cbh".into(), "c".into()],
                show_token: true,
                new_token: true,
                version: None,
                port_wait: Duration::ZERO
            }
        );
        assert!(Options::parse(args(&["--database"])).is_err());
        assert!(Options::parse(args(&["--port", "1"])).is_err());
        let text = usage("cbtool bridge", "unknown option --port");
        assert!(text.starts_with("unknown option --port\n\nusage: cbtool bridge [--database <path>]..."), "{text}");
    }

    /// A data folder whose settings name a port that was free a moment ago.
    fn folder(name: &str, web: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-start-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = server::bind(0).unwrap()[0].local_addr().unwrap().port();
        let web = web.map(|w| format!("web = \"{w}\"\n")).unwrap_or_default();
        std::fs::write(dir.join("bridge.toml"), format!("port = {port}\n{web}")).unwrap();
        dir
    }

    /// The port the settings of `dir` name.
    fn port_of(dir: &Path) -> u16 {
        config::load(&dir.join(config::FILE_NAME)).unwrap().port
    }

    /// The lines of the log in `dir` that contain `text`.
    fn logged(dir: &Path, text: &str) -> usize {
        let log = std::fs::read_to_string(dir.join(log::FILE_NAME)).unwrap();
        log.lines().filter(|l| l.contains(text)).count()
    }

    #[test]
    fn the_first_run_is_the_one_that_creates_the_token() {
        let _log = log::testing::hold();
        let dir = folder("first", None);
        let first = prepare(&dir, &Options::default()).unwrap();
        assert!(first.first_run);
        assert!(token::is_valid(&first.token));
        assert_eq!(first.link, pairing::link(pairing::DEFAULT_WEB, &first.token, first.port));
        let token = first.token.clone();
        drop(first);
        let again = prepare(&dir, &Options::default()).unwrap();
        assert!(!again.first_run);
        assert_eq!(again.token, token);
        drop(again);
        let renewed = prepare(&dir, &Options { new_token: true, ..Options::default() }).unwrap();
        assert!(!renewed.first_run);
        assert_ne!(renewed.token, token);
        drop(renewed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_site_must_be_an_allowed_origin() {
        let _log = log::testing::hold();
        let dir = folder("staging", Some("https://staging.oschess.org/"));
        let bridge = prepare(&dir, &Options::default()).unwrap();
        assert!(bridge.link.starts_with("https://staging.oschess.org/library?"));
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);

        let dir = folder("foreign", Some("https://example.com"));
        let e = prepare(&dir, &Options::default()).err().unwrap();
        assert!(e.to_string().contains("not an allowed origin"), "{e}");
        assert_eq!(e.cause, Cause::Other { port: Some(port_of(&dir)) });
        assert!(!token::exists(&dir), "a refused start creates no token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read without a start, the pairing is the one the start makes, and a
    /// read before the first start creates no token: the start after it is
    /// still the first run (#183).
    #[test]
    fn the_pairing_read_without_a_start_is_the_starts() {
        let _log = log::testing::hold();
        let dir = folder("pairing", Some("https://staging.oschess.org/"));
        let e = pairing(&dir).err().unwrap();
        assert!(e.starts_with("no pairing token yet"), "{e}");
        assert!(!token::exists(&dir), "reading the pairing creates no token");
        assert_eq!(site(&dir).unwrap(), "https://staging.oschess.org");
        let bridge = prepare(&dir, &Options::default()).unwrap();
        assert!(bridge.first_run);
        assert!(bridge.link.starts_with("https://staging.oschess.org/library?"));
        assert_eq!(pairing(&dir).unwrap(), Pairing { token: bridge.token.clone(), link: bridge.link.clone() });
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A site the start refuses gets no pairing link, even with a token
    /// stored, nor is it the site of a page without one (#183).
    #[test]
    fn a_refused_site_gets_no_pairing_link() {
        let dir = folder("pairing-foreign", Some("file:///C:/Windows/"));
        let e = pairing(&dir).err().unwrap();
        assert!(e.contains("not an allowed origin"), "{e}");
        assert!(!token::exists(&dir), "a refused read creates no token");
        token::replace(&dir).unwrap();
        let e = pairing(&dir).err().unwrap();
        assert!(e.contains("not an allowed origin"), "{e}");
        let e = site(&dir).err().unwrap();
        assert!(e.contains("not an allowed origin"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_the_version_it_is_given_else_its_own() {
        let _log = log::testing::hold();
        // The Windows app passes its release version, which differs from this
        // library's package version.
        let dir = folder("version", None);
        let app = prepare(&dir, &Options { version: Some("9.8.7"), ..Options::default() }).unwrap();
        assert_eq!(app.app.version, "9.8.7");
        drop(app);
        let console = prepare(&dir, &Options::default()).unwrap();
        assert_eq!(console.app.version, env!("CARGO_PKG_VERSION"));
        drop(console);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_port_stops_the_start_before_the_token() {
        let _log = log::testing::hold();
        let dir = folder("taken", None);
        let running = prepare(&dir, &Options::default()).unwrap();
        let e = prepare(&dir, &Options { new_token: true, ..Options::default() }).err().unwrap();
        assert!(e.to_string().starts_with(&format!("port {}", running.port)), "{e}");
        assert_eq!(e.cause, Cause::PortInUse(running.port));
        assert_eq!(token::load_or_create(&dir).unwrap(), running.token, "the running bridge keeps its token");
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A port that another listener holds stops the start as a port in use,
    /// which the app tells apart from other failures (#185). A start given a
    /// wait tries again until the port is free, and records one line
    /// however often it tried: a failure after the whole wait, else its start.
    #[test]
    fn a_port_held_by_another_listener_is_a_port_in_use() {
        let _log = log::testing::hold();
        let dir = folder("held", None);
        let port = port_of(&dir);
        let held = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let e = prepare(&dir, &Options::default()).err().unwrap();
        assert_eq!(e.cause, Cause::PortInUse(port));
        assert!(e.to_string().starts_with(&format!("port {port}: ")), "{e}");
        assert!(!token::exists(&dir), "a start stopped at the port creates no token");
        let failed = format!("the bridge cannot start: port {port}: ");
        assert_eq!(logged(&dir, &failed), 1);

        let wait = PORT_RETRY * 3;
        let started = Instant::now();
        let e = prepare(&dir, &Options { port_wait: wait, ..Options::default() }).err().unwrap();
        assert!(started.elapsed() >= wait, "it waits for the port");
        assert_eq!(e.cause, Cause::PortInUse(port));
        assert_eq!(logged(&dir, &failed), 2, "one line for a start that tried again");

        // Freed while the start waits: the start takes it.
        let freed = std::thread::spawn(move || {
            std::thread::sleep(PORT_RETRY * 2);
            drop(held);
        });
        let bridge = prepare(&dir, &Options { port_wait: Duration::from_secs(30), ..Options::default() }).unwrap();
        freed.join().unwrap();
        assert_eq!(bridge.port, port);
        assert_eq!(logged(&dir, &failed), 2, "a start that waited and started is no failure");
        assert_eq!(logged(&dir, &format!("starts on port {port}")), 1);
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only a port in use is waited for: any other failure ends the start at
    /// once, though the port is taken as well (#185).
    #[test]
    fn only_a_port_in_use_is_waited_for() {
        let _log = log::testing::hold();
        let dir = folder("refused-held", Some("https://example.com"));
        let port = port_of(&dir);
        let held = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let started = Instant::now();
        let e = prepare(&dir, &Options { port_wait: Duration::from_secs(60), ..Options::default() }).err().unwrap();
        assert!(started.elapsed() < Duration::from_secs(60), "it does not wait");
        assert_eq!(e.cause, Cause::Other { port: Some(port) });
        assert_eq!(logged(&dir, "the bridge cannot start: "), 1);
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A start is logged in the data folder, and so is one that fails, with
    /// no path in the log (#117).
    #[test]
    fn a_start_is_logged_in_the_data_folder() {
        let _log = log::testing::hold();
        let dir = folder("logged", None);
        let running = prepare(&dir, &Options::default()).unwrap();
        let port = running.port;
        assert!(prepare(&dir, &Options::default()).is_err());
        drop(running);
        std::fs::write(dir.join("bridge.toml"), format!("engine = {}\n", dir.display())).unwrap();
        let e = prepare(&dir, &Options::default()).err().unwrap();
        assert!(e.to_string().contains(&dir.display().to_string()), "the message shown names the file: {e}");
        assert_eq!(e.cause, Cause::Other { port: None }, "the port is unknown");
        let text = std::fs::read_to_string(dir.join(log::FILE_NAME)).unwrap();
        for line in [
            format!("the bridge {} starts on port {port}", env!("CARGO_PKG_VERSION")),
            format!("the bridge cannot start: port {port}: "),
            "the bridge cannot start: bridge.toml: line 1: not a value".to_string(),
        ] {
            assert!(text.lines().any(|l| l.contains(&line)), "{line} not in {text}");
        }
        assert!(!text.contains(dir.to_str().unwrap()), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file broken while the bridge runs is logged once for each broken
    /// revision, though the database list and the engine both follow it and
    /// look at it again and again (#175): a broken file changed into another
    /// broken one is logged again, so that the log says what is wrong now.
    #[test]
    fn a_broken_file_is_logged_once_for_each_change() {
        let _log = log::testing::hold();
        let dir = folder("broken", None);
        let bridge = prepare(&dir, &Options::default()).unwrap();
        let path = dir.join(config::FILE_NAME);
        let good = std::fs::read(&path).unwrap();
        // Replaced whole, so that no look reads it half written; each text of
        // another length than the one before, so that its signature changes
        // whatever the clock.
        let replace = |text: &[u8]| crate::files::write_atomic(&path, text).unwrap();
        let logged = || {
            // Each looks more than once, as requests and the engine's own
            // looks do.
            for _ in 0..2 {
                bridge.app.catalog.entries();
                bridge.app.engine.is_configured();
            }
            let text = std::fs::read_to_string(dir.join(log::FILE_NAME)).unwrap();
            text.lines().filter(|l| l.contains("keeping the settings read before")).count()
        };
        replace(b"port = \n");
        assert_eq!(logged(), 1);
        replace(b"databases = [unquoted]\n");
        assert_eq!(logged(), 2, "another broken revision is another line");
        replace(&good);
        assert_eq!(logged(), 2, "a good file is no line");
        replace(b"Jane = 1\n");
        assert_eq!(logged(), 3);
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A catalog given a data folder alone keeps its indexes where a start
    /// from that folder keeps them (#147, #175): a folder of the tests' or
    /// the tools' own, in its `index` and `pgn`; the default data folder,
    /// which only a real start uses, on Windows in the local application data
    /// folder, unless `OSCHESS_BRIDGE_HOME` names it.
    #[test]
    fn a_data_folder_keeps_its_indexes_where_a_start_does() {
        let _log = log::testing::hold();
        let dir = folder("indexes", None);
        let started = prepare(&dir, &Options::default()).unwrap();
        let alone = Catalog::new(Vec::new());
        alone.use_data_dir(&dir);
        let kept = |catalog: &Catalog| (catalog.explorer.dir(), catalog.pgn().dir());
        assert_eq!(kept(&started.app.catalog), (Some(dir.join("index")), Some(dir.join("pgn"))));
        assert_eq!(kept(&alone), kept(&started.app.catalog));
        drop(started);
        let _ = std::fs::remove_dir_all(&dir);

        // Only named: nothing is written there.
        let Some(data) = folders::data_dir() else { return };
        let alone = Catalog::new(Vec::new());
        alone.use_data_dir(&data);
        let var = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let index = match var("LOCALAPPDATA") {
            Some(local) if cfg!(windows) && var("OSCHESS_BRIDGE_HOME").is_none() => {
                PathBuf::from(local).join("oschess bridge").join("index")
            }
            _ => data.join("index"),
        };
        assert_eq!(kept(&alone), (Some(index), Some(data.join("pgn"))));
    }
}
