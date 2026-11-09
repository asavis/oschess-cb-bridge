//! The bridge server inside the app, and the state the tray, the windows and
//! the status thread share.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use bridge::config::{self, DEFAULT_PORT};
use bridge::pairing;
use bridge::snapshot::{Background, Pulse};
use bridge::start::{self, Cause, Options};

use crate::diagnostics::PageErrors;
use crate::i18n::Strings;
use crate::opening::{FlyoutLife, Openings};
use crate::status::{Asked, Problem, View};

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
    /// What asked the status thread for a look at the databases, and what
    /// wakes it to take one (#365).
    asked: Mutex<Asked>,
    wake: Condvar,
    pub flyout: Mutex<Flyout>,
    /// How far each window's latest opening got (#332).
    pub openings: Openings,
    /// The script errors each window logged in this run (#332).
    pub page_errors: PageErrors,
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
    /// Who wants it, which keeps it alive (#365).
    pub life: FlyoutLife,
}

/// A started bridge, and what the start should show.
pub struct Started {
    pub shared: Shared,
    /// No token existed: the first-run wizard opens, and it opens the pairing
    /// link on its last screen, not while the user is on its steps (#201).
    pub first_run: bool,
    /// Browsers must pair again after a new code or a new port: the pairing
    /// link opens, which pairs the default browser at once.
    pub pair: bool,
}

/// The file whose presence at a start asks for the pairing link: the start
/// after a new code or a new port.
const PAIR_ON_START: &str = "pair-on-start";

/// Asks the next start to open the pairing link.
pub fn pair_on_next_start(dir: &std::path::Path) -> Result<(), String> {
    std::fs::write(dir.join(PAIR_ON_START), b"").map_err(|e| {
        bridge::fault!("{PAIR_ON_START} cannot be written: {e}");
        e.to_string()
    })
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
        let shared = Shared {
            strings,
            dir,
            running,
            view: Mutex::new(view),
            asked: Mutex::default(),
            wake: Condvar::new(),
            flyout: Mutex::default(),
            openings: Openings::default(),
            page_errors: PageErrors::default(),
        };
        Started { shared, first_run, pair: asked }
    }

    /// What changes when the serving bridge's snapshot may have, without
    /// looking at any database (#348); none when it does not serve.
    pub fn pulse(&self) -> Option<Pulse> {
        self.running.as_ref().map(|r| r.background.pulse())
    }

    /// A user is looking through the app, which keeps the bridge in use.
    pub fn touch(&self) {
        if let Some(r) = &self.running {
            r.background.touch();
        }
    }

    /// Asks the status thread for a look at the databases, and wakes it.
    pub fn ask(&self, asked: Asked) {
        let mut waiting = self.asked.lock().unwrap_or_else(|e| e.into_inner());
        *waiting = (*waiting).max(asked);
        self.wake.notify_all();
    }

    /// Waits up to `limit` for a look to be asked for, and takes what asked.
    pub fn wait_asked(&self, limit: Duration) -> Asked {
        let waiting = self.asked.lock().unwrap_or_else(|e| e.into_inner());
        let (mut waiting, _) = self
            .wake
            .wait_timeout_while(waiting, limit, |asked| *asked == Asked::Nothing)
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *waiting)
    }

    /// The view an update decides by: the last one seen, with `busy` read
    /// from the bridge now, which looks at no database (#365).
    pub fn view_for_update(&self) -> View {
        let mut view = self.view();
        if let Some(r) = &self.running {
            view.busy = r.background.pulse().busy();
        }
        view
    }

    /// The names of the databases on the list, which a report must not
    /// carry: read from the list, whether or not anyone looked at them yet
    /// (#365).
    pub fn database_names(&self) -> Vec<String> {
        match &self.running {
            Some(r) => r.background.names(),
            None => self.view().databases.into_iter().map(|d| d.name).collect(),
        }
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

    /// The pairing code and link: the serving bridge's, else the stored ones
    /// under the rule of a start (#183): none for a site that is not an
    /// allowed origin, and none before a start created the code.
    pub fn pairing(&self) -> Result<Pairing, String> {
        if let Some(r) = &self.running {
            return Ok(Pairing { code: r.token.clone(), link: r.link.clone() });
        }
        let pairing = start::pairing(&self.dir()?)?;
        Ok(Pairing { code: pairing.token, link: pairing.link })
    }

    /// The oschess page the bridge's section is on, without the pairing
    /// fragment, for when the pairing cannot be read: on the site of
    /// `bridge.toml` when it is an allowed origin, else on oschess.org.
    pub fn section_url(&self) -> String {
        let site = self.dir().and_then(|dir| start::site(&dir));
        pairing::section(site.as_deref().unwrap_or(pairing::DEFAULT_WEB))
    }

    pub fn dir(&self) -> Result<PathBuf, String> {
        self.dir.clone().ok_or_else(|| "no data folder".to_string())
    }

    pub fn config_path(&self) -> Result<PathBuf, String> {
        Ok(self.dir()?.join(config::FILE_NAME))
    }
}

fn failed(port: u16, reason: String) -> View {
    View::failed(env!("CARGO_PKG_VERSION"), port, Problem::Stopped { reason })
}

/// Starts serving from `dir`, waiting up to [`PORT_WAIT`] for a port that is
/// taken.
fn serve(dir: &std::path::Path) -> (Option<Running>, View, bool) {
    // The status reports the app's version, the one its release carries, not
    // the bridge library's.
    let options = Options { version: Some(env!("CARGO_PKG_VERSION")), port_wait: PORT_WAIT, ..Options::default() };
    let bridge = match start::prepare(dir, &options) {
        Ok(bridge) => bridge,
        Err(e) => {
            let view = match e.cause {
                Cause::PortInUse(port) => View::failed(env!("CARGO_PKG_VERSION"), port, Problem::PortBusy { port }),
                Cause::Other { port } => failed(port.unwrap_or(DEFAULT_PORT), e.to_string()),
            };
            return (None, view, false);
        }
    };
    let (first_run, port, link, token) = (bridge.first_run, bridge.port, bridge.link.clone(), bridge.token.clone());
    match Background::serve(bridge) {
        Ok(background) => {
            let running = Running { background, link, token };
            // No database is touched at the start: the status thread looks
            // when someone uses the bridge (#365).
            let view = View::serving(env!("CARGO_PKG_VERSION"), port);
            (Some(running), view, first_run)
        }
        Err(e) => {
            bridge::fault!("the bridge cannot start: no server thread: {e}");
            (None, failed(port, format!("no server thread: {e}")), false)
        }
    }
}
