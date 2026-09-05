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
/// `argv[0]` (i.e. `std::env::args_os().skip(1)` lossy-decoded to
/// `String`s — a non-Unicode argument arrives with U+FFFD replacement
/// characters and is refused, never a panic): exactly no arguments runs
/// the server, exactly one known flag prints and exits, every other
/// shape is rejected so no stray argument can ever start a service.
pub fn decide_startup(args: &[String]) -> StartupDecision {
    match args {
        [] => StartupDecision::Run,
        [only] if matches!(only.as_str(), "--version" | "-V") => StartupDecision::PrintVersion,
        [only] if matches!(only.as_str(), "--help" | "-h") => StartupDecision::PrintHelp,
        _ => StartupDecision::Invalid(format!(
            "unexpected argument(s): {}. This server takes no command-line \
             arguments; run it with none to start, or pass --help for usage.",
            args.join(" ")
        )),
    }
}
