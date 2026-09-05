//! Startup argument handling for the `cydrive-sync-server` bin.
//!
//! The server takes no runtime flags and no positional arguments — all
//! configuration arrives via `SYNC_*` environment variables. Historically
//! the bin silently ignored anything on the command line and started
//! serving, which turned deployment probes like `--help` into stray
//! daemon processes holding the port; this module is the pure core of
//! the fix, consumed by `main` before any setup runs.

/// What `main` should do after looking at the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupDecision {
    /// No arguments: start the server as usual.
    Run,
    /// `--version` or `-V`: print the version line and exit 0.
    PrintVersion,
    /// `--help` or `-h`: print the usage text and exit 0.
    PrintHelp,
    /// Anything else (unknown flags, extra arguments): refuse to start.
    /// The message names the offending arguments and points at `--help`.
    Invalid(String),
}

/// Decides what the bin should do. `args` is the argument vector *after*
/// `argv[0]` (i.e. `std::env::args().skip(1)`): exactly no arguments runs
/// the server, exactly one known flag prints and exits, every other
/// shape is rejected so no stray argument can ever start a service.
// TODO(green): drop the allow once the parameter is read by the real body.
#[allow(unused_variables)]
pub fn decide_startup(args: &[String]) -> StartupDecision {
    todo!()
}
