use crate::error::{CfmpegError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Auth(AuthAction),
    Codecs,
    Config(Option<ConfigAction>),
    Encode {
        ffmpeg_args: Vec<String>,
        force_local: bool,
    },
    Help,
    Usage,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthAction {
    Login,
    Logout,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigAction {
    Edit,
    Path,
    Show,
}

pub fn parse_args(raw_args: Vec<String>) -> Result<Command> {
    if raw_args.is_empty() {
        return Ok(Command::Help);
    }

    match raw_args[0].as_str() {
        "auth" => parse_auth(&raw_args[1..]),
        "config" => parse_config(&raw_args[1..]),
        "help" => Ok(Command::Help),
        "usage" => parse_exact(raw_args, Command::Usage),
        "version" => parse_exact(raw_args, Command::Version),
        _ => parse_passthrough(raw_args),
    }
}

pub fn print_help() {
    println!("cfmpeg");
    println!();
    println!("Usage:");
    println!("  cfmpeg [--local] <ffmpeg args...>");
    println!("  cfmpeg auth <login|status|logout>");
    println!("  cfmpeg config [path|show|edit]");
    println!("  cfmpeg usage");
    println!("  cfmpeg --codecs");
    println!("  cfmpeg --version");
    println!("  cfmpeg help");
    println!();
    println!("Notes:");
    println!("  - Arguments that look like ffmpeg flags are passed through directly.");
    println!("  - Use `--local` to force local ffmpeg execution.");
    println!("  - Use `cfmpeg help` for CLI help because `-h` is treated as an ffmpeg flag.");
}

fn parse_auth(args: &[String]) -> Result<Command> {
    match args {
        [] => Ok(Command::Auth(AuthAction::Status)),
        [action] => match action.as_str() {
            "login" => Ok(Command::Auth(AuthAction::Login)),
            "logout" => Ok(Command::Auth(AuthAction::Logout)),
            "status" => Ok(Command::Auth(AuthAction::Status)),
            _ => Err(CfmpegError::ParseError(format!(
                "unknown auth action: {action}"
            ))),
        },
        _ => Err(CfmpegError::ParseError(
            "auth accepts exactly one action".to_string(),
        )),
    }
}

fn parse_config(args: &[String]) -> Result<Command> {
    match args {
        [] => Ok(Command::Config(None)),
        [action] => match action.as_str() {
            "edit" => Ok(Command::Config(Some(ConfigAction::Edit))),
            "path" => Ok(Command::Config(Some(ConfigAction::Path))),
            "show" => Ok(Command::Config(Some(ConfigAction::Show))),
            _ => Err(CfmpegError::ParseError(format!(
                "unknown config action: {action}"
            ))),
        },
        _ => Err(CfmpegError::ParseError(
            "config accepts at most one action".to_string(),
        )),
    }
}

fn parse_exact(raw_args: Vec<String>, command: Command) -> Result<Command> {
    if raw_args.len() == 1 {
        Ok(command)
    } else {
        Err(CfmpegError::ParseError(
            "command does not accept additional arguments".to_string(),
        ))
    }
}

fn parse_passthrough(raw_args: Vec<String>) -> Result<Command> {
    let mut force_local = false;
    let mut show_codecs = false;
    let mut show_version = false;
    let mut ffmpeg_args = Vec::new();

    for arg in raw_args {
        match arg.as_str() {
            "--local" => force_local = true,
            "--codecs" => show_codecs = true,
            "--version" => show_version = true,
            _ => ffmpeg_args.push(arg),
        }
    }

    if show_codecs {
        if force_local || show_version || !ffmpeg_args.is_empty() {
            return Err(CfmpegError::ParseError(
                "--codecs must be used on its own".to_string(),
            ));
        }

        return Ok(Command::Codecs);
    }

    if show_version {
        if force_local || !ffmpeg_args.is_empty() {
            return Err(CfmpegError::ParseError(
                "--version must be used on its own".to_string(),
            ));
        }

        return Ok(Command::Version);
    }

    if ffmpeg_args.is_empty() {
        return Ok(Command::Help);
    }

    Ok(Command::Encode {
        ffmpeg_args,
        force_local,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_args, AuthAction, Command, ConfigAction};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn parses_ffmpeg_passthrough_with_local_flag() {
        let command =
            parse_args(args(&["--local", "-i", "input.mov", "output.mp4"])).expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["-i", "input.mov", "output.mp4"]),
                force_local: true,
            }
        );
    }

    #[test]
    fn parses_auth_subcommand() {
        let command = parse_args(args(&["auth", "logout"])).expect("command");

        assert_eq!(command, Command::Auth(AuthAction::Logout));
    }

    #[test]
    fn parses_config_subcommand() {
        let command = parse_args(args(&["config", "show"])).expect("command");

        assert_eq!(command, Command::Config(Some(ConfigAction::Show)));
    }

    #[test]
    fn rejects_mixed_codecs_and_ffmpeg_args() {
        let error = parse_args(args(&["--codecs", "-i", "input.mov"])).expect_err("error");

        assert!(error.to_string().contains("--codecs"));
    }
}
