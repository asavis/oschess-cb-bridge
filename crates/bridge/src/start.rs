//! Starting the bridge, shared by `oschess-bridge`, `cbtool bridge` and the
//! Windows application to come: the settings, the port, the pairing token and the
//! databases served.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::{DEFAULT_ORIGINS, Policy};
use crate::api::App;
use crate::catalog::Catalog;
use crate::fetch::System;
use crate::sources::Sources;
use crate::{config, documents, pairing, server, token};

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
    token::data_dir().ok_or_else(|| "no data folder: set OSCHESS_BRIDGE_HOME".to_string())
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

/// Prepares the bridge whose data folder is `dir`, creating the folder, its
/// `bridge.toml` and the token when they are missing.
pub fn prepare(dir: &Path, options: &Options) -> Result<Bridge, String> {
    let first_run = !token::exists(dir);
    let config = config::load_or_create(&dir.join("bridge.toml"))?;
    let mut origins: Vec<String> = DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect();
    origins.extend(config.origins);
    let web = config.web.trim_end_matches('/');
    if !origins.iter().any(|o| o == web) {
        return Err(format!("web = \"{}\" in bridge.toml is not an allowed origin", config.web));
    }
    // The port first: a second instance stops here, before it could replace the
    // token the running one still accepts.
    let listeners = server::bind(config.port).map_err(|e| format!("port {}: {e}", config.port))?;
    let token = if options.new_token { token::replace(dir) } else { token::load_or_create(dir) }
        .map_err(|e| format!("pairing token in {}: {e}", dir.display()))?;
    // The databases of bridge.toml are read by the catalog, again whenever
    // the file changes.
    let config_path = dir.join("bridge.toml");
    let sources = Sources {
        chessbase: documents::chessbase_folder(),
        config: Some(config_path),
        fixed: options.databases.clone(),
    };
    let link = pairing::link(web, &token, config.port);
    let app = App {
        version: env!("CARGO_PKG_VERSION"),
        policy: Policy { port: config.port, origins, token: token.clone() },
        catalog: Catalog::with_sources(sources, Arc::new(System)),
        between_reads: None,
    };
    app.catalog.explorer.set_dir(dir.join("index"));
    Ok(Bridge { listeners, app: Arc::new(app), port: config.port, token, link, first_run })
}

/// The message for an option error: the error, then how to call `program`.
pub fn usage(program: &str, error: &str) -> String {
    format!("{error}\n\nusage: {program} {OPTIONS}")
}

/// Runs the bridge in a console until the process ends.
pub fn console(program: &str, args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let options = Options::parse(args).map_err(|e| usage(program, &e))?;
    let bridge = prepare(&data_dir()?, &options)?;
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
            Options { databases: vec!["a b.2cbh".into(), "c".into()], show_token: true, new_token: true }
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

    #[test]
    fn the_first_run_is_the_one_that_creates_the_token() {
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
        let dir = folder("staging", Some("https://staging.oschess.org/"));
        let bridge = prepare(&dir, &Options::default()).unwrap();
        assert!(bridge.link.starts_with("https://staging.oschess.org/library?"));
        drop(bridge);
        let _ = std::fs::remove_dir_all(&dir);

        let dir = folder("foreign", Some("https://example.com"));
        let e = prepare(&dir, &Options::default()).err().unwrap();
        assert!(e.contains("not an allowed origin"), "{e}");
        assert!(!token::exists(&dir), "a refused start creates no token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_port_stops_the_start_before_the_token() {
        let dir = folder("taken", None);
        let running = prepare(&dir, &Options::default()).unwrap();
        let e = prepare(&dir, &Options { new_token: true, ..Options::default() }).err().unwrap();
        assert!(e.starts_with(&format!("port {}", running.port)), "{e}");
        assert_eq!(token::load_or_create(&dir).unwrap(), running.token, "the running bridge keeps its token");
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
