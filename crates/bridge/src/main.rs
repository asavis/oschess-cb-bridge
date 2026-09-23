//! `oschess-bridge`: runs the bridge in a console, as `cbtool bridge` does.
//! The Windows application of #23 will start the same bridge
//! ([`bridge::start`]).

use std::process::ExitCode;

fn main() -> ExitCode {
    match bridge::start::console("oschess-bridge", std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oschess-bridge: {e}");
            ExitCode::FAILURE
        }
    }
}
