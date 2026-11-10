//! Output shapes a remote job cannot deliver yet.
//!
//! The worker returns exactly the files named on the command line, one per
//! output argument. Anything that writes to a stream, to no file at all, or to
//! more files than the one that was named is refused before a job is created.

use crate::error::{CfmpegError, Result};
use std::path::Path;

/// The options of one output that decide what ffmpeg writes for it.
#[derive(Debug, Default)]
pub struct OutputOptions {
    /// The muxer forced with `-f`, when one was given.
    pub format: Option<String>,
    /// Whether `-strftime` expands the output name.
    pub strftime: bool,
}

/// Returns an error when `output` cannot be produced by a remote job.
pub fn ensure_supported(output: &str, options: &OutputOptions) -> Result<()> {
    let extension = Path::new(output)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let muxer = options.format.as_deref().map(str::to_ascii_lowercase);
    // ffmpeg picks the muxer from the file name only when `-f` is absent.
    let guessed_muxer = match (muxer.as_deref(), extension.as_deref()) {
        (Some(muxer), _) => Some(muxer),
        (None, Some("m3u8")) => Some("hls"),
        (None, Some("mpd")) => Some("dash"),
        (None, Some("chk")) => Some("webm_chunk"),
        (None, _) => None,
    };

    let reason = if output == "-"
        || output.starts_with("pipe:")
        || output.starts_with("fd:")
        || output.starts_with("/dev/")
    {
        "standard output, pipes and device files cannot receive the output of a remote job"
    } else if output.contains("://") {
        "network destinations are not supported, only local output files"
    } else if options.strftime && output.contains('%') {
        "`-strftime` expands the name on the worker, so the files it writes are not the one named on the command line"
    } else if has_sequence_pattern(output) {
        "the name contains a number pattern such as `%03d`, so ffmpeg writes a series of files and only files named on the command line are returned"
    } else {
        match guessed_muxer {
            Some("hls") => {
                "HLS writes a playlist plus segment files, and only the playlist would be returned"
            }
            Some("dash") => {
                "DASH writes a manifest plus segment files, and only the manifest would be returned"
            }
            Some("segment" | "stream_segment" | "ssegment") => {
                "the segment muxers write a series of files instead of the one named on the command line"
            }
            Some("smoothstreaming" | "hds" | "webm_chunk") => {
                "this muxer writes a manifest plus fragment files, and only one file would be returned"
            }
            Some("tee") => "the tee muxer writes to several destinations listed inside the output argument",
            Some("null") => "the null muxer writes no file, so there is nothing to return",
            Some(_) | None => return Ok(()),
        }
    };

    Err(CfmpegError::UnsupportedOutput {
        output: output.to_string(),
        reason: reason.to_string(),
    })
}

/// Whether `name` holds a frame-number pattern (`%d`, `%5d`, `%03d`) as ffmpeg's
/// image and segment muxers expand it; `%%` is a literal percent sign.
fn has_sequence_pattern(name: &str) -> bool {
    let mut characters = name.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '%' {
            continue;
        }

        if characters.next_if_eq(&'%').is_some() {
            continue;
        }

        while characters.next_if(char::is_ascii_digit).is_some() {}

        if characters.peek() == Some(&'d') {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{ensure_supported, has_sequence_pattern, OutputOptions};
    use crate::error::CfmpegError;

    fn rejection(output: &str, format: Option<&str>) -> Option<String> {
        let options = OutputOptions {
            format: format.map(str::to_string),
            strftime: false,
        };

        match ensure_supported(output, &options) {
            Ok(()) => None,
            Err(CfmpegError::UnsupportedOutput { reason, .. }) => Some(reason),
            Err(error) => panic!("unexpected error for {output}: {error}"),
        }
    }

    fn assert_rejected(output: &str, format: Option<&str>, expected: &str) {
        let reason = rejection(output, format)
            .unwrap_or_else(|| panic!("{output} with -f {format:?} should be rejected"));

        assert!(
            reason.contains(expected),
            "{output} with -f {format:?}: unexpected reason `{reason}`"
        );
    }

    #[test]
    fn rejects_playlist_and_manifest_muxers() {
        assert_rejected("out/stream.m3u8", None, "HLS");
        assert_rejected("STREAM.M3U8", None, "HLS");
        assert_rejected("stream.txt", Some("hls"), "HLS");
        assert_rejected("manifest.mpd", None, "DASH");
        assert_rejected("manifest", Some("dash"), "DASH");
        assert_rejected("Manifest", Some("smoothstreaming"), "manifest");
        assert_rejected("out", Some("hds"), "manifest");
        assert_rejected("header.chk", None, "manifest");
        assert_rejected("header.hdr", Some("webm_chunk"), "manifest");
    }

    #[test]
    fn rejects_segment_and_tee_muxers() {
        for format in ["segment", "stream_segment", "ssegment"] {
            assert_rejected("part.mp4", Some(format), "segment");
        }
        assert_rejected("[f=mp4]a.mp4|[f=flv]b.flv", Some("tee"), "tee");
        assert_rejected("discard.mp4", Some("null"), "null");
    }

    #[test]
    fn rejects_sequence_patterns() {
        assert_rejected("thumb%03d.jpg", None, "number pattern");
        assert_rejected("thumb%d.png", Some("image2"), "number pattern");
        assert_rejected("frames/%5d.bmp", None, "number pattern");
        assert_rejected("part%03d.ts", Some("mpegts"), "number pattern");
    }

    #[test]
    fn rejects_strftime_expansion() {
        let options = OutputOptions {
            format: None,
            strftime: true,
        };

        let error = ensure_supported("shot-%Y%m%d-%H%M%S.jpg", &options).expect_err("strftime");

        assert!(error.to_string().contains("-strftime"));
        assert!(ensure_supported("shot.jpg", &options).is_ok());
    }

    #[test]
    fn rejects_stdout_pipes_devices_and_network_destinations() {
        for output in ["-", "pipe:", "pipe:1", "fd:3", "/dev/stdout", "/dev/null"] {
            assert_rejected(output, Some("mpegts"), "standard output");
        }
        for output in ["rtmp://live.example.com/app/key", "udp://239.0.0.1:1234"] {
            assert_rejected(output, Some("flv"), "network destinations");
        }
    }

    #[test]
    fn every_rejection_names_the_reason_and_suggests_local() {
        let error = ensure_supported("stream.m3u8", &OutputOptions::default()).expect_err("hls");

        assert_eq!(
            error.to_string(),
            "output `stream.m3u8` is not supported for remote execution yet: HLS writes a playlist plus segment files, and only the playlist would be returned; run this command with `--local`"
        );
    }

    #[test]
    fn accepts_plain_single_file_outputs() {
        for (output, format) in [
            ("output.mp4", None),
            ("exports/final cut.mov", None),
            ("poster.jpg", Some("image2")),
            ("poster.jpg", None),
            ("100%_done.mp4", None),
            ("50%%d.mp4", None),
            ("audio.m3u8", Some("mp4")),
            ("clip.ts", Some("mpegts")),
        ] {
            assert_eq!(rejection(output, format), None, "{output}");
        }
    }

    #[test]
    fn detects_only_frame_number_patterns() {
        assert!(has_sequence_pattern("%d"));
        assert!(has_sequence_pattern("a%010d.png"));
        assert!(!has_sequence_pattern("%%d"));
        assert!(!has_sequence_pattern("100%"));
        assert!(!has_sequence_pattern("%s.png"));
        assert!(!has_sequence_pattern("%Y-%m.png"));
    }
}
