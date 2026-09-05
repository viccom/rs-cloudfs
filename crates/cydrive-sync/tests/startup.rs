//! Startup decision specs: the pure function that turns the argument
//! vector (after `argv[0]`) into run / print-version / print-help /
//! refuse, pinning the exact accepted shapes so a mistyped or stray
//! argument can never silently start the service.

use cydrive_sync::startup::{decide_startup, StartupDecision};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn no_arguments_runs_the_server() {
    assert_eq!(decide_startup(&args(&[])), StartupDecision::Run);
}

#[test]
fn version_flags_print_version() {
    for flag in ["--version", "-V"] {
        assert_eq!(
            decide_startup(&args(&[flag])),
            StartupDecision::PrintVersion,
            "{flag} alone should print the version"
        );
    }
}

#[test]
fn help_flags_print_help() {
    for flag in ["--help", "-h"] {
        assert_eq!(
            decide_startup(&args(&[flag])),
            StartupDecision::PrintHelp,
            "{flag} alone should print help"
        );
    }
}

#[test]
fn unknown_single_argument_is_invalid_naming_it_with_a_usage_hint() {
    for bad in ["--bogus", "-x", "extra"] {
        let StartupDecision::Invalid(message) = decide_startup(&args(&[bad])) else {
            panic!("unknown argument {bad} must be rejected, not ignored");
        };
        assert!(
            message.contains(bad),
            "message should quote the offending argument {bad}: {message}"
        );
        assert!(
            message.contains("--help"),
            "message should point at --help for usage: {message}"
        );
    }
}

#[test]
fn multiple_arguments_are_invalid_even_when_each_is_a_known_flag() {
    for list in [
        vec!["--version", "--help"],
        vec!["--help", "extra"],
        vec!["--version", "sync.db"],
        vec!["a", "b"],
    ] {
        assert!(
            matches!(decide_startup(&args(&list)), StartupDecision::Invalid(_)),
            "{list:?} must be rejected: only the exact no-argument / one-flag shapes are valid"
        );
    }
}
