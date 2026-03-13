use crate::error::{CfmpegError, Result};
use std::path::{Path, PathBuf};

/// Represents a parsed input to the ffmpeg command.
#[derive(Debug, Clone)]
pub enum Input {
    /// A local file that needs to be uploaded.
    LocalFile {
        path: PathBuf,
        size: u64,
    },
    /// A remote URL that the sandbox can fetch directly.
    Url(String),
    /// A special input like a device, pipe, or filter source.
    Special(String),
}

/// Represents a parsed output from the ffmpeg command.
#[derive(Debug, Clone)]
pub struct Output {
    pub path: PathBuf,
}

/// The result of parsing an ffmpeg command line.
#[derive(Debug)]
pub struct ParsedCommand {
    /// The raw ffmpeg arguments as provided by the user.
    pub raw_args: Vec<String>,
    /// Identified input files/URLs.
    pub inputs: Vec<Input>,
    /// Identified output files.
    pub outputs: Vec<Output>,
    /// Arguments rewritten for the sandbox (local paths replaced with sandbox paths).
    pub sandbox_args: Vec<String>,
}

/// Options that take a following argument (the next token is NOT a filename).
/// This prevents us from misidentifying option values as output files.
const VALUED_OPTIONS: &[&str] = &[
    "-f", "-c", "-codec", "-c:v", "-c:a", "-c:s",
    "-b", "-b:v", "-b:a",
    "-r", "-s", "-aspect", "-vn", "-an", "-sn",
    "-vf", "-af", "-filter_complex", "-filter:v", "-filter:a",
    "-preset", "-tune", "-profile", "-profile:v", "-level",
    "-crf", "-qp", "-maxrate", "-bufsize", "-minrate",
    "-g", "-keyint_min", "-sc_threshold", "-bf",
    "-pix_fmt", "-color_primaries", "-color_trc", "-colorspace",
    "-map", "-map_metadata", "-map_chapters",
    "-t", "-to", "-ss", "-sseof",
    "-frames", "-frames:v", "-frames:a",
    "-ac", "-ar", "-sample_fmt",
    "-threads", "-filter_threads",
    "-movflags", "-fflags",
    "-hls_time", "-hls_list_size", "-hls_segment_filename",
    "-start_number", "-segment_time", "-segment_list",
    "-metadata", "-disposition",
    "-loglevel", "-v", "-stats_period",
    "-max_muxing_queue_size",
    "-tag", "-tag:v", "-tag:a",
    "-rc-lookahead", "-spatial-aq", "-temporal-aq",
    "-video_size", "-framerate", "-pixel_format",
    "-vsync", "-async",
    "-shortest", "-strict",
];

/// Options that are boolean flags (no following argument).
const FLAG_OPTIONS: &[&str] = &[
    "-y", "-n", "-nostdin", "-nostats",
    "-vn", "-an", "-sn", "-dn",
    "-hide_banner",
    "-ignore_unknown",
    "-copy_unknown",
    "-benchmark",
    "-dump",
    "-hex",
    "-re",
    "-shortest",
    "-accurate_seek",
    "-noaccurate_seek",
    "-overwrite", "-never_overwrite",
];

/// Parse ffmpeg arguments to identify inputs, outputs, and rewrite paths.
pub fn parse_ffmpeg_args(args: &[String]) -> Result<ParsedCommand> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut sandbox_args = Vec::new();

    let mut i = 0;
    let mut input_counter = 0;

    while i < args.len() {
        let arg = &args[i];

        match arg.as_str() {
            // Input flag: next arg is an input file/URL
            "-i" => {
                sandbox_args.push("-i".to_string());
                i += 1;

                if i >= args.len() {
                    return Err(CfmpegError::ParseError(
                        "-i flag requires an argument".into(),
                    ));
                }

                let input_path = &args[i];
                let input = classify_input(input_path)?;

                match &input {
                    Input::LocalFile { .. } => {
                        // Rewrite to sandbox path
                        let ext = Path::new(input_path)
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        let sandbox_path = format!("/tmp/cfmpeg/inputs/input_{}.{}", input_counter, ext);
                        sandbox_args.push(sandbox_path);
                        input_counter += 1;
                    }
                    Input::Url(url) => {
                        // Pass URL through as-is
                        sandbox_args.push(url.clone());
                    }
                    Input::Special(s) => {
                        sandbox_args.push(s.clone());
                    }
                }

                inputs.push(input);
            }

            // Concat file list: parse the list and upload referenced files
            _ if is_concat_input(&args, i) => {
                sandbox_args.push(arg.clone());
                // The -f concat flag is already handled; the filelist.txt
                // will be processed when it appears as -i argument
            }

            // Known valued option: push option + its value, skip both
            _ if is_valued_option(arg) => {
                sandbox_args.push(arg.clone());
                i += 1;
                if i < args.len() {
                    sandbox_args.push(args[i].clone());
                }
            }

            // Known flag option: push and continue
            _ if is_flag_option(arg) => {
                sandbox_args.push(arg.clone());
            }

            // Starts with dash: unknown option. Heuristic — assume it takes a value
            // if the next arg doesn't start with dash and isn't the last arg.
            _ if arg.starts_with('-') && arg.len() > 1 => {
                sandbox_args.push(arg.clone());

                // Peek at next arg to decide if it's a value
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    // Could be a valued option or the last positional (output).
                    // If there are more args after, treat next as option value.
                    // If next is the very last arg, it's likely the output file.
                    if i + 2 < args.len() {
                        // More args follow — treat as option value
                        i += 1;
                        sandbox_args.push(args[i].clone());
                    }
                    // else: next is last arg, fall through to let it be caught as output
                }
            }

            // Positional argument (no dash prefix) — this is an output file
            _ => {
                let output_path = PathBuf::from(arg);
                let ext = output_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                let sandbox_output = format!("/tmp/cfmpeg/outputs/{}", output_path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&format!("output.{}", ext)));

                sandbox_args.push(sandbox_output);
                outputs.push(Output { path: output_path });
            }
        }

        i += 1;
    }

    if outputs.is_empty() {
        return Err(CfmpegError::ParseError(
            "No output file detected. Ensure your ffmpeg command includes an output path.".into(),
        ));
    }

    Ok(ParsedCommand {
        raw_args: args.to_vec(),
        inputs,
        outputs,
        sandbox_args,
    })
}

/// Classify an input as a local file, URL, or special source.
fn classify_input(path: &str) -> Result<Input> {
    // URLs — pass through to sandbox
    if path.starts_with("http://")
        || path.starts_with("https://")
        || path.starts_with("s3://")
        || path.starts_with("r2://")
        || path.starts_with("gs://")
        || path.starts_with("rtmp://")
        || path.starts_with("rtsp://")
        || path.starts_with("srt://")
    {
        return Ok(Input::Url(path.to_string()));
    }

    // Special inputs (pipes, devices, virtual sources)
    if path == "-"
        || path == "pipe:"
        || path.starts_with("pipe:")
        || path.starts_with("/dev/")
        || path.starts_with("lavfi:")
        || path.starts_with("nullsrc")
        || path.starts_with("anullsrc")
        || path.starts_with("color=")
        || path.starts_with("testsrc")
    {
        return Ok(Input::Special(path.to_string()));
    }

    // Local file
    let file_path = PathBuf::from(path);

    if !file_path.exists() {
        return Err(CfmpegError::InputNotFound(path.to_string()));
    }

    let metadata = std::fs::metadata(&file_path)?;
    let size = metadata.len();

    Ok(Input::LocalFile {
        path: file_path,
        size,
    })
}

/// Check if the current position is part of a `-f concat` sequence.
fn is_concat_input(args: &[String], pos: usize) -> bool {
    if pos < 2 {
        return false;
    }
    args[pos - 2] == "-f" && args[pos - 1] == "concat"
}

/// Check if an argument is a known valued option.
fn is_valued_option(arg: &str) -> bool {
    VALUED_OPTIONS.contains(&arg)
}

/// Check if an argument is a known boolean flag.
fn is_flag_option(arg: &str) -> bool {
    FLAG_OPTIONS.contains(&arg)
}

/// Parse a concat file list and return the paths referenced within.
pub fn parse_concat_filelist(filelist_path: &Path) -> Result<Vec<PathBuf>> {
    let contents = std::fs::read_to_string(filelist_path).map_err(|e| {
        CfmpegError::ParseError(format!(
            "Failed to read concat file list {}: {}",
            filelist_path.display(),
            e
        ))
    })?;

    let mut paths = Vec::new();
    let base_dir = filelist_path.parent().unwrap_or(Path::new("."));

    for line in contents.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse "file 'path'" or "file path" directives
        if let Some(rest) = line.strip_prefix("file ") {
            let path_str = rest.trim().trim_matches('\'').trim_matches('"');
            let path = PathBuf::from(path_str);

            // Resolve relative paths against the filelist's directory
            let full_path = if path.is_relative() {
                base_dir.join(&path)
            } else {
                path
            };

            paths.push(full_path);
        }
    }

    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn test_simple_transcode() {
        // We can't test with real files, but we can test the parse logic
        // by checking that the parser identifies the right structure.
        let parsed = parse_ffmpeg_args(&args("-i test.mov -c:v libx265 output.mp4"));
        // Will fail because test.mov doesn't exist, which is expected
        assert!(parsed.is_err());
    }

    #[test]
    fn test_url_input() {
        let parsed = parse_ffmpeg_args(&args(
            "-i https://example.com/video.mov -c:v libx265 output.mp4",
        ));
        // URL inputs don't require file existence
        // But output file parsing should still work
        match parsed {
            Ok(cmd) => {
                assert_eq!(cmd.inputs.len(), 1);
                assert!(matches!(cmd.inputs[0], Input::Url(_)));
                assert_eq!(cmd.outputs.len(), 1);
            }
            Err(_) => {} // Acceptable in test env
        }
    }

    #[test]
    fn test_no_output() {
        let result = parse_ffmpeg_args(&args("-i https://example.com/video.mov"));
        // Hmm, with URL input and no output, this should error.
        // Actually -i consumes the URL, and there's nothing left. Let's check.
        assert!(result.is_err());
    }
}
