//! Which ffmpeg command line options exist and whether they take an argument.
//!
//! The lookup follows ffmpeg's own `split_commandline`: ffmpeg's options are
//! tried first, then AVOptions (codec, format, scaler and resampler options),
//! then the `-no<name>` spelling of boolean options. The tables are generated
//! by `scripts/generate-ffmpeg-options.py` and checked in.

mod table;

use table::{AV_OPTIONS, NATIVE_OPTIONS};

/// One of ffmpeg's own options, as declared in its option table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeKind {
    /// Takes no argument and has no `-no<name>` spelling (`-vstats`).
    Flag,
    /// Takes no argument; `-no<name>` switches it off (`-y`, `-stdin`).
    Bool,
    /// Takes one argument (`-c`, `-map`).
    Value,
    /// Prints information and exits (`-version`, `-encoders`).
    Info,
}

/// How far an AVOption reaches; every AVOption takes one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AvScope {
    /// An `AVCodecContext` option: accepts a `:stream_spec` suffix and a
    /// `v`, `a` or `s` prefix (`-b:v`, `-vb`).
    CodecGeneric,
    /// A private option of one codec: accepts a `:stream_spec` suffix (`-crf:v`).
    CodecPrivate,
    /// A format, protocol, scaler or resampler option: exact name only (`-movflags`).
    Exact,
}

/// How an option consumes the tokens that follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionSyntax {
    /// Takes no argument.
    Flag,
    /// Takes the next token as its argument.
    Value,
    /// Reads its argument from the file named by the next token (`-/filter:v graph.txt`).
    ValueFromFile,
    /// Prints information and exits instead of transcoding.
    Info,
}

/// An option token that ffmpeg would accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownOption<'a> {
    /// The option name without the leading dash, `no` prefix or stream specifier.
    pub name: &'a str,
    pub syntax: OptionSyntax,
    /// Whether a boolean option was written as `-no<name>`.
    pub negated: bool,
}

/// Classifies an option token such as `-c:v` or `-nostdin`.
///
/// Returns `None` when ffmpeg 8 would answer "Unrecognized option".
pub fn classify(arg: &str) -> Option<KnownOption<'_>> {
    let option = arg.strip_prefix('-')?;
    let (from_file, option) = match option.strip_prefix('/') {
        Some(option) => (true, option),
        None => (false, option),
    };
    let base = option.split_once(':').map_or(option, |(base, _)| base);

    if let Some(kind) = find(NATIVE_OPTIONS, base) {
        let syntax = match (kind, from_file) {
            (NativeKind::Value, false) => OptionSyntax::Value,
            (NativeKind::Value, true) => OptionSyntax::ValueFromFile,
            (NativeKind::Flag | NativeKind::Bool, false) => OptionSyntax::Flag,
            (NativeKind::Info, false) => OptionSyntax::Info,
            (NativeKind::Flag | NativeKind::Bool | NativeKind::Info, true) => return None,
        };

        return Some(KnownOption {
            name: base,
            syntax,
            negated: false,
        });
    }

    if from_file {
        return None;
    }

    let av_option = match find(AV_OPTIONS, base) {
        Some(AvScope::CodecGeneric | AvScope::CodecPrivate) => true,
        Some(AvScope::Exact) => base.len() == option.len(),
        None => false,
    };
    let media_prefixed = option
        .strip_prefix(['v', 'a', 's'])
        .is_some_and(|generic| find(AV_OPTIONS, generic) == Some(AvScope::CodecGeneric));

    if av_option || media_prefixed {
        return Some(KnownOption {
            name: base,
            syntax: OptionSyntax::Value,
            negated: false,
        });
    }

    let positive = option.strip_prefix("no")?;
    let positive = positive.split_once(':').map_or(positive, |(base, _)| base);

    (find(NATIVE_OPTIONS, positive) == Some(NativeKind::Bool)).then_some(KnownOption {
        name: positive,
        syntax: OptionSyntax::Flag,
        negated: true,
    })
}

fn find<K: Copy>(table: &[(&str, K)], name: &str) -> Option<K> {
    table
        .binary_search_by(|(candidate, _)| candidate.cmp(&name))
        .ok()
        .map(|index| table[index].1)
}

#[cfg(test)]
mod tests {
    use super::table::{AV_OPTIONS, NATIVE_OPTIONS};
    use super::{classify, KnownOption, OptionSyntax};

    fn syntax(arg: &str) -> Option<OptionSyntax> {
        classify(arg).map(|option| option.syntax)
    }

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert!(NATIVE_OPTIONS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(AV_OPTIONS.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn classifies_options_that_take_an_argument() {
        for arg in [
            // ffmpeg's own options, with and without stream specifiers
            "-c",
            "-c:v",
            "-c:v:0",
            "-b:a:0",
            "-filter:v",
            "-metadata:s:a:0",
            "-disposition:a:1",
            "-map",
            "-ss",
            "-t",
            "-s",
            "-s:v",
            "-force_key_frames",
            "-force_key_frames:v",
            "-fps_mode",
            "-fps_mode:v",
            "-thread_queue_size",
            "-filter_complex",
            "-dec",
            // generic codec options
            "-strict",
            "-strict:a",
            "-g",
            "-bufsize:v",
            "-vb",
            "-ab",
            // private codec options
            "-x264-params",
            "-x265-params",
            "-x265-params:v",
            "-crf",
            "-crf:v:0",
            "-preset",
            "-tune",
            "-cpu-used",
            "-svtav1-params",
            // format, protocol, scaler and resampler options
            "-probesize",
            "-analyzeduration",
            "-movflags",
            "-hls_flags",
            "-hls_time",
            "-segment_time",
            "-fflags",
            "-sws_flags",
            "-user_agent",
        ] {
            assert_eq!(syntax(arg), Some(OptionSyntax::Value), "{arg}");
        }
    }

    #[test]
    fn knows_options_that_only_the_worker_build_has() {
        for arg in [
            // libaom-av1
            "-aom-params",
            "-tiles",
            "-tiles:v",
            // libopenh264, libvorbis, libwebp, libjxl
            "-allow_skip_frames",
            "-iblock",
            "-cr_threshold",
            "-effort",
            // hardware device options that a macOS build lacks
            "-vaapi_device",
            "-qsv_device",
        ] {
            assert_eq!(syntax(arg), Some(OptionSyntax::Value), "{arg}");
        }
    }

    #[test]
    fn classifies_options_that_take_no_argument() {
        for arg in [
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
            "-stats",
            "-report",
            "-vstats",
            "-autorotate",
            "-nostdin",
            "-nostats",
            "-noautorotate",
            "-noautorotate:v",
            "-nohide_banner",
        ] {
            assert_eq!(syntax(arg), Some(OptionSyntax::Flag), "{arg}");
        }
    }

    #[test]
    fn reports_the_bare_name_and_negation() {
        assert_eq!(
            classify("-nostdin"),
            Some(KnownOption {
                name: "stdin",
                syntax: OptionSyntax::Flag,
                negated: true,
            })
        );
        assert_eq!(
            classify("-filter_script:v"),
            Some(KnownOption {
                name: "filter_script",
                syntax: OptionSyntax::Value,
                negated: false,
            })
        );
    }

    #[test]
    fn classifies_information_options() {
        for arg in ["-version", "-encoders", "-h", "-formats", "--help"] {
            assert_eq!(syntax(arg), Some(OptionSyntax::Info), "{arg}");
        }
    }

    #[test]
    fn classifies_arguments_loaded_from_a_file() {
        assert_eq!(syntax("-/filter:v"), Some(OptionSyntax::ValueFromFile));
        assert_eq!(
            syntax("-/filter_complex"),
            Some(OptionSyntax::ValueFromFile)
        );
        // Only ffmpeg's own valued options can be loaded from a file.
        assert_eq!(syntax("-/y"), None);
        assert_eq!(syntax("-/crf"), None);
    }

    #[test]
    fn rejects_what_ffmpeg_does_not_recognise() {
        for arg in [
            "-foo",
            "-nofoo",
            "--codecs",
            "-crf=23",
            // format options take no stream specifier
            "-movflags:v",
            "-sws_flags:v",
            // only booleans have a `no` spelling
            "-novstats",
            "-noc",
            // the media prefix is for generic codec options only
            "-vcrf",
            "-vmovflags",
        ] {
            assert_eq!(syntax(arg), None, "{arg}");
        }
    }

    #[test]
    fn accepts_every_option_of_the_hand_written_list() {
        // The value-taking option families cfmpeg <= 0.1.6 knew.
        for name in [
            "ab",
            "ac",
            "acodec",
            "af",
            "aframes",
            "ar",
            "aspect",
            "aq",
            "atag",
            "b",
            "bsf",
            "bufsize",
            "c",
            "canvas_size",
            "ch_layout",
            "channel_layout",
            "codec",
            "color_primaries",
            "colorspace",
            "color_trc",
            "crf",
            "dcodec",
            "disposition",
            "f",
            "fflags",
            "filter",
            "filter_complex",
            "filter_complex_script",
            "filter_script",
            "filter_threads",
            "framerate",
            "frames",
            "fpsmax",
            "fs",
            "g",
            "guess_layout_max",
            "hls_list_size",
            "hls_segment_filename",
            "hls_time",
            "hwaccel",
            "hwaccel_device",
            "hwaccel_output_format",
            "itsscale",
            "itsoffset",
            "keyint_min",
            "lavfi",
            "level",
            "loglevel",
            "map",
            "map_chapters",
            "map_metadata",
            "max_muxing_queue_size",
            "maxrate",
            "metadata",
            "minrate",
            "movflags",
            "muxdelay",
            "muxpreload",
            "pass",
            "passlogfile",
            "pix_fmt",
            "preset",
            "pre",
            "profile",
            "q",
            "qscale",
            "qp",
            "r",
            "rc-lookahead",
            "readrate",
            "readrate_catchup",
            "readrate_initial_burst",
            "s",
            "sample_fmt",
            "sc_threshold",
            "scodec",
            "segment_list",
            "segment_time",
            "spatial-aq",
            "spre",
            "ss",
            "sseof",
            "start_number",
            "stats_period",
            "stream_loop",
            "t",
            "tag",
            "temporal-aq",
            "threads",
            "thread_queue_size",
            "timecode",
            "timestamp",
            "to",
            "tune",
            "vcodec",
            "vf",
            "vframes",
            "vpre",
            "vtag",
            "video_size",
            "vsync",
        ] {
            assert_eq!(
                syntax(&format!("-{name}")),
                Some(OptionSyntax::Value),
                "-{name}"
            );
        }
    }
}
