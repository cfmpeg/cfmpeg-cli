//! Options and filters that depend on files beside the ffmpeg command.
//!
//! A remote worker receives the inputs named with `-i` and returns the output
//! files, nothing else. An option that reads another local file, or that
//! writes a side file, cannot work there and is refused before a job exists.

use crate::error::{CfmpegError, Result};

/// A filter option that names a local file.
struct FilterFile {
    filter: &'static str,
    /// Option names that hold the path.
    keys: &'static [&'static str],
    /// Whether the first unnamed argument is the path as well.
    first_argument: bool,
}

/// The filters whose listed options are always paths on the local machine.
const FILTER_FILES: &[FilterFile] = &[
    FilterFile {
        filter: "subtitles",
        keys: &["filename", "f", "fontsdir"],
        first_argument: true,
    },
    FilterFile {
        filter: "ass",
        keys: &["filename", "f", "fontsdir"],
        first_argument: true,
    },
    FilterFile {
        filter: "movie",
        keys: &["filename"],
        first_argument: true,
    },
    FilterFile {
        filter: "amovie",
        keys: &["filename"],
        first_argument: true,
    },
    FilterFile {
        filter: "lut3d",
        keys: &["file"],
        first_argument: true,
    },
    FilterFile {
        filter: "lut1d",
        keys: &["file"],
        first_argument: true,
    },
    FilterFile {
        filter: "drawtext",
        keys: &["fontfile", "textfile"],
        first_argument: true,
    },
    FilterFile {
        filter: "sofalizer",
        keys: &["sofa"],
        first_argument: true,
    },
    FilterFile {
        filter: "arnndn",
        keys: &["model", "m"],
        first_argument: true,
    },
    FilterFile {
        filter: "removelogo",
        keys: &["filename", "f"],
        first_argument: true,
    },
    FilterFile {
        filter: "sendcmd",
        keys: &["filename", "f"],
        first_argument: false,
    },
    FilterFile {
        filter: "asendcmd",
        keys: &["filename", "f"],
        first_argument: false,
    },
    FilterFile {
        filter: "curves",
        keys: &["psfile"],
        first_argument: false,
    },
];

/// Refuses an option that reads or writes a file the worker never exchanges.
///
/// `arg` is the option as written, `name` its bare name and `value` its
/// argument, when it takes one.
pub fn ensure_option_works_remotely(arg: &str, name: &str, value: Option<&str>) -> Result<()> {
    let reason = match name {
        "pass" | "passlogfile" => Some(
            "two-pass encoding keeps its statistics in a log file between two ffmpeg runs, and every remote job starts on an empty worker",
        ),
        "attach" => Some("it attaches a local file, which the worker never receives"),
        "fpre" | "pre" | "vpre" | "apre" | "spre" => {
            Some("it reads a local preset file, which the worker never receives")
        }
        "sdp_file" | "vstats" | "vstats_file" | "dump_attachment" | "stats_enc_pre"
        | "stats_enc_post" | "stats_mux_pre" | "print_graphs_file" => {
            Some("it writes a side file on the worker, and only the output files are returned")
        }
        // Progress sent to a pipe or a URL is not a file that would be lost.
        "progress"
            if value.is_some_and(|target| {
                target != "-" && !target.starts_with("pipe:") && !target.contains("://")
            }) =>
        {
            Some("it writes the progress to a file on the worker, and only the output files are returned")
        }
        _ => None,
    };

    if let Some(reason) = reason {
        return Err(CfmpegError::UnsupportedOption {
            option: arg.to_string(),
            reason: reason.to_string(),
        });
    }

    match (name, value) {
        ("vf" | "af" | "filter" | "filter_complex" | "lavfi", Some(graph)) => {
            ensure_graph_works_remotely(arg, graph)
        }
        _ => Ok(()),
    }
}

/// Refuses a filtergraph in which a filter reads a local file.
///
/// Only options that always hold a path count; a filter name inside a quoted
/// string or a label is not a filter.
pub fn ensure_graph_works_remotely(arg: &str, graph: &str) -> Result<()> {
    for filter in split_unquoted(graph, &[',', ';']) {
        let mut filter = filter.trim();
        // Input labels: `[0:v][logo]overlay`.
        while let Some((_, rest)) = filter
            .strip_prefix('[')
            .and_then(|labelled| labelled.split_once(']'))
        {
            filter = rest.trim_start();
        }
        // Output labels follow the arguments: `scale=2:2[out]`.
        let filter = split_unquoted(filter, &['['])
            .first()
            .copied()
            .unwrap_or("");
        let (name, arguments) = filter.split_once('=').unwrap_or((filter, ""));
        // `drawtext@title` is the filter `drawtext` with an instance name.
        let name = name.split_once('@').map_or(name, |(name, _)| name).trim();

        let Some(known) = FILTER_FILES.iter().find(|known| known.filter == name) else {
            continue;
        };

        for (index, argument) in split_unquoted(arguments, &[':']).into_iter().enumerate() {
            let named = split_unquoted(argument, &['=']);
            let path = match named.as_slice() {
                [key, ..] if named.len() > 1 => known
                    .keys
                    .contains(&key.trim())
                    .then(|| argument[key.len() + 1..].trim()),
                [unnamed] => (index == 0 && known.first_argument).then_some(unnamed.trim()),
                _ => None,
            };
            let Some(path) = path.map(|path| path.replace(['\'', '\\'], "")) else {
                continue;
            };

            // A URL is fetched by the worker itself.
            if !path.is_empty() && !path.contains("://") {
                return Err(CfmpegError::UnsupportedOption {
                    option: arg.to_string(),
                    reason: format!(
                        "the `{name}` filter reads the local path `{path}`, which the worker never receives"
                    ),
                });
            }
        }
    }

    Ok(())
}

/// Splits at the separators that are neither inside single quotes nor escaped
/// with a backslash, the way ffmpeg tokenises filtergraphs.
fn split_unquoted<'a>(text: &'a str, separators: &[char]) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '\'' {
            quoted = !quoted;
        } else if !quoted && separators.contains(&character) {
            parts.push(&text[start..index]);
            start = index + character.len_utf8();
        }
    }
    parts.push(&text[start..]);

    parts
}

#[cfg(test)]
mod tests {
    use super::{ensure_graph_works_remotely, ensure_option_works_remotely, split_unquoted};
    use crate::error::CfmpegError;

    fn refusal(name: &str, value: Option<&str>) -> Option<String> {
        match ensure_option_works_remotely(&format!("-{name}"), name, value) {
            Ok(()) => None,
            Err(CfmpegError::UnsupportedOption { reason, .. }) => Some(reason),
            Err(error) => panic!("unexpected error for -{name}: {error}"),
        }
    }

    #[test]
    fn refuses_options_that_read_or_write_side_files() {
        for (name, value) in [
            ("pass", Some("1")),
            ("passlogfile", Some("stats")),
            ("attach", Some("font.ttf")),
            ("fpre", Some("fast.ffpreset")),
            ("pre", Some("fast")),
            ("vpre", Some("fast")),
            ("apre", Some("fast")),
            ("spre", Some("fast")),
            ("sdp_file", Some("stream.sdp")),
            ("vstats_file", Some("stats.log")),
            ("vstats", None),
            ("dump_attachment", Some("font.ttf")),
            ("stats_enc_pre", Some("enc.log")),
            ("progress", Some("progress.txt")),
            ("progress", Some("/tmp/progress.txt")),
        ] {
            assert!(refusal(name, value).is_some(), "-{name} {value:?}");
        }
    }

    #[test]
    fn names_the_option_and_suggests_local() {
        let error = ensure_option_works_remotely("-pass:v", "pass", Some("2")).expect_err("pass");

        assert_eq!(
            error.to_string(),
            "`-pass:v` is not supported for remote execution yet: two-pass encoding keeps its statistics in a log file between two ffmpeg runs, and every remote job starts on an empty worker; run this command with `--local`"
        );
    }

    #[test]
    fn accepts_options_without_side_files() {
        for (name, value) in [
            ("c", Some("libx264")),
            ("preset", Some("slow")),
            ("profile", Some("high")),
            ("stats", None),
            ("progress", Some("-")),
            ("progress", Some("pipe:1")),
            ("progress", Some("https://example.com/progress")),
            ("map_metadata", Some("0")),
            ("vf", Some("scale=1280:-2")),
        ] {
            assert_eq!(refusal(name, value), None, "-{name} {value:?}");
        }
    }

    #[test]
    fn refuses_filters_that_read_local_files() {
        for (graph, filter, path) in [
            ("subtitles=subs.srt", "subtitles", "subs.srt"),
            (
                "subtitles=filename=subs.srt:force_style='FontSize=20'",
                "subtitles",
                "subs.srt",
            ),
            ("subtitles=f='my subs.ass'", "subtitles", "my subs.ass"),
            ("subtitles=in.mkv:fontsdir=fonts", "subtitles", "in.mkv"),
            ("ass=subs.ass", "ass", "subs.ass"),
            ("scale=1280:-2,subtitles=subs.srt", "subtitles", "subs.srt"),
            ("[0:v]subtitles=subs.srt[v]", "subtitles", "subs.srt"),
            (
                "movie=logo.png[wm];[0:v][wm]overlay=10:10",
                "movie",
                "logo.png",
            ),
            (
                "amovie=music.mp3:loop=0,asetpts=N/SR/TB",
                "amovie",
                "music.mp3",
            ),
            ("lut3d=grade.cube", "lut3d", "grade.cube"),
            (
                "lut3d=file=grade.cube:interp=tetrahedral",
                "lut3d",
                "grade.cube",
            ),
            ("lut3d='grade.cube'", "lut3d", "grade.cube"),
            ("lut1d=file=curve.cube", "lut1d", "curve.cube"),
            (
                "drawtext=fontfile=/Library/Fonts/Arial.ttf:text=hi",
                "drawtext",
                "/Library/Fonts/Arial.ttf",
            ),
            (
                "drawtext=text=credits:textfile=credits.txt",
                "drawtext",
                "credits.txt",
            ),
            ("drawtext@title=fontfile=a.ttf:text=hi", "drawtext", "a.ttf"),
            (
                "sofalizer=sofa=/hrtf/default.sofa",
                "sofalizer",
                "/hrtf/default.sofa",
            ),
            ("arnndn=m=speech.rnnn", "arnndn", "speech.rnnn"),
            ("arnndn=model=speech.rnnn", "arnndn", "speech.rnnn"),
            ("removelogo=logo-mask.png", "removelogo", "logo-mask.png"),
            ("sendcmd=f=commands.txt", "sendcmd", "commands.txt"),
            ("curves=psfile=grade.acv", "curves", "grade.acv"),
            (
                "[0:v]scale=640:-2[bg];[bg]subtitles=C\\:/subs/a.srt[out]",
                "subtitles",
                "C:/subs/a.srt",
            ),
        ] {
            let error = ensure_graph_works_remotely("-vf", graph).expect_err(graph);

            assert_eq!(
                error.to_string(),
                format!(
                    "`-vf` is not supported for remote execution yet: the `{filter}` filter reads the local path `{path}`, which the worker never receives; run this command with `--local`"
                ),
                "{graph}"
            );
        }
    }

    #[test]
    fn accepts_filters_that_only_mention_those_names() {
        for graph in [
            "scale=1280:-2",
            "scale=1280:-2,fps=30,format=yuv420p",
            // text and labels are not filters
            "drawtext=text='subtitles=subs.srt':font=Sans",
            "drawtext=font=Sans:text='movie=logo.png, lut3d=a.cube':fontsize=24",
            "[0:v]scale=2:2[movie];[movie][1:v]overlay[ass]",
            // the same filters without a file
            "lut3d",
            "lut3d=interp=trilinear",
            "curves=preset=vintage",
            "curves=vintage",
            "sendcmd=c='0.0 drawtext reinit text=hi'",
            "drawtext=text=hi:font=Sans:fontcolor=white",
            // other filters whose names contain a listed name
            "bass=g=3",
            "amix=inputs=2,asubboost",
            "lut3dx=file=a.cube",
            // escaped separators stay inside one argument
            "select='eq(n\\,0)',scale=640:-2",
            "drawtext=text=a\\:b\\,subtitles=c",
            // the worker can fetch a URL itself
            "movie=https\\://example.com/logo.png[wm];[0:v][wm]overlay",
            "",
        ] {
            assert!(
                ensure_graph_works_remotely("-vf", graph).is_ok(),
                "{graph} should be accepted"
            );
        }
    }

    #[test]
    fn checks_every_option_that_carries_a_filtergraph() {
        for name in ["vf", "af", "filter", "filter_complex", "lavfi"] {
            assert!(
                refusal(name, Some("[0:v]subtitles=subs.srt[v]")).is_some(),
                "-{name}"
            );
        }
        assert_eq!(
            refusal("metadata", Some("comment=subtitles=subs.srt")),
            None
        );
    }

    #[test]
    fn splits_outside_quotes_and_escapes_only() {
        assert_eq!(split_unquoted("a,b;c", &[',', ';']), vec!["a", "b", "c"]);
        assert_eq!(split_unquoted("a='b,c',d", &[',']), vec!["a='b,c'", "d"]);
        assert_eq!(split_unquoted("a\\,b,c", &[',']), vec!["a\\,b", "c"]);
        assert_eq!(split_unquoted("", &[',']), vec![""]);
    }
}
