//! Command-line arguments (D51, D53).

use std::ffi::OsString;

use crate::error::Error;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print the help text.
    Help,
    /// Run the agent.
    Run(Args),
}

/// Options of a run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// `-p <message>`: answer one message and exit; REPL otherwise.
    pub prompt: Option<String>,
    /// `--resume <id>`.
    pub resume: Option<String>,
    /// `--function <name>`.
    pub function: Option<String>,
}

/// Parses the arguments after the program name. Each option takes the
/// next argument as its value, even one that starts with `-`.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, Error> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|_| Error::InvalidUnicode)?;
        let slot = match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-p" => &mut parsed.prompt,
            "--resume" => &mut parsed.resume,
            "--function" => &mut parsed.function,
            _ => return Err(Error::UnknownOption(arg)),
        };
        if slot.is_some() {
            return Err(Error::RepeatedOption(arg));
        }
        let value = args
            .next()
            .ok_or_else(|| Error::MissingValue(arg.clone()))?;
        *slot = Some(value.into_string().map_err(|_| Error::InvalidUnicode)?);
    }
    Ok(Command::Run(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, Error> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_start_the_repl() {
        assert_eq!(parse_strs(&[]).unwrap(), Command::Run(Args::default()));
    }

    #[test]
    fn all_options() {
        let command = parse_strs(&["--function", "coder", "-p", "hi there", "--resume", "id"]);
        assert_eq!(
            command.unwrap(),
            Command::Run(Args {
                prompt: Some("hi there".into()),
                resume: Some("id".into()),
                function: Some("coder".into()),
            })
        );
    }

    #[test]
    fn values_may_start_with_a_dash() {
        let command = parse_strs(&["-p", "--help"]).unwrap();
        let Command::Run(args) = command else {
            panic!("expected a run");
        };
        assert_eq!(args.prompt.as_deref(), Some("--help"));
    }

    #[test]
    fn help() {
        assert_eq!(parse_strs(&["-h"]).unwrap(), Command::Help);
        assert_eq!(parse_strs(&["-p", "x", "--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            parse_strs(&["--url", "x"]),
            Err(Error::UnknownOption(o)) if o == "--url"
        ));
        assert!(matches!(
            parse_strs(&["hello"]),
            Err(Error::UnknownOption(o)) if o == "hello"
        ));
        assert!(matches!(
            parse_strs(&["-p"]),
            Err(Error::MissingValue(o)) if o == "-p"
        ));
        assert!(matches!(
            parse_strs(&["-p", "a", "-p", "b"]),
            Err(Error::RepeatedOption(o)) if o == "-p"
        ));
    }

    #[test]
    fn non_utf8_is_an_error() {
        use std::os::unix::ffi::OsStringExt;
        let arg = OsString::from_vec(vec![0xff]);
        assert!(matches!(
            parse([OsString::from("-p"), arg]),
            Err(Error::InvalidUnicode)
        ));
    }
}
