//! `knowa` CLI: argument parsing and rendering only (DESIGN §7). All behavior lives
//! in `knowa-api`. `--json` output is contract; text output is not.

mod cli;
mod parse;
mod render;
mod run;

use std::ffi::OsString;
use std::io::Write;

use clap::Parser;
use knowa_model::{Error, ErrorCode};

fn main() {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let cli = match cli::Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(e) => std::process::exit(clap_failure(&e, wants_json(&argv))),
    };
    let json = cli.json;
    let code = match run::run(cli) {
        Ok(()) => 0,
        Err(e) => {
            report(&e, json);
            e.code.exit_code()
        }
    };
    std::process::exit(code);
}

/// The error contract (DESIGN §6.3) on stderr.
fn report(e: &Error, json: bool) {
    let text = if json {
        e.to_json().to_string()
    } else {
        let mut t = format!("error[{}]: {}", e.code.as_str(), e.message);
        if let Some(d) = &e.detail {
            t.push_str(&format!("\n  detail: {d}"));
        }
        t
    };
    // Fallback: stderr itself is gone (closed pipe). There is no channel left to report
    // on; the exit code still carries the error, which is what callers branch on.
    let _ = writeln!(std::io::stderr(), "{text}");
}

/// clap's own outcome: help/version → stdout, exit 0. Anything else is a usage error
/// (exit 2); with `--json` it uses the JSON envelope with code `usage`.
fn clap_failure(e: &clap::Error, json: bool) -> i32 {
    if e.exit_code() == 0 || !json {
        // Fallback: as in `report`, a closed stdout/stderr leaves nowhere to write;
        // the exit code still says what happened.
        let _ = e.print();
        return if e.exit_code() == 0 { 0 } else { ErrorCode::Usage.exit_code() };
    }
    report(&Error::usage(usage_message(&e.render().to_string())), true);
    ErrorCode::Usage.exit_code()
}

/// clap's rendered error up to its first blank line (the usage block follows), on one
/// line and without the `error: ` prefix.
fn usage_message(rendered: &str) -> String {
    let head: Vec<&str> =
        rendered.trim_start().lines().take_while(|l| !l.trim().is_empty()).map(str::trim).collect();
    let joined = head.join(" ");
    joined.strip_prefix("error: ").unwrap_or(&joined).to_string()
}

/// Whether `--json` was requested, for errors raised before parsing succeeded.
fn wants_json(argv: &[OsString]) -> bool {
    argv.iter().skip(1).take_while(|a| *a != "--").any(|a| a == "--json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn json_flag_is_found_anywhere_before_double_dash() {
        assert!(wants_json(&os(&["knowa", "bogus", "--json"])));
        assert!(wants_json(&os(&["knowa", "--json", "search"])));
        assert!(!wants_json(&os(&["knowa", "note", "--", "--json"])));
        assert!(!wants_json(&os(&["--json"])));
    }

    #[test]
    fn usage_message_is_the_first_paragraph_on_one_line() {
        let rendered = "error: unrecognized subcommand 'bogus'\n\nUsage: knowa [OPTIONS] <COMMAND>\n";
        assert_eq!(usage_message(rendered), "unrecognized subcommand 'bogus'");
        let rendered = "error: the following required arguments were not provided:\n  <TEXT>\n\nUsage: knowa search <TEXT>\n";
        assert_eq!(usage_message(rendered), "the following required arguments were not provided: <TEXT>");
    }

    #[test]
    fn clap_errors_exit_2() {
        let argv = os(&["knowa", "bogus"]);
        let e = cli::Cli::try_parse_from(&argv).unwrap_err();
        assert_eq!(e.exit_code(), ErrorCode::Usage.exit_code());
    }
}
