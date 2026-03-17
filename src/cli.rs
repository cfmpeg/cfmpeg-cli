use crate::error::{CfmpegError, Result};
use crate::remote::{
    parse_cpu_cores, parse_memory_mb, parse_profile, parse_timeout_seconds, RemoteExecutionOptions,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Auth(AuthAction),
    Codecs,
    Config(Option<ConfigAction>),
    Encode {
        ffmpeg_args: Vec<String>,
        force_local: bool,
        no_download: bool,
        remote: RemoteExecutionOptions,
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
    println!("  cfmpeg [--local] [--no-download] [--cf-profile <value>] [--cf-cpu <cores>] [--cf-memory <size>] [--cf-timeout <duration>] <ffmpeg args...>");
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
    println!("  - Use `--no-download` to leave completed outputs in cloud storage and print signed URLs instead.");
    println!("  - Use `--cf-*` flags to request remote execution resources without changing ffmpeg arguments.");
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
    let mut no_download = false;
    let mut show_codecs = false;
    let mut show_version = false;
    let mut ffmpeg_args = Vec::new();
    let mut remote = RemoteExecutionOptions::default();
    let mut index = 0usize;

    while index < raw_args.len() {
        let arg = &raw_args[index];
        match arg.as_str() {
            "--local" => force_local = true,
            "--no-download" => no_download = true,
            "--codecs" => show_codecs = true,
            "--version" => show_version = true,
            _ if arg.starts_with("--cf-profile=") => {
                remote.profile = Some(parse_profile(value_after_equals(arg, "--cf-profile"))?);
            }
            "--cf-profile" => {
                index += 1;
                remote.profile = Some(parse_profile(value_after_flag(
                    &raw_args,
                    index,
                    "--cf-profile",
                )?)?);
            }
            _ if arg.starts_with("--cf-cpu=") => {
                remote.cpu = Some(parse_cpu_cores(value_after_equals(arg, "--cf-cpu"))?);
            }
            "--cf-cpu" => {
                index += 1;
                remote.cpu = Some(parse_cpu_cores(value_after_flag(
                    &raw_args, index, "--cf-cpu",
                )?)?);
            }
            _ if arg.starts_with("--cf-memory=") => {
                remote.memory_mb = Some(parse_memory_mb(value_after_equals(arg, "--cf-memory"))?);
            }
            "--cf-memory" => {
                index += 1;
                remote.memory_mb = Some(parse_memory_mb(value_after_flag(
                    &raw_args,
                    index,
                    "--cf-memory",
                )?)?);
            }
            _ if arg.starts_with("--cf-gpu=") || arg == "--cf-gpu" => {
                return Err(CfmpegError::ParseError(
                    "GPU execution is not currently available; remove `--cf-gpu` and use `--cf-profile highcpu` instead".to_string(),
                ));
            }
            _ if arg.starts_with("--cf-timeout=") => {
                remote.timeout_seconds = Some(parse_timeout_seconds(value_after_equals(
                    arg,
                    "--cf-timeout",
                ))?);
            }
            "--cf-timeout" => {
                index += 1;
                remote.timeout_seconds = Some(parse_timeout_seconds(value_after_flag(
                    &raw_args,
                    index,
                    "--cf-timeout",
                )?)?);
            }
            _ if arg.starts_with("--cf-") => {
                return Err(CfmpegError::ParseError(format!(
                    "unknown remote execution flag: {arg}"
                )));
            }
            _ => ffmpeg_args.push(arg.clone()),
        }

        index += 1;
    }

    if show_codecs {
        if force_local || no_download || show_version || !ffmpeg_args.is_empty() {
            return Err(CfmpegError::ParseError(
                "--codecs must be used on its own".to_string(),
            ));
        }

        return Ok(Command::Codecs);
    }

    if show_version {
        if force_local || no_download || !ffmpeg_args.is_empty() {
            return Err(CfmpegError::ParseError(
                "--version must be used on its own".to_string(),
            ));
        }

        return Ok(Command::Version);
    }

    if ffmpeg_args.is_empty() {
        return Ok(Command::Help);
    }

    if force_local && !remote.is_empty() {
        return Err(CfmpegError::ParseError(
            "--cf-* flags cannot be used together with --local".to_string(),
        ));
    }

    Ok(Command::Encode {
        ffmpeg_args,
        force_local,
        no_download,
        remote,
    })
}

fn value_after_equals<'a>(arg: &'a str, flag: &str) -> &'a str {
    arg.split_once('=')
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("expected {flag}=..."))
}

fn value_after_flag<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| CfmpegError::ParseError(format!("{flag} requires a value")))
}

#[cfg(test)]
mod tests {
    use super::{parse_args, AuthAction, Command, ConfigAction};
    use crate::remote::{RemoteExecutionOptions, PROFILE_HIGHCPU};

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
                no_download: false,
                remote: RemoteExecutionOptions::default(),
            }
        );
    }

    #[test]
    fn parses_remote_execution_flags_separately_from_ffmpeg_args() {
        let command = parse_args(args(&[
            "--cf-profile",
            "highcpu",
            "--cf-cpu=8",
            "--cf-memory",
            "16g",
            "--cf-timeout=90m",
            "-i",
            "input.mov",
            "output.mp4",
        ]))
        .expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["-i", "input.mov", "output.mp4"]),
                force_local: false,
                no_download: false,
                remote: RemoteExecutionOptions {
                    profile: Some(PROFILE_HIGHCPU.to_string()),
                    cpu: Some(8),
                    memory_mb: Some(16 * 1024),
                    timeout_seconds: Some(90 * 60),
                },
            }
        );
    }

    #[test]
    fn rejects_gpu_remote_flag() {
        let error = parse_args(args(&[
            "--cf-gpu",
            "required",
            "-i",
            "input.mov",
            "output.mp4",
        ]))
        .expect_err("error");

        assert!(error
            .to_string()
            .contains("GPU execution is not currently available"));
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

    #[test]
    fn rejects_remote_flags_with_local_mode() {
        let error = parse_args(args(&[
            "--local",
            "--cf-cpu",
            "8",
            "-i",
            "input.mov",
            "output.mp4",
        ]))
        .expect_err("error");

        assert!(error.to_string().contains("--cf-*"));
    }

    #[test]
    fn parses_no_download_flag_separately_from_ffmpeg_args() {
        let command =
            parse_args(args(&["--no-download", "-i", "input.mov", "output.mp4"])).expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["-i", "input.mov", "output.mp4"]),
                force_local: false,
                no_download: true,
                remote: RemoteExecutionOptions::default(),
            }
        );
    }
}
