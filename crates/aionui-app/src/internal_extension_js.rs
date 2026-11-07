//! Private one-shot helper, deliberately outside clap and normal startup.
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    NotRequested,
    Run,
    Invalid,
}

fn invocation(mut args: impl Iterator<Item = OsString>) -> Invocation {
    if args.next().as_deref() != Some(OsStr::new("--internal-extension-js")) {
        return Invocation::NotRequested;
    }
    if args.next().is_some() {
        Invocation::Invalid
    } else {
        Invocation::Run
    }
}

fn reject_to(mut output: impl Write, error: &str) -> ExitCode {
    let response = serde_json::json!({"ok": false, "error": error});
    if writeln!(output, "{response}").and_then(|()| output.flush()).is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::from(2)
}

fn reject(error: &str) -> ExitCode {
    reject_to(io::stdout().lock(), error)
}

pub(super) fn try_run(args: impl Iterator<Item = OsString>) -> Option<ExitCode> {
    match invocation(args) {
        Invocation::NotRequested => None,
        Invocation::Invalid => Some(reject("invalid_helper_arguments")),
        Invocation::Run => {
            #[cfg(feature = "experimental-extension-js")]
            {
                match aionui_app::extension_js_client::prepare_child_parent_guard() {
                    Ok(()) => Some(aionui_extension_js::run_stdio()),
                    Err(error) => Some(reject(error)),
                }
            }
            #[cfg(not(feature = "experimental-extension-js"))]
            {
                Some(reject("runner_unavailable"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Invocation {
        invocation(args.iter().map(OsString::from))
    }
    #[test]
    fn accepts_only_exact_private_invocation() {
        assert_eq!(parse(&["--internal-extension-js"]), Invocation::Run);
        assert_eq!(parse(&["--internal-extension-js", "extra"]), Invocation::Invalid);
        assert_eq!(
            parse(&["--internal-extension-js", "--data-dir", "/tmp/no"]),
            Invocation::Invalid
        );
    }
    #[test]
    fn failed_protocol_write_is_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(reject_to(Broken, "runner_unavailable"), ExitCode::FAILURE);
    }
    #[test]
    fn preserves_normal_clap_path() {
        assert_eq!(parse(&[]), Invocation::NotRequested);
        assert_eq!(parse(&["--help"]), Invocation::NotRequested);
        assert_eq!(
            parse(&["--headless", "--internal-extension-js"]),
            Invocation::NotRequested
        );
    }
}
