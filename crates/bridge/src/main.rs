//! `oschess-bridge`: runs the bridge in a console. The tray application of #23
//! wraps the same server.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::Catalog;
use bridge::{config, server, token};

const USAGE: &str = "usage: oschess-bridge [--database <file.2cbh>]... [--show-token] [--new-token]

  --database <path>   serve this database too (repeatable)
  --show-token        print the pairing link, which contains the token
  --new-token         replace the pairing token; paired browsers must pair again

Settings live in bridge.toml in the data folder (OSCHESS_BRIDGE_HOME, else
%APPDATA%\\oschess-bridge on Windows, ~/.config/oschess-bridge elsewhere).";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oschess-bridge: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut databases = Vec::new();
    let (mut show_token, mut new_token) = (false, false);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database" => databases.push(PathBuf::from(args.next().ok_or(USAGE)?)),
            "--show-token" => show_token = true,
            "--new-token" => new_token = true,
            _ => return Err(USAGE.into()),
        }
    }
    let dir = token::data_dir().ok_or("no data folder: set OSCHESS_BRIDGE_HOME")?;
    let config = config::load_or_create(&dir.join("bridge.toml"))?;
    // The port first: a second instance stops here, before it could replace the
    // token the running one still accepts.
    let listeners = server::bind(config.port).map_err(|e| format!("port {}: {e}", config.port))?;
    let token = if new_token { token::replace(&dir) } else { token::load_or_create(&dir) }
        .map_err(|e| format!("pairing token in {}: {e}", dir.display()))?;
    let mut origins: Vec<String> = DEFAULT_ORIGINS.iter().map(|o| o.to_string()).collect();
    origins.extend(config.origins);
    databases.extend(config.databases);
    let app = App {
        version: env!("CARGO_PKG_VERSION"),
        policy: Policy { port: config.port, origins, token: token.clone() },
        catalog: Catalog::new(databases),
        between_reads: None,
    };
    for l in &listeners {
        if let Ok(addr) = l.local_addr() {
            println!("oschess bridge {} on http://{addr}", app.version);
        }
    }
    for e in app.catalog.entries() {
        println!("  {} [{}] {}", e.id, e.state().name(), e.name);
    }
    if show_token {
        println!("pairing link: https://oschess.org/library?source=chessbase#cb-bridge={token}&port={}", config.port);
    }
    server::serve(listeners, Arc::new(app)).map_err(|e| e.to_string())
}
