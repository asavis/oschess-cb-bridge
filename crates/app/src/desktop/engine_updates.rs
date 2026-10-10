//! Updates of the Stockfish the bridge installed (#322): a look a minute after
//! the start and every six hours while «Update Stockfish automatically» is on,
//! and one when the user turns it on. What a look does is
//! `stockfish_updates::Look`; this runs it on a thread and says what it did.

use std::sync::{Mutex, TryLockError};
use std::time::Duration;

use bridge::{engine, stockfish};
use tauri::{AppHandle, Emitter};

use super::commands::{CHOICES, KNOWN};
use super::notices::notify;
use super::{shared, updater};
use crate::prefs;
use crate::stockfish_updates::{Look, Outcome};

const FIRST_LOOK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// One look at a time: one asked for while another runs is that one.
static LOOKING: Mutex<()> = Mutex::new(());

/// Starts the automatic looks.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("stockfish-updates".into()).spawn(move || {
        std::thread::sleep(FIRST_LOOK);
        loop {
            look(&app);
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        bridge::log!("no Stockfish update thread: {e}");
    }
}

/// Looks at once, on a thread of its own: the commands run on the event
/// loop's thread.
pub fn look_now(app: &AppHandle) {
    let app = app.clone();
    if let Err(e) = std::thread::Builder::new().name("stockfish-update-now".into()).spawn(move || look(&app)) {
        bridge::log!("Stockfish update: no worker thread: {e}");
    }
}

/// Looks while the option is on. A new build chosen is announced, and the
/// windows read the engines again.
fn look(app: &AppHandle) {
    let _one = match LOOKING.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    let shared = shared(app);
    let (Ok(data), Ok(config_path)) = (shared.dir(), shared.config_path()) else { return };
    if !prefs::load(&data).stockfish_auto_update {
        return;
    }
    let look = Look {
        data: &data,
        config_path: &config_path,
        choices: &CHOICES,
        known: &KNOWN,
        transport: &stockfish::System,
        arch: stockfish::machine_arch(),
    };
    match look.run(|| updater::wait_idle(&shared), |exe| engine::probe(exe).map(drop)) {
        Ok(Outcome::Updated(version)) => {
            bridge::log!("Stockfish update: Stockfish {version} installed and chosen");
            let strings = &shared.strings;
            notify(
                app,
                strings.fill("toast.stockfishUpdated.title", &[("version", &version)]),
                strings.get("toast.stockfishUpdated.body"),
            );
            let _ = app.emit("engines-changed", ());
        }
        Ok(Outcome::Kept(version)) => {
            bridge::log!("Stockfish update: Stockfish {version} installed; another engine was chosen meanwhile");
            let _ = app.emit("engines-changed", ());
        }
        Ok(Outcome::Current | Outcome::NotOurs) => {}
        Err(e) => bridge::log!("Stockfish update: {e}"),
    }
}
