use crate::error::{CfmpegError, Result};
use crate::ffmpeg_options::{self, OptionSyntax};
use crate::local_references;
use crate::output_support::{self, OutputOptions};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Input {
    LocalFile { path: PathBuf, size: u64 },
    Special(String),
    Url(String),
}

#[derive(Debug, Clone)]
pub struct Output {
    pub path: PathBuf,
    pub remote_name: String,
}

/// What to do with an output file that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// Replace it: `-y`, and also the default because cfmpeg never prompts.
    Allowed,
    /// Refuse to run: `-n`.
    Never,
}

#[derive(Debug)]
pub struct ParsedCommand {
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub sandbox_args: Vec<String>,
    pub overwrite: Overwrite,
}

/// Splits an ffmpeg command line into inputs, outputs and the arguments the
/// remote worker runs, the way ffmpeg 8 itself reads it.
///
/// Fails without guessing when an option is not known, and when an input or
/// output cannot work on a remote worker.
pub fn parse_ffmpeg_args(args: &[String]) -> Result<ParsedCommand> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut sandbox_args = Vec::new();
    let mut input_counter = 0usize;
    // ffmpeg applies options to the next input or output that follows them.
    let mut pending = OutputOptions::default();
    let mut force_overwrite = false;
    let mut never_overwrite = false;
    // ffmpeg reads the token after `--` as an output file even when it starts
    // with a dash.
    let mut after_double_dash = false;

    let mut tokens = args.iter();
    while let Some(arg) = tokens.next() {
        if arg == "--" && !after_double_dash {
            sandbox_args.push(arg.clone());
            after_double_dash = true;
            continue;
        }

        if std::mem::take(&mut after_double_dash) || !arg.starts_with('-') || arg == "-" {
            // ffmpeg reads `file:out.mp4` as the plain path `out.mp4`.
            let output = arg.strip_prefix("file:").unwrap_or(arg);
            output_support::ensure_supported(output, &pending)?;

            let output_path = PathBuf::from(output);
            let remote_name = build_remote_output_name(&output_path, outputs.len());
            sandbox_args.push(rewrite_output(&remote_name));
            outputs.push(Output {
                path: output_path,
                remote_name,
            });
            pending = OutputOptions::default();
            continue;
        }

        if arg == "-i" {
            let input_arg = tokens.next().ok_or_else(|| {
                CfmpegError::ParseError("-i flag requires an input path or URL".to_string())
            })?;

            if pending
                .format
                .as_deref()
                .is_some_and(|format| format.eq_ignore_ascii_case("concat"))
            {
                return Err(CfmpegError::ParseError(
                    "concat file lists are not supported for remote execution; use `--local` or expand the concat inputs before submitting the job".to_string(),
                ));
            }

            let input = classify_input(input_arg, pending.format.as_deref())?;
            sandbox_args.push(arg.clone());
            sandbox_args.push(rewrite_input(&input, input_counter));
            if matches!(input, Input::LocalFile { .. }) {
                input_counter += 1;
            }
            inputs.push(input);
            pending = OutputOptions::default();
            continue;
        }

        let option =
            ffmpeg_options::classify(arg).ok_or_else(|| CfmpegError::UnknownOption(arg.clone()))?;

        match option.syntax {
            OptionSyntax::Info => {
                return Err(CfmpegError::ParseError(format!(
                    "{arg} prints ffmpeg information instead of running a job; run it with `--local`"
                )));
            }
            OptionSyntax::ValueFromFile => {
                return Err(CfmpegError::ParseError(format!(
                    "{arg} reads its value from a local file, which is not supported for remote execution; use `--local` or pass the value inline"
                )));
            }
            OptionSyntax::Flag => {
                local_references::ensure_option_works_remotely(arg, option.name, None)?;

                match option.name {
                    "y" => force_overwrite = !option.negated,
                    "n" => never_overwrite = !option.negated,
                    _ => {}
                }
                sandbox_args.push(arg.clone());
            }
            OptionSyntax::Value => {
                if matches!(option.name, "filter_script" | "filter_complex_script") {
                    return Err(CfmpegError::ParseError(format!(
                        "{arg} references a local script file, which is not supported for remote execution; use `--local` or inline the filter graph"
                    )));
                }

                let value = tokens.next().ok_or_else(|| {
                    CfmpegError::ParseError(format!("{arg} requires an argument"))
                })?;
                local_references::ensure_option_works_remotely(arg, option.name, Some(value))?;

                match option.name {
                    "f" => pending.format = Some(value.clone()),
                    "strftime" => {
                        // The spellings ffmpeg reads as a false boolean.
                        pending.strftime = !matches!(
                            value.to_ascii_lowercase().as_str(),
                            "0" | "n" | "no" | "false" | "off" | "disable" | "disabled"
                        );
                    }
                    _ => {}
                }
                sandbox_args.push(arg.clone());
                sandbox_args.push(value.clone());
            }
        }
    }

    if outputs.is_empty() {
        return Err(CfmpegError::ParseError(
            "no output file detected; include at least one output path".to_string(),
        ));
    }

    if inputs.is_empty() {
        return Err(CfmpegError::ParseError(
            "no input file detected; include at least one `-i <input>`".to_string(),
        ));
    }

    let overwrite = match (force_overwrite, never_overwrite) {
        (true, true) => {
            return Err(CfmpegError::ParseError(
                "both -y and -n supplied".to_string(),
            ));
        }
        (_, true) => Overwrite::Never,
        (_, false) => Overwrite::Allowed,
    };

    Ok(ParsedCommand {
        inputs,
        outputs,
        sandbox_args,
        overwrite,
    })
}

fn classify_input(path: &str, format: Option<&str>) -> Result<Input> {
    // With `-f lavfi` the input is a filtergraph that generates the media, not
    // a file, whatever it is called.
    if format.is_some_and(|format| format.eq_ignore_ascii_case("lavfi")) {
        local_references::ensure_graph_works_remotely("-i", path)?;

        if !is_special_input(path) {
            return Err(CfmpegError::ParseError(format!(
                "lavfi input `{path}` is not supported for remote execution yet; the workers accept graphs that start with color=, testsrc, anullsrc or nullsrc; run this command with `--local`"
            )));
        }

        return Ok(Input::Special(path.to_string()));
    }

    if is_remote_url(path) {
        return Ok(Input::Url(path.to_string()));
    }

    if is_remote_unsupported_input(path) {
        return Err(CfmpegError::ParseError(format!(
            "input `{path}` cannot be used for remote execution; stdin, pipe, and /dev inputs require `--local`"
        )));
    }

    if is_special_input(path) {
        return Ok(Input::Special(path.to_string()));
    }

    // ffmpeg reads `file:in.mov` as the plain path `in.mov`.
    let path = path.strip_prefix("file:").unwrap_or(path);
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        return Err(CfmpegError::InputNotFound(path.to_string()));
    }

    let size = std::fs::metadata(&path_buf)?.len();
    Ok(Input::LocalFile {
        path: path_buf,
        size,
    })
}

fn is_remote_url(path: &str) -> bool {
    [
        "http://", "https://", "s3://", "r2://", "gs://", "rtmp://", "rtsp://", "srt://",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

fn is_special_input(path: &str) -> bool {
    path.starts_with("lavfi:")
        || path.starts_with("color=")
        || path.starts_with("testsrc")
        || path.starts_with("anullsrc")
        || path.starts_with("nullsrc")
}

fn is_remote_unsupported_input(path: &str) -> bool {
    path == "-" || path.starts_with("pipe:") || path.starts_with("/dev/")
}

fn rewrite_input(input: &Input, input_counter: usize) -> String {
    match input {
        Input::LocalFile { path, .. } => {
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .map(|value| format!(".{value}"))
                .unwrap_or_default();
            format!("/tmp/cfmpeg/inputs/input_{input_counter}{extension}")
        }
        Input::Special(value) | Input::Url(value) => value.clone(),
    }
}

fn rewrite_output(remote_name: &str) -> String {
    format!("/tmp/cfmpeg/outputs/{remote_name}")
}

fn build_remote_output_name(output_path: &Path, output_counter: usize) -> String {
    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();

    format!("output_{output_counter}{extension}")
}

#[cfg(test)]
mod tests {
    use super::{parse_ffmpeg_args, Input, Overwrite, ParsedCommand};
    use crate::error::{CfmpegError, Result};
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cfmpeg-parser-{name}-{}", Uuid::new_v4()))
    }

    /// Parses `-i <url>` followed by `rest`.
    fn parse_after_url_input(rest: &[&str]) -> Result<ParsedCommand> {
        let mut args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
        ];
        args.extend(rest.iter().map(|arg| (*arg).to_string()));

        parse_ffmpeg_args(&args)
    }

    fn output_paths(parsed: &ParsedCommand) -> Vec<PathBuf> {
        parsed
            .outputs
            .iter()
            .map(|output| output.path.clone())
            .collect()
    }

    #[test]
    fn option_values_are_never_mistaken_for_output_files() {
        for (option, value) in [
            ("-x265-params", "crf=28"),
            ("-x264-params", "keyint=48:min-keyint=48"),
            ("-force_key_frames", "expr:gte(t,n_forced*2)"),
            ("-strict", "experimental"),
            ("-fps_mode", "cfr"),
            ("-probesize", "50M"),
            ("-analyzeduration", "100M"),
            ("-movflags", "+faststart"),
            ("-b:a:0", "128k"),
            ("-c:v", "libx264"),
            ("-filter:v", "scale=1280:-2"),
            ("-metadata:s:a:0", "language=eng"),
            ("-disposition:a:1", "default"),
            ("-map", "-0:s"),
            ("-s", "1280x720"),
            ("-crf", "23"),
            ("-preset", "slow"),
            ("-pix_fmt", "yuv420p"),
            ("-thread_queue_size", "512"),
            ("-avoid_negative_ts", "make_zero"),
            ("-sws_flags", "lanczos"),
        ] {
            let parsed = parse_after_url_input(&[option, value, "output.mp4"])
                .unwrap_or_else(|error| panic!("{option} {value}: {error}"));

            assert_eq!(
                output_paths(&parsed),
                vec![PathBuf::from("output.mp4")],
                "{option}"
            );
            assert_eq!(
                parsed.sandbox_args[2..],
                [option, value, "/tmp/cfmpeg/outputs/output_0.mp4"],
                "{option}"
            );
        }
    }

    #[test]
    fn parses_the_commands_documented_in_the_readme() {
        for command in [
            "-i https://example.com/input.mov -c:v libx265 -crf 28 output.mp4",
            "-f lavfi -i testsrc=size=128x128:rate=1 -t 1 -pix_fmt yuv420p /tmp/cfmpeg-smoke.mp4",
            "-i https://example.com/input.mp4 -c:v libx264 -crf 30 -preset veryfast /tmp/cfmpeg-url-smoke.mp4",
        ] {
            let args: Vec<String> = command.split(' ').map(str::to_string).collect();

            let parsed =
                parse_ffmpeg_args(&args).unwrap_or_else(|error| panic!("{command}: {error}"));

            assert_eq!(parsed.outputs.len(), 1, "{command}");
        }
    }

    #[test]
    fn treats_lavfi_graphs_as_generated_inputs_not_files() {
        for graph in [
            "color=c=black:s=1280x720",
            "testsrc2=size=640x360:rate=30",
            "anullsrc=r=48000:cl=stereo",
            "nullsrc=s=64x64",
        ] {
            let args = ["-f", "lavfi", "-i", graph, "-t", "1", "output.mp4"].map(str::to_string);

            let parsed =
                parse_ffmpeg_args(&args).unwrap_or_else(|error| panic!("{graph}: {error}"));

            assert!(
                matches!(&parsed.inputs[0], Input::Special(value) if value == graph),
                "{graph}"
            );
            assert_eq!(parsed.sandbox_args[..4], ["-f", "lavfi", "-i", graph]);
        }
    }

    #[test]
    fn explains_lavfi_graphs_the_workers_do_not_accept_yet() {
        for graph in ["sine=frequency=440", "smptebars=size=640x360", "mandelbrot"] {
            let args = ["-f", "lavfi", "-i", graph, "-t", "1", "output.mp4"].map(str::to_string);

            let error = parse_ffmpeg_args(&args).expect_err(graph);

            assert!(
                matches!(error, CfmpegError::ParseError(_)),
                "{graph}: {error}"
            );
            assert!(
                error.to_string().contains("lavfi input")
                    && error.to_string().contains("`--local`"),
                "{graph}: {error}"
            );
        }

        let args = ["-f", "lavfi", "-i", "movie=clip.mp4", "output.mp4"].map(str::to_string);
        let error = parse_ffmpeg_args(&args).expect_err("movie source");
        assert!(error
            .to_string()
            .contains("the `movie` filter reads the local path `clip.mp4`"));
    }

    #[test]
    fn rejects_options_and_filters_that_need_local_side_files() {
        for rest in [
            &["-pass", "1", "-an", "output.mp4"][..],
            &["-pass:v", "2", "-passlogfile", "stats", "output.mp4"][..],
            &["-attach", "font.ttf", "output.mkv"][..],
            &["-fpre", "fast.ffpreset", "output.mp4"][..],
            &["-vpre", "fast", "output.mp4"][..],
            &["-vstats", "output.mp4"][..],
            &["-vstats_file", "stats.log", "output.mp4"][..],
            &["-sdp_file", "out.sdp", "output.mp4"][..],
            &["-progress", "progress.txt", "output.mp4"][..],
            &["-vf", "subtitles=subs.srt", "output.mp4"][..],
            &["-filter:v", "ass=subs.ass", "output.mp4"][..],
            &["-af", "arnndn=m=speech.rnnn", "output.mp4"][..],
            &[
                "-filter_complex",
                "movie=logo.png[wm];[0:v][wm]overlay=10:10",
                "output.mp4",
            ][..],
            &[
                "-lavfi",
                "[0:v]lut3d=grade.cube[v]",
                "-map",
                "[v]",
                "output.mp4",
            ][..],
        ] {
            let error = match parse_after_url_input(rest) {
                Ok(parsed) => panic!("{rest:?} parsed as {:?}", parsed.sandbox_args),
                Err(error) => error,
            };

            assert!(
                matches!(error, CfmpegError::UnsupportedOption { .. }),
                "{rest:?}: {error}"
            );
        }
    }

    #[test]
    fn keeps_filters_and_progress_pipes_that_need_no_local_file() {
        parse_after_url_input(&[
            "-vf",
            "scale=1280:-2,drawtext=text='subtitles=on':font=Sans",
            "-af",
            "loudnorm=I=-16",
            "-progress",
            "pipe:1",
            "-stats",
            "output.mp4",
        ])
        .expect("no local file is involved");
    }

    #[test]
    fn reads_a_file_prefix_as_the_plain_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let input_path = directory.path().join("clip.mov");
        fs::write(&input_path, b"video").expect("input file");
        let args = vec![
            "-i".to_string(),
            format!("file:{}", input_path.display()),
            "file:exports/output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert!(matches!(&parsed.inputs[0], Input::LocalFile { path, .. } if *path == input_path));
        assert_eq!(
            output_paths(&parsed),
            vec![PathBuf::from("exports/output.mp4")]
        );
        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "/tmp/cfmpeg/inputs/input_0.mov",
                "/tmp/cfmpeg/outputs/output_0.mp4"
            ]
        );
    }

    #[test]
    fn options_without_argument_do_not_consume_the_output() {
        for flag in [
            "-y",
            "-n",
            "-an",
            "-vn",
            "-sn",
            "-dn",
            "-shortest",
            "-hide_banner",
            "-re",
            "-copyts",
            "-nostdin",
            "-nostats",
            "-noautorotate",
        ] {
            let parsed = parse_after_url_input(&[flag, "output.mp4"])
                .unwrap_or_else(|error| panic!("{flag}: {error}"));

            assert_eq!(
                output_paths(&parsed),
                vec![PathBuf::from("output.mp4")],
                "{flag}"
            );
        }
    }

    #[test]
    fn rejects_unknown_options_instead_of_guessing() {
        let error = parse_after_url_input(&["-foo", "bar", "output.mp4"]).expect_err("unknown");

        assert!(matches!(&error, CfmpegError::UnknownOption(option) if option == "-foo"));
        assert_eq!(
            error.to_string(),
            "cfmpeg does not recognise the ffmpeg option `-foo`; if it is a valid ffmpeg 8 option, please report it at https://github.com/cfmpeg/cfmpeg-cli/issues, and meanwhile run this command with `--local`"
        );
    }

    #[test]
    fn rejects_information_options() {
        let error = parse_after_url_input(&["-encoders"]).expect_err("information option");

        assert!(error.to_string().contains("prints ffmpeg information"));
    }

    #[test]
    fn rejects_option_values_loaded_from_local_files() {
        let error = parse_after_url_input(&["-/filter:v", "graph.txt", "output.mp4"])
            .expect_err("value from file");

        assert!(error
            .to_string()
            .contains("reads its value from a local file"));
    }

    #[test]
    fn reads_the_token_after_a_double_dash_as_an_output() {
        let parsed = parse_after_url_input(&["--", "-dashed.mp4"]).expect("parsed command");

        assert_eq!(output_paths(&parsed), vec![PathBuf::from("-dashed.mp4")]);
        assert_eq!(
            parsed.sandbox_args[2..],
            ["--", "/tmp/cfmpeg/outputs/output_0.mp4"]
        );
    }

    #[test]
    fn overwrites_unless_n_is_given() {
        for (flags, expected) in [
            (&[][..], Overwrite::Allowed),
            (&["-y"][..], Overwrite::Allowed),
            (&["-n"][..], Overwrite::Never),
            (&["-n", "-non"][..], Overwrite::Allowed),
        ] {
            let mut rest = flags.to_vec();
            rest.push("output.mp4");

            let parsed = parse_after_url_input(&rest).expect("parsed command");

            assert_eq!(parsed.overwrite, expected, "{flags:?}");
        }
    }

    #[test]
    fn rejects_y_together_with_n_like_ffmpeg() {
        let error = parse_after_url_input(&["-y", "-n", "output.mp4"]).expect_err("-y and -n");

        assert!(error.to_string().contains("both -y and -n supplied"));
    }

    #[test]
    fn rejects_outputs_that_write_more_than_the_named_file() {
        for rest in [
            &["-f", "hls", "-hls_time", "6", "stream.m3u8"][..],
            &["-hls_time", "6", "stream.m3u8"][..],
            &["-f", "dash", "manifest.mpd"][..],
            &["manifest.mpd"][..],
            &["-f", "segment", "-segment_time", "10", "part%03d.mp4"][..],
            &["-f", "segment", "-segment_time", "10", "part.mp4"][..],
            &["-f", "stream_segment", "part.ts"][..],
            &["-f", "ssegment", "part.ts"][..],
            &["-f", "tee", "[f=mp4]a.mp4|[f=flv]b.flv"][..],
            &["thumb%03d.jpg"][..],
            &["-f", "image2", "thumb%d.png"][..],
            &["-strftime", "1", "shot-%Y%m%d.jpg"][..],
            &["-f", "mpegts", "-"][..],
            &["-f", "null", "-"][..],
            &["-f", "mpegts", "pipe:"][..],
            &["-f", "mpegts", "pipe:1"][..],
            &["-f", "mpegts", "/dev/stdout"][..],
            &["first.mp4", "-f", "hls", "second.m3u8"][..],
        ] {
            let error = match parse_after_url_input(rest) {
                Ok(parsed) => panic!("{rest:?} parsed as {:?}", parsed.sandbox_args),
                Err(error) => error,
            };

            assert!(
                matches!(error, CfmpegError::UnsupportedOutput { .. }),
                "{rest:?}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("not supported for remote execution yet")
                    && error.to_string().contains("`--local`"),
                "{rest:?}: {error}"
            );
        }
    }

    #[test]
    fn keeps_single_image_and_plain_multiple_outputs_working() {
        let poster = parse_after_url_input(&["-f", "image2", "-frames:v", "1", "poster.jpg"])
            .expect("single image");
        assert_eq!(output_paths(&poster), vec![PathBuf::from("poster.jpg")]);

        // `-f` applies to the next output only.
        let several = parse_after_url_input(&[
            "-f",
            "mp4",
            "first.m3u8",
            "-strftime",
            "0",
            "second-100%.mp4",
            "third.webm",
        ])
        .expect("several outputs");
        assert_eq!(
            output_paths(&several),
            vec![
                PathBuf::from("first.m3u8"),
                PathBuf::from("second-100%.mp4"),
                PathBuf::from("third.webm"),
            ]
        );
    }

    #[test]
    fn parses_local_input_and_rewrites_paths() {
        let input_path = temp_path("input.mov");
        let output_dir = temp_path("outputs");
        let output_path = output_dir.join("output.mp4");
        fs::create_dir_all(&output_dir).expect("output dir");
        fs::write(&input_path, b"video").expect("input file");

        let args = vec![
            "-i".to_string(),
            input_path.display().to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
            output_path.display().to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(parsed.inputs.len(), 1);
        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].path, output_path);
        assert_eq!(parsed.outputs[0].remote_name, "output_0.mp4");
        assert!(matches!(parsed.inputs[0], Input::LocalFile { .. }));
        assert_eq!(parsed.sandbox_args[0], "-i");
        assert!(parsed.sandbox_args[1].starts_with("/tmp/cfmpeg/inputs/input_0"));
        assert_eq!(
            parsed.sandbox_args.last().expect("output arg"),
            "/tmp/cfmpeg/outputs/output_0.mp4"
        );

        let _ = fs::remove_file(input_path);
        let _ = fs::remove_dir_all(output_dir);
    }

    #[test]
    fn parses_remote_url_input() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert!(matches!(parsed.inputs[0], Input::Url(_)));
        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].remote_name, "output_0.mp4");
    }

    #[test]
    fn returns_error_when_output_is_missing() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
        ];

        let error = parse_ffmpeg_args(&args).expect_err("missing output");

        assert!(error.to_string().contains("no output file detected"));
    }

    #[test]
    fn returns_error_when_input_is_missing() {
        let args = vec!["output.mp4".to_string()];

        let error = parse_ffmpeg_args(&args).expect_err("missing input");

        assert!(error.to_string().contains("no input file detected"));
    }

    #[test]
    fn assigns_unique_remote_names_for_duplicate_output_basenames() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "renders/output.mp4".to_string(),
            "exports/output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(parsed.outputs[0].remote_name, "output_0.mp4");
        assert_eq!(parsed.outputs[1].remote_name, "output_1.mp4");
        assert_eq!(parsed.sandbox_args[1], "https://example.com/input.mov");
        assert_eq!(parsed.sandbox_args[2], "/tmp/cfmpeg/outputs/output_0.mp4");
        assert_eq!(parsed.sandbox_args[3], "/tmp/cfmpeg/outputs/output_1.mp4");
    }

    #[test]
    fn reports_missing_output_for_stream_specifier_valued_option() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "-disposition:s:0".to_string(),
            "default".to_string(),
        ];

        let error = parse_ffmpeg_args(&args).expect_err("missing output");

        assert!(error.to_string().contains("no output file detected"));
    }

    #[test]
    fn reports_missing_output_for_common_valued_option_family() {
        let args = vec![
            "-stream_loop".to_string(),
            "2".to_string(),
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
        ];

        let error = parse_ffmpeg_args(&args).expect_err("missing output");

        assert!(error.to_string().contains("no output file detected"));
    }

    #[test]
    fn matches_stream_specifier_option_families_without_duplicate_entries() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "-c:v:0".to_string(),
            "libx264".to_string(),
            "-metadata:s:v:0".to_string(),
            "title=Main".to_string(),
            "output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "https://example.com/input.mov",
                "-c:v:0",
                "libx264",
                "-metadata:s:v:0",
                "title=Main",
                "/tmp/cfmpeg/outputs/output_0.mp4",
            ]
        );
    }

    #[test]
    fn reads_an_equals_sign_as_part_of_the_stream_specifier_like_ffmpeg() {
        // ffmpeg has no `-option=value` form: `-c:v=libx264` is `-c` for the
        // stream specifier `v=libx264`, and the next token is its argument.
        let error = parse_after_url_input(&["-c:v=libx264", "output.mp4"])
            .expect_err("output is consumed as the codec name");

        assert!(error.to_string().contains("no output file detected"));
    }

    #[test]
    fn does_not_consume_first_output_as_unknown_option_value() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "-copyts".to_string(),
            "first.mp4".to_string(),
            "second.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(
            parsed
                .outputs
                .iter()
                .map(|output| output.path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("first.mp4"), PathBuf::from("second.mp4")]
        );
        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "https://example.com/input.mov",
                "-copyts",
                "/tmp/cfmpeg/outputs/output_0.mp4",
                "/tmp/cfmpeg/outputs/output_1.mp4",
            ]
        );
    }

    #[test]
    fn does_not_consume_single_output_before_following_flag_as_unknown_option_value() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mp4".to_string(),
            "-copyts".to_string(),
            "output.mp4".to_string(),
            "-y".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].path, PathBuf::from("output.mp4"));
        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "https://example.com/input.mp4",
                "-copyts",
                "/tmp/cfmpeg/outputs/output_0.mp4",
                "-y",
            ]
        );
    }

    #[test]
    fn consumes_fpsmax_value_before_output() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mp4".to_string(),
            "-fpsmax".to_string(),
            "60".to_string(),
            "output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].path, PathBuf::from("output.mp4"));
        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "https://example.com/input.mp4",
                "-fpsmax",
                "60",
                "/tmp/cfmpeg/outputs/output_0.mp4",
            ]
        );
    }

    #[test]
    fn rejects_concat_filelist_inputs_for_remote_execution() {
        let dir = temp_path("concat-remote");
        fs::create_dir_all(&dir).expect("concat dir");
        let list_path = dir.join("files.txt");
        fs::write(&list_path, "file './one.mp4'\n").expect("concat file");

        let args = vec![
            "-f".to_string(),
            "concat".to_string(),
            "-i".to_string(),
            list_path.display().to_string(),
            "output.mp4".to_string(),
        ];

        let error = parse_ffmpeg_args(&args).expect_err("concat should be rejected");

        assert!(error.to_string().contains("concat file lists"));

        let _ = fs::remove_file(list_path);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_filter_complex_scripts_for_remote_execution() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mp4".to_string(),
            "-filter_complex_script".to_string(),
            "filters.txt".to_string(),
            "output.mp4".to_string(),
        ];

        let error = parse_ffmpeg_args(&args).expect_err("script should be rejected");

        assert!(error.to_string().contains("-filter_complex_script"));
    }

    #[test]
    fn rejects_stdin_pipe_and_device_inputs_for_remote_execution() {
        for input in ["-", "pipe:0", "/dev/video0"] {
            let args = vec![
                "-i".to_string(),
                input.to_string(),
                "output.mp4".to_string(),
            ];

            let error = parse_ffmpeg_args(&args).expect_err("special input should be rejected");

            assert!(
                error.to_string().contains("remote execution"),
                "unexpected error for {input}: {error}"
            );
        }
    }
}
