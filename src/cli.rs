use crate::error::{CfmpegError, Result};
use crate::parser::Overwrite;
use crate::remote::{
    parse_cpu_cores, parse_memory_mb, parse_profile, parse_timeout_seconds, RemoteExecutionOptions,
};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Auth(AuthAction),
    Cancel {
        job_id: String,
    },
    Config(Option<ConfigAction>),
    Download {
        job_id: String,
        /// Where to write the outputs; empty for the job's own file names.
        paths: Vec<PathBuf>,
        overwrite: Overwrite,
    },
    Encode {
        ffmpeg_args: Vec<String>,
        force_local: bool,
        no_download: bool,
        allow_fallback: bool,
        remote: RemoteExecutionOptions,
    },
    Help {
        topic: HelpTopic,
        exit_success: bool,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    Global,
    Auth,
    Cancel,
    Config,
    Download,
    Usage,
}

fn help_text(topic: HelpTopic) -> &'static str {
    match topic {
        HelpTopic::Global => concat!(
            "cfmpeg\n",
            "\n",
            "Usage:\n",
            "  cfmpeg [--local|--remote|--allow-fallback] [--no-download] [--cf-profile <value>] [--cf-cpu <cores>] [--cf-memory <size>] [--cf-timeout <duration>] <ffmpeg args...>\n",
            "  cfmpeg auth <login|status|logout>\n",
            "  cfmpeg cancel <job_id>\n",
            "  cfmpeg download [-y|-n] <job_id> [output paths...]\n",
            "  cfmpeg config [path|show|edit]\n",
            "  cfmpeg usage\n",
            "  cfmpeg --version\n",
            "  cfmpeg help [auth|cancel|config|download|usage]\n",
            "\n",
            "Notes:\n",
            "  - Arguments that look like ffmpeg flags are passed through directly.\n",
            "  - Options that the workers' ffmpeg 8 does not know are rejected before anything is uploaded.\n",
            "  - Use `--local` to run the command with local ffmpeg instead.\n",
            "  - Remote failures are errors by default. Use `--allow-fallback` to run local ffmpeg when\n",
            "    cloud execution is unavailable (no API key, API unreachable, insufficient balance).\n",
            "  - Use `--remote` to require cloud execution even when fallback is enabled in the config.\n",
            "  - Ctrl-C cancels the remote job. Outputs are written atomically; `-n` never overwrites.\n",
            "  - Use `--no-download` to leave completed outputs in cloud storage and print signed URLs instead.\n",
            "  - Use `--cf-*` flags to request remote execution resources without changing ffmpeg arguments.\n",
            "  - Use `cfmpeg --help` or `cfmpeg help` for CLI help.\n",
            "\n",
            "Examples:\n",
            "  cfmpeg --remote -f lavfi -i testsrc=size=128x128:rate=1 -t 1 -pix_fmt yuv420p /tmp/cfmpeg-smoke.mp4\n",
            "  cfmpeg --remote -i https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4 -c:v libx264 -crf 30 -preset veryfast /tmp/cfmpeg-url-smoke.mp4\n",
        ),
        HelpTopic::Auth => concat!(
            "cfmpeg auth\n",
            "\n",
            "Usage:\n",
            "  cfmpeg auth [status]\n",
            "  cfmpeg auth login\n",
            "  cfmpeg auth logout\n",
            "\n",
            "Notes:\n",
            "  - With no action, `cfmpeg auth` prints the current auth status.\n",
            "  - `cfmpeg auth login` opens the API key page and stores the pasted key locally.\n",
            "  - `cfmpeg auth status` masks saved API keys in its output.\n",
        ),
        HelpTopic::Cancel => concat!(
            "cfmpeg cancel\n",
            "\n",
            "Usage:\n",
            "  cfmpeg cancel <job_id>\n",
            "\n",
            "Cancel a remote job by id.\n",
        ),
        HelpTopic::Download => concat!(
            "cfmpeg download\n",
            "\n",
            "Usage:\n",
            "  cfmpeg download [-y|-n] <job_id> [output paths...]\n",
            "\n",
            "Download the outputs of a finished job again, while cloud storage still has them.\n",
            "\n",
            "Notes:\n",
            "  - Without paths, each output is written to the current directory under the name the job\n",
            "    stored it with (output_0.mp4, output_1.mp4, ...).\n",
            "  - With paths, give one per output of the job, in the order of the original command.\n",
            "  - Existing files are overwritten unless `-n` is given. `-y` is accepted for symmetry.\n",
        ),
        HelpTopic::Config => concat!(
            "cfmpeg config\n",
            "\n",
            "Usage:\n",
            "  cfmpeg config [path]\n",
            "  cfmpeg config show\n",
            "  cfmpeg config edit\n",
            "\n",
            "Notes:\n",
            "  - With no action, `cfmpeg config` prints the active config path.\n",
            "  - `cfmpeg config show` prints the current config with API keys masked.\n",
            "  - `cfmpeg config edit` opens the config file in $EDITOR.\n",
        ),
        HelpTopic::Usage => concat!(
            "cfmpeg usage\n",
            "\n",
            "Usage:\n",
            "  cfmpeg usage\n",
            "\n",
            "Print the current billing-period summary for the authenticated account.\n",
        ),
    }
}

pub fn parse_args(raw_args: Vec<String>) -> Result<Command> {
    if raw_args.is_empty() {
        return Ok(help_command(HelpTopic::Global, false));
    }

    if matches!(raw_args.as_slice(), [arg] if arg == "--help" || arg == "-h") {
        return Ok(help_command(HelpTopic::Global, true));
    }

    match raw_args[0].as_str() {
        "auth" => parse_auth(&raw_args[1..]),
        "cancel" => parse_cancel(&raw_args[1..]),
        "download" => parse_download(&raw_args[1..]),
        "config" => parse_config(&raw_args[1..]),
        "help" => parse_help(&raw_args[1..]),
        "usage" => parse_usage(&raw_args[1..]),
        "version" if raw_args.len() == 1 => Ok(Command::Version),
        "version" => Err(CfmpegError::ParseError(
            "command does not accept additional arguments".to_string(),
        )),
        _ => parse_passthrough(raw_args),
    }
}

pub fn print_help(topic: HelpTopic) {
    print!("{}", help_text(topic));
}

fn parse_auth(args: &[String]) -> Result<Command> {
    if is_help_request(args) {
        return Ok(help_command(HelpTopic::Auth, true));
    }

    match args {
        [] => Ok(Command::Auth(AuthAction::Status)),
        [action] => match action.as_str() {
            "login" => Ok(Command::Auth(AuthAction::Login)),
            "logout" => Ok(Command::Auth(AuthAction::Logout)),
            "status" => Ok(Command::Auth(AuthAction::Status)),
            _ => Err(CfmpegError::ParseError(format!(
                "unknown auth action: {action}; expected login, status, logout, or --help"
            ))),
        },
        _ => Err(CfmpegError::ParseError(
            "auth accepts exactly one action".to_string(),
        )),
    }
}

fn parse_cancel(args: &[String]) -> Result<Command> {
    if is_help_request(args) {
        return Ok(help_command(HelpTopic::Cancel, true));
    }

    match args {
        [job_id] => Ok(Command::Cancel {
            job_id: job_id.clone(),
        }),
        _ => Err(CfmpegError::ParseError(
            "cancel accepts exactly one job id".to_string(),
        )),
    }
}

fn parse_download(args: &[String]) -> Result<Command> {
    if is_help_request(args) {
        return Ok(help_command(HelpTopic::Download, true));
    }

    let mut force_overwrite = false;
    let mut never_overwrite = false;
    let mut positional = Vec::new();

    for arg in args {
        match arg.as_str() {
            "-y" => force_overwrite = true,
            "-n" => never_overwrite = true,
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(CfmpegError::ParseError(format!(
                    "unknown download option: {arg}; expected -y, -n, or --help"
                )));
            }
            _ => positional.push(arg),
        }
    }

    let Some((job_id, paths)) = positional.split_first() else {
        return Err(CfmpegError::ParseError(
            "download requires a job id".to_string(),
        ));
    };
    let overwrite = match (force_overwrite, never_overwrite) {
        (true, true) => {
            return Err(CfmpegError::ParseError(
                "both -y and -n supplied".to_string(),
            ));
        }
        (_, true) => Overwrite::Never,
        (_, false) => Overwrite::Allowed,
    };

    Ok(Command::Download {
        job_id: (*job_id).clone(),
        paths: paths.iter().map(PathBuf::from).collect(),
        overwrite,
    })
}

fn parse_config(args: &[String]) -> Result<Command> {
    if is_help_request(args) {
        return Ok(help_command(HelpTopic::Config, true));
    }

    match args {
        [] => Ok(Command::Config(None)),
        [action] => match action.as_str() {
            "edit" => Ok(Command::Config(Some(ConfigAction::Edit))),
            "path" => Ok(Command::Config(Some(ConfigAction::Path))),
            "show" => Ok(Command::Config(Some(ConfigAction::Show))),
            _ => Err(CfmpegError::ParseError(format!(
                "unknown config action: {action}; expected path, show, edit, or --help"
            ))),
        },
        _ => Err(CfmpegError::ParseError(
            "config accepts at most one action".to_string(),
        )),
    }
}

fn parse_help(args: &[String]) -> Result<Command> {
    match args {
        [] => Ok(help_command(HelpTopic::Global, true)),
        [topic] => help_topic(topic)
            .map(|topic| help_command(topic, true))
            .ok_or_else(|| {
                CfmpegError::ParseError(format!(
                    "unknown help topic: {topic}; expected auth, cancel, config, download, or usage"
                ))
            }),
        _ => Err(CfmpegError::ParseError(
            "help accepts at most one topic".to_string(),
        )),
    }
}

fn parse_usage(args: &[String]) -> Result<Command> {
    if is_help_request(args) {
        return Ok(help_command(HelpTopic::Usage, true));
    }

    if args.is_empty() {
        Ok(Command::Usage)
    } else {
        Err(CfmpegError::ParseError(
            "usage does not accept additional arguments; use `cfmpeg usage --help` for help"
                .to_string(),
        ))
    }
}

fn help_command(topic: HelpTopic, exit_success: bool) -> Command {
    Command::Help {
        topic,
        exit_success,
    }
}

fn is_help_request(args: &[String]) -> bool {
    matches!(args, [arg] if arg == "--help" || arg == "-h")
}

fn help_topic(topic: &str) -> Option<HelpTopic> {
    match topic {
        "auth" => Some(HelpTopic::Auth),
        "cancel" => Some(HelpTopic::Cancel),
        "config" => Some(HelpTopic::Config),
        "download" => Some(HelpTopic::Download),
        "usage" => Some(HelpTopic::Usage),
        _ => None,
    }
}

fn parse_passthrough(raw_args: Vec<String>) -> Result<Command> {
    let mut force_local = false;
    let mut no_download = false;
    let mut force_remote = false;
    let mut allow_fallback = false;
    let mut show_version = false;
    let mut ffmpeg_args = Vec::new();
    let mut remote = RemoteExecutionOptions::default();
    let mut index = 0usize;

    while index < raw_args.len() {
        let arg = &raw_args[index];
        match arg.as_str() {
            "--local" => force_local = true,
            "--remote" => force_remote = true,
            "--allow-fallback" => allow_fallback = true,
            "--no-download" => no_download = true,
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
                return Err(CfmpegError::ParseError(match suggested_cfmpeg_flag(arg) {
                    Some(suggestion) => format!(
                        "unknown remote execution flag: {arg}; did you mean `{suggestion}`?"
                    ),
                    None => format!("unknown remote execution flag: {arg}"),
                }));
            }
            _ if arg.starts_with("--") => {
                if let Some(suggestion) = suggested_cfmpeg_flag(arg) {
                    return Err(CfmpegError::ParseError(format!(
                        "unknown cfmpeg flag: {arg}; did you mean `{suggestion}`?"
                    )));
                }

                ffmpeg_args.push(arg.clone());
            }
            _ => ffmpeg_args.push(arg.clone()),
        }

        index += 1;
    }

    if show_version {
        if force_local || force_remote || allow_fallback || no_download || !ffmpeg_args.is_empty() {
            return Err(CfmpegError::ParseError(
                "--version must be used on its own".to_string(),
            ));
        }

        return Ok(Command::Version);
    }

    if ffmpeg_args.is_empty() {
        return Ok(help_command(HelpTopic::Global, false));
    }

    if force_remote {
        remote.strict_remote = true;
    }

    if force_local && remote.strict_remote {
        return Err(CfmpegError::ParseError(
            "--remote cannot be used together with --local".to_string(),
        ));
    }

    if force_local && !remote.is_empty() {
        return Err(CfmpegError::ParseError(
            "--cf-* flags cannot be used together with --local".to_string(),
        ));
    }

    if allow_fallback && force_remote {
        return Err(CfmpegError::ParseError(
            "--allow-fallback cannot be used together with --remote".to_string(),
        ));
    }

    // Asking for remote resources means asking for a remote run, unless the
    // same command line explicitly allows the fallback.
    if !remote.is_empty() && !allow_fallback {
        remote.strict_remote = true;
    }

    Ok(Command::Encode {
        ffmpeg_args,
        force_local,
        no_download,
        allow_fallback,
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

fn suggested_cfmpeg_flag(arg: &str) -> Option<&'static str> {
    const OWNED_FLAGS: &[&str] = &[
        "--local",
        "--remote",
        "--allow-fallback",
        "--no-download",
        "--cf-profile",
        "--cf-cpu",
        "--cf-memory",
        "--cf-timeout",
    ];

    let candidate = arg.split_once('=').map(|(flag, _)| flag).unwrap_or(arg);

    OWNED_FLAGS
        .iter()
        .filter_map(|flag| {
            let distance = damerau_levenshtein_distance(candidate, flag);
            (distance <= 2).then_some((*flag, distance))
        })
        .min_by_key(|(_, distance)| *distance)
        .map(|(flag, _)| flag)
}

fn damerau_levenshtein_distance(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut distances = vec![vec![0usize; right.len() + 1]; left.len() + 1];

    for (index, row) in distances.iter_mut().enumerate() {
        row[0] = index;
    }

    for (index, distance) in distances[0].iter_mut().enumerate() {
        *distance = index;
    }

    for left_index in 1..=left.len() {
        for right_index in 1..=right.len() {
            let substitution_cost = usize::from(left[left_index - 1] != right[right_index - 1]);
            let mut distance = (distances[left_index - 1][right_index] + 1)
                .min(distances[left_index][right_index - 1] + 1)
                .min(distances[left_index - 1][right_index - 1] + substitution_cost);

            if left_index > 1
                && right_index > 1
                && left[left_index - 1] == right[right_index - 2]
                && left[left_index - 2] == right[right_index - 1]
            {
                distance = distance.min(distances[left_index - 2][right_index - 2] + 1);
            }

            distances[left_index][right_index] = distance;
        }
    }

    distances[left.len()][right.len()]
}

#[cfg(test)]
mod tests {
    use super::{parse_args, AuthAction, Command, ConfigAction, HelpTopic};
    use crate::parser::Overwrite;
    use crate::remote::{RemoteExecutionOptions, PROFILE_HIGHCPU};
    use std::path::PathBuf;

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
                allow_fallback: false,
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
                allow_fallback: false,
                remote: RemoteExecutionOptions {
                    profile: Some(PROFILE_HIGHCPU.to_string()),
                    cpu: Some(8),
                    memory_mb: Some(16 * 1024),
                    timeout_seconds: Some(90 * 60),
                    strict_remote: true,
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
    fn parses_common_help_flags_as_cli_help() {
        assert_eq!(
            parse_args(args(&["--help"])).expect("command"),
            Command::Help {
                topic: HelpTopic::Global,
                exit_success: true,
            }
        );
        assert_eq!(
            parse_args(args(&["-h"])).expect("command"),
            Command::Help {
                topic: HelpTopic::Global,
                exit_success: true,
            }
        );
    }

    #[test]
    fn parses_no_args_as_unsuccessful_help() {
        assert_eq!(
            parse_args(args(&[])).expect("command"),
            Command::Help {
                topic: HelpTopic::Global,
                exit_success: false,
            }
        );
    }

    #[test]
    fn parses_subcommand_help_flags_as_successful_help() {
        assert_eq!(
            parse_args(args(&["auth", "--help"])).expect("command"),
            Command::Help {
                topic: HelpTopic::Auth,
                exit_success: true,
            }
        );
        assert_eq!(
            parse_args(args(&["config", "-h"])).expect("command"),
            Command::Help {
                topic: HelpTopic::Config,
                exit_success: true,
            }
        );
    }

    #[test]
    fn parses_cancel_subcommand() {
        let command = parse_args(args(&["cancel", "job_123"])).expect("command");

        assert_eq!(
            command,
            Command::Cancel {
                job_id: "job_123".to_string(),
            }
        );
    }

    #[test]
    fn parses_download_subcommand() {
        assert_eq!(
            parse_args(args(&["download", "job_123"])).expect("command"),
            Command::Download {
                job_id: "job_123".to_string(),
                paths: Vec::new(),
                overwrite: Overwrite::Allowed,
            }
        );
        assert_eq!(
            parse_args(args(&["download", "-n", "job_123", "a.mp4", "out/b.webm"]))
                .expect("command"),
            Command::Download {
                job_id: "job_123".to_string(),
                paths: vec![PathBuf::from("a.mp4"), PathBuf::from("out/b.webm")],
                overwrite: Overwrite::Never,
            }
        );
        assert_eq!(
            parse_args(args(&["download", "--help"])).expect("command"),
            Command::Help {
                topic: HelpTopic::Download,
                exit_success: true,
            }
        );
    }

    #[test]
    fn rejects_incomplete_or_contradictory_download_commands() {
        for (arguments, expected) in [
            (&["download"][..], "requires a job id"),
            (&["download", "-y"][..], "requires a job id"),
            (&["download", "-y", "-n", "job_123"][..], "both -y and -n"),
            (
                &["download", "--force", "job_123"][..],
                "unknown download option",
            ),
        ] {
            let error = parse_args(args(arguments)).expect_err("error");

            assert!(
                error.to_string().contains(expected),
                "{arguments:?}: {error}"
            );
        }
    }

    #[test]
    fn parses_config_subcommand() {
        let command = parse_args(args(&["config", "show"])).expect("command");

        assert_eq!(command, Command::Config(Some(ConfigAction::Show)));
    }

    #[test]
    fn treats_double_dash_codecs_as_ffmpeg_passthrough() {
        let command = parse_args(args(&["--codecs", "output.txt"])).expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["--codecs", "output.txt"]),
                force_local: false,
                no_download: false,
                allow_fallback: false,
                remote: RemoteExecutionOptions::default(),
            }
        );
    }

    #[test]
    fn rejects_near_miss_cfmpeg_owned_flags() {
        let remote_error =
            parse_args(args(&["--remot", "-i", "input.mov", "output.mp4"])).expect_err("error");
        assert!(remote_error.to_string().contains("did you mean `--remote`"));

        let no_download_error =
            parse_args(args(&["--no-downlaod", "-i", "input.mov", "output.mp4"]))
                .expect_err("error");
        assert!(no_download_error
            .to_string()
            .contains("did you mean `--no-download`"));
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
    fn parses_remote_flag_as_strict_remote_without_execution_options() {
        let command =
            parse_args(args(&["--remote", "-i", "input.mov", "output.mp4"])).expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["-i", "input.mov", "output.mp4"]),
                force_local: false,
                no_download: false,
                allow_fallback: false,
                remote: RemoteExecutionOptions {
                    strict_remote: true,
                    ..RemoteExecutionOptions::default()
                },
            }
        );
    }

    #[test]
    fn parses_allow_fallback_flag_separately_from_ffmpeg_args() {
        let command = parse_args(args(&["--allow-fallback", "-i", "input.mov", "output.mp4"]))
            .expect("command");

        assert_eq!(
            command,
            Command::Encode {
                ffmpeg_args: args(&["-i", "input.mov", "output.mp4"]),
                force_local: false,
                no_download: false,
                allow_fallback: true,
                remote: RemoteExecutionOptions::default(),
            }
        );
    }

    #[test]
    fn explicit_allow_fallback_keeps_resource_flags_from_requiring_remote() {
        let command = parse_args(args(&[
            "--allow-fallback",
            "--cf-cpu",
            "8",
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
                allow_fallback: true,
                remote: RemoteExecutionOptions {
                    cpu: Some(8),
                    ..RemoteExecutionOptions::default()
                },
            }
        );
    }

    #[test]
    fn rejects_allow_fallback_with_remote_flag() {
        let error = parse_args(args(&[
            "--allow-fallback",
            "--remote",
            "-i",
            "input.mov",
            "output.mp4",
        ]))
        .expect_err("error");

        assert!(error.to_string().contains("--allow-fallback"));
    }

    #[test]
    fn rejects_remote_flag_with_local_mode() {
        let error = parse_args(args(&[
            "--local",
            "--remote",
            "-i",
            "input.mov",
            "output.mp4",
        ]))
        .expect_err("error");

        assert!(error.to_string().contains("--remote"));
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
                allow_fallback: false,
                remote: RemoteExecutionOptions::default(),
            }
        );
    }
}
