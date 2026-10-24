//! Updates of the Stockfish the bridge installed (#322): a look a minute after
//! the start and every six hours while «Update Stockfish automatically» is on,
//! and one when the user turns it on. What a look does is
//! `stockfish_updates::Look`; this runs it on a thread and says what it did.
//! Each look, whether the option is on or not, also tells once per bridge
//! version of the engine section's offer of a newer Stockfish (#344).

use std::path::Path;
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, SystemTime};

use bridge::stockfish::Arch;
use bridge::{config, engine, stockfish};
use tauri::{AppHandle, Emitter};

use super::commands::{CHOICES, KNOWN, found_engines};
use super::notices::{notify, notify_opening};
use super::{shared, updater};
use crate::choices;
use crate::i18n::Strings;
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
        bridge::fault!("no Stockfish update thread: {e}");
    }
}

/// Looks at once, on a thread of its own: the commands run on the event
/// loop's thread.
pub fn look_now(app: &AppHandle) {
    let app = app.clone();
    if let Err(e) = std::thread::Builder::new().name("stockfish-update-now".into()).spawn(move || look(&app)) {
        bridge::fault!("Stockfish update: no worker thread: {e}");
    }
}

/// Looks while the option is on; an update waiting for the bridge to be idle
/// ends when it is turned off. A new build chosen is announced. The windows
/// read the engines again when a build was installed or a newer release
/// became known, also when nothing was replaced.
fn look(app: &AppHandle) {
    let _one = match LOOKING.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(e)) => e.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    let shared = shared(app);
    let (Ok(data), Ok(config_path)) = (shared.dir(), shared.config_path()) else { return };
    let arch = stockfish::machine_arch();
    let known_before = KNOWN.newest(arch);
    let look = Look {
        data: &data,
        config_path: &config_path,
        choices: &CHOICES,
        known: &KNOWN,
        transport: &stockfish::System,
        arch,
        now: SystemTime::now(),
    };
    let wanted = || prefs::load(&data).stockfish_auto_update;
    let outcome = look.run(|| updater::wait_idle_while(&shared, wanted), |exe| engine::probe(exe).map(drop));
    if KNOWN.newest(arch) != known_before || outcome.as_ref().is_ok_and(Outcome::installed) {
        let _ = app.emit("engines-changed", ());
    }
    match outcome {
        Ok(Outcome::Updated(version)) => {
            bridge::log!("Stockfish update: Stockfish {version} installed and chosen");
            let strings = &shared.strings;
            notify(
                app,
                strings.fill("toast.stockfishUpdated.title", &[("version", &version)]),
                strings.get("toast.stockfishUpdated.body"),
            );
        }
        Ok(Outcome::Kept(version)) => {
            bridge::log!("Stockfish update: Stockfish {version} installed; another engine was chosen meanwhile");
        }
        Ok(Outcome::Withdrawn(version)) => {
            bridge::log!(
                "Stockfish update: Stockfish {version} installed; the update was turned off before the switch"
            );
        }
        Ok(Outcome::Off | Outcome::Current | Outcome::NotOurs) => {}
        Err(e) => bridge::log!("Stockfish update: {e}"),
    }
    notice_offer(app, &shared.strings, &data, &config_path, arch);
}

/// Tells in a notice, once per bridge version, of the engine section's offer
/// of a newer Stockfish in place of the chosen one (#344); a click opens that
/// section, whose «Update» installs it beside the engines found.
fn notice_offer(app: &AppHandle, strings: &Strings, data: &Path, config_path: &Path, arch: Arch) {
    let Ok(config) = config::load_or_create(config_path) else { return };
    let newest = KNOWN.newest(arch);
    let found = found_engines(data);
    let running = env!("CARGO_PKG_VERSION");
    let Some(chosen) = choices::offer_notice(data, config.engine.as_deref(), &found, running, &newest) else { return };
    bridge::log!("Stockfish offer: Stockfish {} in place of {chosen}, told in a notice", newest.version);
    notify_opening(
        app,
        strings.fill("toast.stockfishOffer.title", &[("version", &newest.version)]),
        &strings.fill("toast.stockfishOffer.body", &[("current", &chosen)]),
        "engine",
    );
}
