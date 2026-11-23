//! Command-line flags. Normal use has none; the flags exist for development
//! (demo mode) and, with the `screenshot` cargo feature, for capturing the
//! window to a PNG file.

#[cfg(feature = "screenshot")]
use std::path::PathBuf;

/// A pretend computer, so the window can be developed and checked on a
/// machine without detectable sticks or faulty memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demo {
    /// Four slots, two 16 GB sticks; the test passes.
    Pass,
    /// Four slots, two 16 GB sticks; the test finds a (simulated) fault, so
    /// both sticks end up suspect.
    Fail,
    /// One 16 GB stick in four slots; the simulated fault fails that stick.
    SingleFail,
    /// Eight slots, six filled, with long part numbers, for layout checking.
    Eight,
    /// Stick detection failed.
    NoSticks,
}

impl Demo {
    /// Every demo, with the name used on the command line.
    pub const ALL: [(Demo, &'static str); 5] = [
        (Demo::Pass, "pass"),
        (Demo::Fail, "fail"),
        (Demo::SingleFail, "single-fail"),
        (Demo::Eight, "eight"),
        (Demo::NoSticks, "nosticks"),
    ];

    fn from_name(name: &str) -> Option<Demo> {
        Demo::ALL
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(demo, _)| *demo)
    }
}

/// When a screenshot is taken.
#[cfg(feature = "screenshot")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureWhen {
    /// Just after the window has opened.
    Idle,
    /// Partway through a run, which is started automatically.
    Running,
    /// After a run, started automatically, has finished.
    Done,
}

/// A request to save a PNG of the window and then exit.
#[cfg(feature = "screenshot")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    /// Where to write the PNG.
    pub path: PathBuf,
    /// Which moment to capture.
    pub when: CaptureWhen,
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    /// Run against a pretend computer instead of the real one.
    pub demo: Option<Demo>,
    /// Capture the window and exit.
    #[cfg(feature = "screenshot")]
    pub screenshot: Option<Screenshot>,
}

/// Why the command line was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// `--help` or `-h`: show the usage text and exit successfully.
    Help,
    /// Something was wrong; the message says what.
    Bad(String),
}

/// Parses the arguments that follow the program name.
pub fn parse(args: &[String]) -> Result<Options, CliError> {
    let mut options = Options::default();
    #[cfg(feature = "screenshot")]
    let mut shot_path: Option<PathBuf> = None;
    #[cfg(feature = "screenshot")]
    let mut shot_when: Option<CaptureWhen> = None;

    for arg in args {
        if arg == "--help" || arg == "-h" {
            return Err(CliError::Help);
        }
        let (flag, value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg.as_str(), None),
        };
        match flag {
            "--demo" => {
                let value = value.ok_or_else(|| bad("--demo needs a value, e.g. --demo=pass"))?;
                let demo = Demo::from_name(value)
                    .ok_or_else(|| bad(&format!("unknown demo \"{value}\"")))?;
                if options.demo.replace(demo).is_some() {
                    return Err(bad("--demo given more than once"));
                }
            }
            #[cfg(feature = "screenshot")]
            "--screenshot" => {
                let value = value.filter(|v| !v.is_empty()).ok_or_else(|| {
                    bad("--screenshot needs a file name, e.g. --screenshot=a.png")
                })?;
                if shot_path.replace(PathBuf::from(value)).is_some() {
                    return Err(bad("--screenshot given more than once"));
                }
            }
            #[cfg(feature = "screenshot")]
            "--screenshot-when" => {
                let when = match value {
                    Some("idle") => CaptureWhen::Idle,
                    Some("running") => CaptureWhen::Running,
                    Some("done") => CaptureWhen::Done,
                    _ => return Err(bad("--screenshot-when must be idle, running or done")),
                };
                if shot_when.replace(when).is_some() {
                    return Err(bad("--screenshot-when given more than once"));
                }
            }
            _ => return Err(bad(&format!("unknown argument \"{arg}\""))),
        }
    }

    #[cfg(feature = "screenshot")]
    {
        options.screenshot = match (shot_path, shot_when) {
            (Some(path), when) => Some(Screenshot {
                path,
                when: when.unwrap_or(CaptureWhen::Idle),
            }),
            (None, Some(_)) => return Err(bad("--screenshot-when needs --screenshot")),
            (None, None) => None,
        };
    }
    Ok(options)
}

fn bad(message: &str) -> CliError {
    CliError::Bad(message.to_string())
}

/// The short usage text shown for `--help` and for bad arguments.
pub fn usage() -> String {
    let demos: Vec<&str> = Demo::ALL.iter().map(|(_, name)| *name).collect();
    let mut text = format!(
        "Usage: memory-tester [--demo=<{}>]\n\
         \n\
         Run without arguments to test this computer's memory.\n\
         --demo=<name>  show a pretend computer (for development); results are not saved\n",
        demos.join("|")
    );
    if cfg!(feature = "screenshot") {
        text.push_str(
            "--screenshot=<file.png>                  save a picture of the window, then exit\n\
             --screenshot-when=<idle|running|done>    which moment to capture (default idle)\n",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parse_ok(list: &[&str]) -> Options {
        parse(&args(list)).unwrap_or_else(|e| panic!("{list:?}: {e:?}"))
    }

    fn parse_bad(list: &[&str]) -> String {
        match parse(&args(list)) {
            Err(CliError::Bad(msg)) => msg,
            other => panic!("{list:?} gave {other:?}"),
        }
    }

    #[test]
    fn no_arguments_is_the_real_computer() {
        let options = parse_ok(&[]);
        assert_eq!(options.demo, None);
        assert_eq!(options, Options::default());
    }

    #[test]
    fn every_demo_name_parses() {
        assert_eq!(parse_ok(&["--demo=pass"]).demo, Some(Demo::Pass));
        assert_eq!(parse_ok(&["--demo=fail"]).demo, Some(Demo::Fail));
        assert_eq!(
            parse_ok(&["--demo=single-fail"]).demo,
            Some(Demo::SingleFail)
        );
        assert_eq!(parse_ok(&["--demo=eight"]).demo, Some(Demo::Eight));
        assert_eq!(parse_ok(&["--demo=nosticks"]).demo, Some(Demo::NoSticks));
    }

    #[test]
    fn bad_demo_arguments_are_rejected() {
        assert!(parse_bad(&["--demo"]).contains("needs a value"));
        assert!(parse_bad(&["--demo="]).contains("unknown demo"));
        assert!(parse_bad(&["--demo=PASS"]).contains("unknown demo"));
        assert!(parse_bad(&["--demo=pass", "--demo=fail"]).contains("more than once"));
        // A space instead of '=' leaves a stray argument.
        assert!(parse_bad(&["--demo", "pass"]).contains("needs a value"));
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_bad(&["--fast"]).contains("unknown argument \"--fast\""));
        assert!(parse_bad(&["pass"]).contains("unknown argument"));
        assert!(parse_bad(&["--demo=pass", "-x"]).contains("unknown argument"));
        assert!(parse_bad(&[""]).contains("unknown argument"));
    }

    #[test]
    fn help_is_not_an_error_message() {
        assert_eq!(parse(&args(&["--help"])), Err(CliError::Help));
        assert_eq!(parse(&args(&["--demo=pass", "-h"])), Err(CliError::Help));
    }

    #[test]
    fn usage_lists_every_demo() {
        let text = usage();
        for (_, name) in Demo::ALL {
            assert!(text.contains(name), "{text}");
        }
        assert_eq!(text.contains("--screenshot"), cfg!(feature = "screenshot"));
    }

    #[cfg(not(feature = "screenshot"))]
    #[test]
    fn screenshot_flags_do_not_exist_without_the_feature() {
        assert!(parse_bad(&["--screenshot=a.png"]).contains("unknown argument"));
        assert!(parse_bad(&["--screenshot-when=idle"]).contains("unknown argument"));
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn screenshot_flags_parse() {
        let options = parse_ok(&["--demo=eight", "--screenshot=out/x.png"]);
        assert_eq!(
            options.screenshot,
            Some(Screenshot {
                path: PathBuf::from("out/x.png"),
                when: CaptureWhen::Idle
            })
        );
        assert_eq!(options.demo, Some(Demo::Eight));

        let when = |w: &str| {
            parse_ok(&["--screenshot=a.png", &format!("--screenshot-when={w}")])
                .screenshot
                .unwrap()
                .when
        };
        assert_eq!(when("idle"), CaptureWhen::Idle);
        assert_eq!(when("running"), CaptureWhen::Running);
        assert_eq!(when("done"), CaptureWhen::Done);
    }

    #[cfg(feature = "screenshot")]
    #[test]
    fn bad_screenshot_flags_are_rejected() {
        assert!(parse_bad(&["--screenshot"]).contains("needs a file name"));
        assert!(parse_bad(&["--screenshot="]).contains("needs a file name"));
        assert!(parse_bad(&["--screenshot-when=done"]).contains("needs --screenshot"));
        assert!(
            parse_bad(&["--screenshot=a.png", "--screenshot-when=later"])
                .contains("idle, running or done")
        );
        assert!(
            parse_bad(&["--screenshot=a.png", "--screenshot=b.png"]).contains("more than once")
        );
    }
}
