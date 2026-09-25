//! The tray's state and tooltip from a real catalog: databases in
//! `bridge.toml` that are ready, damaged, gone, or taken off the list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use app::i18n::{Lang, Strings};
use app::status::{Tray, View};
use bridge::access::{DEFAULT_ORIGINS, Policy};
use bridge::api::App;
use bridge::catalog::{Catalog, State};
use bridge::config::{self, Config};
use bridge::fetch::System;
use bridge::server;
use bridge::snapshot::Background;
use bridge::sources::Sources;
use bridge::start::Bridge;
use cbformat::fixture::{Builder, TempDb, lid_header, quiet};
use cbformat::movetable::{Color, END_OF_LINE, MOVES, Piece};

/// A database of two games of 1.e4.
fn database(name: &str) -> TempDb {
    let mut b = Builder::new();
    let e4 = b.moves(1, &[MOVES, quiet(Color::White, Piece::Pawn, "e2", "e4"), END_OF_LINE]);
    b.game(e4);
    b.game(e4);
    let player = [&6i32.to_le_bytes()[..], b"Morphy"].concat();
    let mut lid = lid_header(1024, 1);
    lid.extend((player.len() as i32).to_le_bytes());
    lid.extend(&player);
    b.lid(lid);
    b.write(name)
}

/// Lists `paths` in `bridge.toml`, which the catalog reads again on change.
fn list(config: &Path, paths: &[&PathBuf]) {
    let settings = Config { databases: paths.iter().map(|p| p.to_path_buf()).collect(), ..Config::default() };
    config::save(config, &settings).unwrap();
}

#[test]
fn the_tray_follows_the_catalog() {
    let dir = std::env::temp_dir().join(format!("bridge-app-status-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let good = database("app-status-good");
    let damaged = database("app-status-damaged");
    // A header file that is not a whole number of records cannot be opened.
    std::fs::write(damaged.dir().join("db.2cbh"), vec![0u8; 100]).unwrap();
    let (good, damaged) = (good.dir().join("db.2cbh"), damaged.dir().join("db.2cbh"));
    let gone = dir.join("a database that is not there.2cbh");
    let config_path = dir.join("bridge.toml");
    list(&config_path, &[&good]);

    let sources = Sources { config: Some(config_path.clone()), ..Sources::default() };
    let listeners = server::bind(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let token = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ".to_string();
    let app = App {
        version: "test",
        policy: Policy { port, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: token.clone() },
        catalog: Catalog::with_sources(sources, Arc::new(System)),
        between_reads: None,
        engine: bridge::engine::Engine::none(),
    };
    let bridge = Bridge { listeners, app: Arc::new(app), port, token, link: String::new(), first_run: false };
    let background = Background::serve(bridge).unwrap();
    let uk = Strings::new(Lang::Uk);
    let now = || View::of(&background.snapshot());

    let view = now();
    assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (Tray::Ready, "oschess міст — 1 база готова"));
    assert_eq!(view.databases[0].records, Some(2));

    list(&config_path, &[&good, &damaged]);
    let view = now();
    let states: Vec<State> = view.databases.iter().map(|d| d.state).collect();
    assert_eq!(states, [State::Ready, State::Unreadable]);
    assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (Tray::Attention, "oschess міст — 1 база не відкривається"));
    assert_eq!(view.mark, "attention");

    list(&config_path, &[&good, &gone]);
    let view = now();
    let states: Vec<State> = view.databases.iter().map(|d| d.state).collect();
    assert_eq!(states, [State::Ready, State::Missing], "the damaged database left the list and is not shown");
    assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (Tray::Attention, "oschess міст — 1 базу не знайдено"));

    list(&config_path, &[&good]);
    let snapshot = background.snapshot();
    let listed: Vec<bool> = snapshot.databases.iter().map(|d| d.listed).collect();
    assert_eq!(listed, [true, false, false], "the bridge keeps the ones that left the list");
    let view = View::of(&snapshot);
    assert_eq!(view.databases.len(), 1);
    assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (Tray::Ready, "oschess міст — 1 база готова"));

    list(&config_path, &[]);
    let view = now();
    assert_eq!((view.tray(), view.tooltip(&uk).as_str()), (Tray::Ready, "oschess міст — баз поки немає"));
    let _ = std::fs::remove_dir_all(&dir);
}
