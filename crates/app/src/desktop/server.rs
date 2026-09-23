//! The bridge server inside the app, and the state the tray, the windows and
//! the status thread share.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bridge::config::{self, DEFAULT_PORT};
use bridge::snapshot::Background;
use bridge::start::{self, Options};
use bridge::{pairing, server, token};

use crate::i18n::Strings;
use crate::status::{Problem, View};

/// How long a start waits for its port: after a restart the process before
/// may still hold it for a moment.
const PORT_WAIT: Duration = Duration::from_secs(5);

pub struct Shared {
    pub strings: Strings,
    /// The data folder, holding `bridge.toml`, the token and `app.json`;
    /// `None` when there is none.
    pub dir: Option<PathBuf>,
    running: Option<Running>,
    view: Mutex<View>,
    pub flyout: Mutex<Flyout>,
}

struct Running {
    background: Background,
    link: String,
    token: String,
}

/// Where the flyout opens and when it last closed.
#[derive(Default)]
pub struct Flyout {
    /// The tray icon's rectangle at the last click, in physical pixels:
    /// x, y, width, height.
    pub anchor: Option<(f64, f64, f64, f64)>,
    /// When the flyout last hid because it lost the focus. A click on the
    /// tray mark takes the focus first, so that click must not reopen it.
    pub hidden_at: Option<Instant>,
}

/// A started bridge, and what the start should show.
pub struct Started {
    pub shared: Shared,
    /// No token existed: the first-run window opens.
    pub first_run: bool,
    /// Browsers must pair again (the first run, a new code or a new port): the
    /// pairing link opens, which pairs the default browser at once.
    pub pair: bool,
}

/// The file whose presence at a start asks for the pairing link: the start
/// after a new code or a new port.
const PAIR_ON_START: &str = "pair-on-start";

/// Asks the next start to open the pairing link.
pub fn pair_on_next_start(dir: &std::path::Path) -> Result<(), String> {
    std::fs::write(dir.join(PAIR_ON_START), b"").map_err(|e| e.to_string())
}

/// The pairing code and the link that carries it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pairing {
    pub code: String,
    pub link: String,
}

impl Shared {
    /// Starts the bridge.
    pub fn start(strings: Strings) -> Started {
        let dir = start::data_dir().ok();
        let (running, view, first_run) = match &dir {
            Some(dir) => serve(dir),
            None => (None, failed(DEFAULT_PORT, "no data folder: %APPDATA% is not set".into()), false),
        };
        // Asked for by the start before, and done once the bridge serves.
        let asked =
            running.is_some() && dir.as_ref().is_some_and(|d| std::fs::remove_file(d.join(PAIR_ON_START)).is_ok());
        let shared = Shared { strings, dir, running, view: Mutex::new(view), flyout: Mutex::default() };
        Started { shared, first_run, pair: first_run || asked }
    }

    /// The state now: the serving bridge's snapshot, or why it does not serve.
    pub fn current_view(&self) -> View {
        match &self.running {
            Some(r) => View::of(&r.background.snapshot()),
            None => self.view(),
        }
    }

    /// The state the status thread saw last.
    pub fn view(&self) -> View {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_view(&self, view: View) {
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = view;
    }

    /// The pairing code and link: the serving bridge's, else the stored ones.
    pub fn pairing(&self) -> Result<Pairing, String> {
        if let Some(r) = &self.running {
            return Ok(Pairing { code: r.token.clone(), link: r.link.clone() });
        }
        let dir = self.dir()?;
        let config = config::load_or_create(&dir.join("bridge.toml"))?;
        let code = token::load_or_create(&dir).map_err(|e| e.to_string())?;
        let link = pairing::link(&config.web, &code, config.port);
        Ok(Pairing { code, link })
    }

    /// The oschess page the bridge's section is on, without the pairing
    /// fragment: a browser that paired once needs no token again.
    pub fn section_url(&self) -> String {
        let config = self.config_path().and_then(|path| config::load_or_create(&path));
        pairing::section(config.as_ref().map_or(pairing::DEFAULT_WEB, |c| c.web.as_str()))
    }

    pub fn dir(&self) -> Result<PathBuf, String> {
        self.dir.clone().ok_or_else(|| "no data folder".to_string())
    }

    pub fn config_path(&self) -> Result<PathBuf, String> {
        Ok(self.dir()?.join("bridge.toml"))
    }
}

fn failed(port: u16, reason: String) -> View {
    View::failed(env!("CARGO_PKG_VERSION"), port, Problem::Stopped { reason })
}

/// Starts serving from `dir`, waiting up to [`PORT_WAIT`] for a port that is
/// taken.
fn serve(dir: &std::path::Path) -> (Option<Running>, View, bool) {
    let port = match config::load_or_create(&dir.join("bridge.toml")) {
        Ok(config) => config.port,
        Err(e) => return (None, failed(DEFAULT_PORT, e), false),
    };
    let deadline = Instant::now() + PORT_WAIT;
    let bridge = loop {
        match start::prepare(dir, &Options::default()) {
            Ok(bridge) => break bridge,
            Err(e) => {
                let busy = port_busy(port);
                if busy && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                let view = if busy {
                    View::failed(env!("CARGO_PKG_VERSION"), port, Problem::PortBusy { port })
                } else {
                    failed(port, e)
                };
                return (None, view, false);
            }
        }
    };
    let (first_run, link, token) = (bridge.first_run, bridge.link.clone(), bridge.token.clone());
    match Background::serve(bridge) {
        Ok(background) => {
            let running = Running { background, link, token };
            let view = View::of(&running.background.snapshot());
            (Some(running), view, first_run)
        }
        Err(e) => (None, failed(port, format!("no server thread: {e}")), false),
    }
}

fn port_busy(port: u16) -> bool {
    matches!(server::bind(port), Err(e) if e.kind() == std::io::ErrorKind::AddrInUse)
}
