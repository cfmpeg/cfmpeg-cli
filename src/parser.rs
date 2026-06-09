use crate::error::{CfmpegError, Result};
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

#[derive(Debug)]
pub struct ParsedCommand {
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub sandbox_args: Vec<String>,
}

// ffmpeg allows many options to carry per-stream suffixes like `:v:0`.
// Matching option families keeps the parser table smaller and avoids
// duplicating every stream-specific spelling by hand.
const VALUED_OPTION_FAMILIES: &[&str] = &[
    "-ab",
    "-ac",
    "-acodec",
    "-af",
    "-aframes",
    "-ar",
    "-aspect",
    "-aq",
    "-atag",
    "-b",
    "-bsf",
    "-bufsize",
    "-c",
    "-canvas_size",
    "-ch_layout",
    "-channel_layout",
    "-codec",
    "-color_primaries",
    "-colorspace",
    "-color_trc",
    "-crf",
    "-dcodec",
    "-disposition",
    "-f",
    "-fflags",
    "-filter",
    "-filter_complex",
    "-filter_complex_script",
    "-filter_script",
    "-filter_threads",
    "-framerate",
    "-frames",
    "-fs",
    "-g",
    "-guess_layout_max",
    "-hls_list_size",
    "-hls_segment_filename",
    "-hls_time",
    "-hwaccel",
    "-hwaccel_device",
    "-hwaccel_output_format",
    "-itsscale",
    "-itsoffset",
    "-keyint_min",
    "-lavfi",
    "-level",
    "-loglevel",
    "-map",
    "-map_chapters",
    "-map_metadata",
    "-max_muxing_queue_size",
    "-maxrate",
    "-metadata",
    "-minrate",
    "-movflags",
    "-muxdelay",
    "-muxpreload",
    "-pass",
    "-passlogfile",
    "-pix_fmt",
    "-preset",
    "-pre",
    "-profile",
    "-q",
    "-qscale",
    "-qp",
    "-r",
    "-rc-lookahead",
    "-readrate",
    "-readrate_catchup",
    "-readrate_initial_burst",
    "-s",
    "-sample_fmt",
    "-sc_threshold",
    "-scodec",
    "-segment_list",
    "-segment_time",
    "-spatial-aq",
    "-spre",
    "-ss",
    "-sseof",
    "-start_number",
    "-stats_period",
    "-stream_loop",
    "-t",
    "-tag",
    "-temporal-aq",
    "-threads",
    "-thread_queue_size",
    "-timecode",
    "-timestamp",
    "-to",
    "-tune",
    "-vcodec",
    "-vf",
    "-vframes",
    "-vpre",
    "-vtag",
    "-video_size",
    "-vsync",
];

const FLAG_OPTIONS: &[&str] = &[
    "-accurate_seek",
    "-an",
    "-benchmark",
    "-copy_unknown",
    "-dn",
    "-dump",
    "-hide_banner",
    "-hex",
    "-ignore_unknown",
    "-n",
    "-noaccurate_seek",
    "-nostats",
    "-nostdin",
    "-re",
    "-shortest",
    "-sn",
    "-vn",
    "-y",
];

pub fn parse_ffmpeg_args(args: &[String]) -> Result<ParsedCommand> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut sandbox_args = Vec::new();
    let mut input_counter = 0usize;
    let mut output_counter = 0usize;

    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];

        if arg == "-i" {
            sandbox_args.push(arg.clone());
            index += 1;

            let input_arg = args.get(index).ok_or_else(|| {
                CfmpegError::ParseError("-i flag requires an input path or URL".to_string())
            })?;

            let input = classify_input(input_arg)?;
            sandbox_args.push(rewrite_input(&input, input_counter));
            if matches!(input, Input::LocalFile { .. }) {
                input_counter += 1;
            }
            inputs.push(input);
            index += 1;
            continue;
        }

        if is_valued_option(arg) {
            sandbox_args.push(arg.clone());

            if has_inline_option_value(arg) {
                index += 1;
                continue;
            }

            index += 1;

            let value = args
                .get(index)
                .ok_or_else(|| CfmpegError::ParseError(format!("{arg} requires an argument")))?;
            sandbox_args.push(value.clone());
            index += 1;
            continue;
        }

        if is_flag_option(arg) {
            sandbox_args.push(arg.clone());
            index += 1;
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            sandbox_args.push(arg.clone());

            if should_consume_unknown_option_value(args, index) {
                sandbox_args.push(args[index + 1].clone());
                index += 2;
                continue;
            }

            index += 1;
            continue;
        }

        let output_path = PathBuf::from(arg);
        let remote_name = build_remote_output_name(&output_path, output_counter);
        sandbox_args.push(rewrite_output(&remote_name));
        outputs.push(Output {
            path: output_path,
            remote_name,
        });
        output_counter += 1;
        index += 1;
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

    Ok(ParsedCommand {
        inputs,
        outputs,
        sandbox_args,
    })
}

#[cfg(test)]
pub fn parse_concat_filelist(filelist_path: &Path) -> Result<Vec<PathBuf>> {
    let contents = std::fs::read_to_string(filelist_path).map_err(|error| {
        CfmpegError::ParseError(format!(
            "failed to read concat file list {}: {error}",
            filelist_path.display()
        ))
    })?;
    let base_dir = filelist_path.parent().unwrap_or_else(|| Path::new("."));
    let mut paths = Vec::new();

    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("file ") {
            let path = rest.trim().trim_matches('\'').trim_matches('"');
            let path = PathBuf::from(path);
            let path = if path.is_relative() {
                base_dir.join(path)
            } else {
                path
            };
            paths.push(path);
        }
    }

    Ok(paths)
}

fn classify_input(path: &str) -> Result<Input> {
    if is_remote_url(path) {
        return Ok(Input::Url(path.to_string()));
    }

    if is_special_input(path) {
        return Ok(Input::Special(path.to_string()));
    }

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
    path == "-"
        || path.starts_with("pipe:")
        || path.starts_with("/dev/")
        || path.starts_with("lavfi:")
        || path.starts_with("color=")
        || path.starts_with("testsrc")
        || path.starts_with("anullsrc")
        || path.starts_with("nullsrc")
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

fn is_valued_option(arg: &str) -> bool {
    VALUED_OPTION_FAMILIES
        .iter()
        .any(|family| matches_option_family(arg, family))
}

fn is_flag_option(arg: &str) -> bool {
    FLAG_OPTIONS.contains(&option_name(arg))
}

fn matches_option_family(arg: &str, family: &str) -> bool {
    let option = option_name(arg);
    option == family
        || option
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with(':'))
}

fn option_name(arg: &str) -> &str {
    arg.split_once('=').map(|(option, _)| option).unwrap_or(arg)
}

fn has_inline_option_value(arg: &str) -> bool {
    arg.split_once('=')
        .is_some_and(|(option, _)| option.starts_with('-'))
}

fn should_consume_unknown_option_value(args: &[String], index: usize) -> bool {
    let Some(next_arg) = args.get(index + 1) else {
        return false;
    };

    if has_inline_option_value(&args[index])
        || next_arg.starts_with('-')
        || index + 1 >= args.len() - 1
    {
        return false;
    }

    !starts_multi_positional_run(args, index + 1)
}

fn starts_multi_positional_run(args: &[String], start: usize) -> bool {
    let mut count = 0usize;

    for arg in &args[start..] {
        if arg.starts_with('-') {
            break;
        }

        count += 1;
        if count > 1 {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{parse_concat_filelist, parse_ffmpeg_args, Input};
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cfmpeg-parser-{name}-{}", Uuid::new_v4()))
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
    fn parses_concat_file_lists_relative_to_the_list_path() {
        let dir = temp_path("concat-dir");
        fs::create_dir_all(&dir).expect("concat dir");

        let list_path = dir.join("files.txt");
        fs::write(&list_path, "file './one.mp4'\n# comment\nfile two.mp4\n").expect("concat file");

        let paths = parse_concat_filelist(&list_path).expect("concat paths");

        assert_eq!(paths, vec![dir.join("./one.mp4"), dir.join("two.mp4")]);

        let _ = fs::remove_file(list_path);
        let _ = fs::remove_dir_all(dir);
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
    fn keeps_output_when_known_option_uses_inline_value_syntax() {
        let args = vec![
            "-i".to_string(),
            "https://example.com/input.mov".to_string(),
            "-c:v=libx264".to_string(),
            "output.mp4".to_string(),
        ];

        let parsed = parse_ffmpeg_args(&args).expect("parsed command");

        assert_eq!(parsed.outputs.len(), 1);
        assert_eq!(parsed.outputs[0].path, PathBuf::from("output.mp4"));
        assert_eq!(
            parsed.sandbox_args,
            vec![
                "-i",
                "https://example.com/input.mov",
                "-c:v=libx264",
                "/tmp/cfmpeg/outputs/output_0.mp4",
            ]
        );
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
}
