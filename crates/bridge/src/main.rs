//! `oschess-bridge`: the bridge in a console, the same one `cbtool bridge`
//! runs. On Windows one bridge runs per signed-in user: a later start asks the
//! running one to open oschess in the browser and exits, and the first run
//! opens the pairing link. The tray application of #23 will take the console's
//! place once its design is approved.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oschess-bridge: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn run() -> Result<(), String> {
    bridge::start::console("oschess-bridge", std::env::args().skip(1))
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use bridge::instance::{self, Session, Start};
    use bridge::{browser, start};

    let options = start::Options::parse(std::env::args().skip(1)).map_err(|e| start::usage("oschess-bridge", &e))?;
    let mut session = Session::default();
    match instance::start(&mut session) {
        Start::First => {}
        Start::Signalled => {
            println!("The oschess bridge is already running; it opens oschess in the browser.");
            return Ok(());
        }
        Start::Unanswered => return Err("another oschess bridge is running but does not answer".into()),
    }
    let bridge = start::prepare(&start::data_dir()?, &options)?;
    let link = bridge.link.clone();
    session.listen(move || {
        browser::open(&link);
    })?;
    if bridge.first_run && !browser::open(&bridge.link) {
        eprintln!("oschess-bridge: could not open the browser; --show-token prints the pairing link");
    }
    // `session` holds the lock until the server ends with the process.
    start::serve_console(bridge, options.show_token)
}
