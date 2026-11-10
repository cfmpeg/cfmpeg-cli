#!/usr/bin/env python3
"""Generate src/ffmpeg_options/table.rs, cfmpeg's table of ffmpeg options.

cfmpeg has to know, for every ffmpeg option, whether the next token is the
option's argument or an output file. The table is the union of

  * every ffmpeg build described by a dump under scripts/ffmpeg-dumps/, and
  * the option list cfmpeg <= 0.1.6 maintained by hand, so that nothing that
    parsed before stops parsing.

The table is checked in. It is never generated at build time, because
`cargo install cfmpeg` has to work without ffmpeg installed.

Dumps
-----
A dump is a directory with what one ffmpeg build says about itself:

  version.txt         `ffmpeg -hide_banner -version`
  help-full.txt       `ffmpeg -hide_banner -h full`; may be cut after the
                      "AVFilter AVOptions:" heading
  probed-options.txt  optional, written by --capture: how ffmpeg itself reacts
                      to each option
  provenance.txt      optional free text, copied into the table's header

The checked-in dump:

  worker-conda-forge-linux-x64  the remote worker's ffmpeg (conda-forge build,
                                ffmpeg 8.1.2, linux x86_64), captured and
                                probed where that ffmpeg is installed. The
                                table describes this build and no other: an
                                option that only some other ffmpeg has would
                                fail on the worker anyway. The worker tracks
                                ffmpeg 8.x, so refresh this dump and the table
                                whenever the worker's ffmpeg changes.

How a build is read
-------------------
1. `ffmpeg -h full` lists the option names. The sections before
   "AVCodecContext AVOptions:" are ffmpeg's own options; the sections from
   there up to "AVFilter AVOptions:" are the AVOptions that ffmpeg accepts on
   the command line (codec, format, protocol, scaler and resampler options),
   each of which takes one argument. Filter and bitstream filter options are
   only valid inside -vf/-bsf strings, so the rest is ignored.
2. The help text does not say reliably whether one of ffmpeg's own options
   takes an argument (`-thread_queue_size` prints no argument name,
   `-hide_banner <hide_banner>` prints one but is a boolean). With a live
   ffmpeg (--capture, --ffmpeg) each one is probed: `ffmpeg -<name>` answers
   "Missing argument for option" when it needs one, and `ffmpeg -no<name>` is
   only accepted for booleans. ffmpeg also leaves some options out of its help
   (`-s` is one), so the strings stored in the executable are probed too, and
   every AVOption is confirmed with `ffmpeg -<name> 0`.
3. An option that only unprobed dumps list gets its arity from the help text.
   The table's header names those options; check them by hand.

Regenerating
------------
From the checked-in dumps (needs no ffmpeg):

    scripts/generate-ffmpeg-options.py

When the worker's ffmpeg has changed, refresh the worker dump first. Run this
script where the worker's ffmpeg is installed (it needs Python 3.9 or newer
and nothing else),

    python3 generate-ffmpeg-options.py --capture worker-conda-forge-linux-x64 \\
        --ffmpeg "$(command -v ffmpeg)"

replace scripts/ffmpeg-dumps/worker-conda-forge-linux-x64 with the directory
it wrote, keep its provenance.txt up to date, and run the script without
arguments. A dump can also be made by hand from the two commands listed under
"Dumps"; it is then not probed.

--dump and --ffmpeg choose other inputs; both can be repeated:

    scripts/generate-ffmpeg-options.py --dump some/dump --ffmpeg /path/to/ffmpeg
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime
import json
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap

REQUIRED_MAJOR_VERSION = 8

CODEC_SECTION = "AVCodecContext AVOptions:"
FORMAT_SECTION = "AVFormatContext AVOptions:"
FILTER_SECTION = "AVFilter AVOptions:"

# `-dec` opens a loopback decoder group; ffmpeg does not list it in its help.
UNLISTED_OPTIONS = ["dec"]

# `-i` is recognised by cfmpeg's parser itself and never looked up.
PARSER_OPTIONS = {"i"}

# The option families cfmpeg <= 0.1.6 treated as taking a value. Any of them
# that no build knows is kept, so it still consumes its argument.
LEGACY_VALUED_OPTIONS = [
    "ab", "ac", "acodec", "af", "aframes", "ar", "aspect", "aq", "atag", "b",
    "bsf", "bufsize", "c", "canvas_size", "ch_layout", "channel_layout",
    "codec", "color_primaries", "colorspace", "color_trc", "crf", "dcodec",
    "disposition", "f", "fflags", "filter", "filter_complex",
    "filter_complex_script", "filter_script", "filter_threads", "framerate",
    "frames", "fpsmax", "fs", "g", "guess_layout_max", "hls_list_size",
    "hls_segment_filename", "hls_time", "hwaccel", "hwaccel_device",
    "hwaccel_output_format", "itsscale", "itsoffset", "keyint_min", "lavfi",
    "level", "loglevel", "map", "map_chapters", "map_metadata",
    "max_muxing_queue_size", "maxrate", "metadata", "minrate", "movflags",
    "muxdelay", "muxpreload", "pass", "passlogfile", "pix_fmt", "preset",
    "pre", "profile", "q", "qscale", "qp", "r", "rc-lookahead", "readrate",
    "readrate_catchup", "readrate_initial_burst", "s", "sample_fmt",
    "sc_threshold", "scodec", "segment_list", "segment_time", "spatial-aq",
    "spre", "ss", "sseof", "start_number", "stats_period", "stream_loop", "t",
    "tag", "temporal-aq", "threads", "thread_queue_size", "timecode",
    "timestamp", "to", "tune", "vcodec", "vf", "vframes", "vpre", "vtag",
    "video_size", "vsync",
]

NATIVE_OPTION = re.compile(r"^-(\S+)(\s+<)?")
EXECUTABLE_STRING = re.compile(rb"(?:^|\x00)([A-Za-z0-9_?][A-Za-z0-9_?-]{0,40})(?=\x00)")
AV_OPTION = re.compile(r"^  -(\S+)\s")
SPECIFIER_SUFFIX = re.compile(r"\[:<\w+>\]$")

# How far an AVOption reaches; a name found in several classes keeps the widest.
SCOPE_RANK = {"Exact": 0, "CodecPrivate": 1, "CodecGeneric": 2}

PROBED_OPTIONS_HEADER = """\
# Written by scripts/generate-ffmpeg-options.py --capture: how this ffmpeg
# reacts to each option. One option per line.
#   Value     takes an argument
#   Bool      takes no argument and has a -no<name> spelling
#   Flag      takes no argument
#   Rejected  listed by `-h full` as an AVOption, but not a command line option
"""


@dataclasses.dataclass
class Build:
    """What one ffmpeg build contributes to the table."""

    source: str
    # ffmpeg's own options whose kind is certain: probed, or information options.
    native: dict[str, str]
    # ffmpeg's own options whose kind was read from the help text.
    unprobed: dict[str, str]
    av: dict[str, str]


def main() -> int:
    repo_root = pathlib.Path(__file__).resolve().parents[1]
    checked_in_dumps = repo_root / "scripts" / "ffmpeg-dumps"

    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog="Without --dump and --ffmpeg, every dump in scripts/ffmpeg-dumps is used.",
    )
    parser.add_argument(
        "--dump",
        action="append",
        type=pathlib.Path,
        default=[],
        help="dump directory to include; repeatable",
    )
    parser.add_argument(
        "--ffmpeg",
        action="append",
        default=[],
        help="ffmpeg binary to probe and include; repeatable",
    )
    parser.add_argument(
        "--capture",
        type=pathlib.Path,
        help="write a dump of the one --ffmpeg (default: ffmpeg) to this directory and stop",
    )
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        default=repo_root / "src" / "ffmpeg_options" / "table.rs",
    )
    args = parser.parse_args()

    if args.capture:
        if args.dump or len(args.ffmpeg) > 1:
            raise SystemExit("--capture takes one --ffmpeg and no --dump")
        capture(args.ffmpeg[0] if args.ffmpeg else "ffmpeg", args.capture)
        return 0

    dumps = args.dump
    if not dumps and not args.ffmpeg:
        dumps = sorted(path for path in checked_in_dumps.iterdir() if path.is_dir())

    builds = [read_dump(dump) for dump in dumps]
    for ffmpeg in args.ffmpeg:
        version_text, help_text, probed = interrogate(ffmpeg)
        builds.append(describe(version_text, help_text, probed, [f"probed live: {ffmpeg}"]))

    native: dict[str, str] = {}
    av: dict[str, str] = {}
    for build in builds:
        for name, kind in build.native.items():
            if native.setdefault(name, kind) != kind:
                raise SystemExit(f"-{name} is {native[name]} in one build and {kind} in another")
        for name, scope in build.av.items():
            if SCOPE_RANK[scope] >= SCOPE_RANK[av.get(name, "Exact")]:
                av[name] = scope

    # Options that no probed build knows fall back to what the help text says.
    from_help_text: dict[str, str] = {}
    for build in builds:
        for name, kind in build.unprobed.items():
            if name in native:
                continue
            if from_help_text.setdefault(name, kind) != kind:
                raise SystemExit(f"the help texts disagree about -{name}")
    native.update(from_help_text)

    legacy_only = sorted(
        name for name in LEGACY_VALUED_OPTIONS if name not in native and name not in av
    )
    for name in legacy_only:
        native[name] = "Value"

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        render([build.source for build in builds], native, av, sorted(from_help_text), legacy_only),
        encoding="ascii",
    )

    print(f"wrote {args.output}")
    print(f"  {len(native)} ffmpeg options, {len(av)} AVOptions")
    print(f"  arity read from the help text: {', '.join(sorted(from_help_text)) or 'none'}")
    print(f"  kept from the hand-written list only: {', '.join(legacy_only) or 'none'}")

    return 0


def capture(ffmpeg: str, directory: pathlib.Path) -> None:
    """Write a dump of a live ffmpeg, probes included."""
    version_text, help_text, probed = interrogate(ffmpeg)
    # Validates the version and the help layout before anything is written.
    describe(version_text, help_text, probed, [])

    command_line_help = help_text[: help_text.index(f"\n{FILTER_SECTION}\n")]
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "version.txt").write_text(version_text, encoding="utf-8")
    (directory / "help-full.txt").write_text(
        f"{command_line_help}\n{FILTER_SECTION}\n", encoding="utf-8"
    )
    (directory / "probed-options.txt").write_text(
        PROBED_OPTIONS_HEADER
        + "".join(f"{name} {kind}\n" for name, kind in sorted(probed.items())),
        encoding="ascii",
    )
    provenance = directory / "provenance.txt"
    if not provenance.exists():
        provenance.write_text(
            f"captured {datetime.date.today().isoformat()} on {platform.system()} "
            f"{platform.machine()} with generate-ffmpeg-options.py --capture; "
            f"help-full.txt is cut after the {FILTER_SECTION!r} heading\n",
            encoding="utf-8",
        )

    print(f"wrote {directory}")


def read_dump(directory: pathlib.Path) -> Build:
    """Describe the build whose dump is in `directory`."""
    for required in ("version.txt", "help-full.txt"):
        if not (directory / required).is_file():
            raise SystemExit(f"{directory} is not a dump: {required} is missing")

    probed = None
    probed_file = directory / "probed-options.txt"
    if probed_file.is_file():
        probed = dict(
            line.split()
            for line in probed_file.read_text(encoding="ascii").splitlines()
            if line and not line.startswith("#")
        )

    provenance = [f"dump: scripts/ffmpeg-dumps/{directory.name}"]
    provenance_file = directory / "provenance.txt"
    if provenance_file.is_file():
        provenance += provenance_file.read_text(encoding="utf-8").splitlines()

    return describe(
        (directory / "version.txt").read_text(encoding="utf-8"),
        (directory / "help-full.txt").read_text(encoding="utf-8"),
        probed,
        provenance,
    )


def describe(
    version_text: str, help_text: str, probed: dict[str, str] | None, provenance: list[str]
) -> Build:
    """Combine what a build prints with how it reacted to probes, if it was probed."""
    version_lines = version_text.splitlines()
    version_line = version_lines[0] if version_lines else ""
    version = re.match(r"ffmpeg version n?(\d+)", version_line)
    if not version or int(version.group(1)) != REQUIRED_MAJOR_VERSION:
        raise SystemExit(
            f"expected ffmpeg {REQUIRED_MAJOR_VERSION}.x, found: {version_line}"
        )

    info, listed, av_scopes = parse_help(help_text)
    probed = probed or {}

    native = {name: kind for name, kind in probed.items() if kind != "Rejected"}
    native.update({name: "Info" for name in info})
    unprobed = {
        name: "Value" if names_an_argument else "Flag"
        for name, names_an_argument in listed.items()
        if name not in native
    }
    av = {name: scope for name, scope in av_scopes.items() if probed.get(name) != "Rejected"}

    details = [
        line
        for line in version_lines
        if line.startswith(("built with", "configuration:"))
    ]
    wrapped = [
        chunk
        for line in [*details, *provenance]
        for chunk in textwrap.wrap(line, 90, break_long_words=False, break_on_hyphens=False)
    ]
    source = f"// Source: {version_line}\n" + "".join(f"//   {line}\n" for line in wrapped)

    print(version_line)
    print(f"  {len(native) + len(unprobed)} ffmpeg options, {len(av)} AVOptions")
    if probed:
        hidden = sorted(name for name in native if name not in listed and name not in info)
        rejected = sorted(name for name, kind in probed.items() if kind == "Rejected")
        print(f"  accepted by ffmpeg but missing from its help: {', '.join(hidden) or 'none'}")
        print(f"  listed by -h full but not accepted by ffmpeg: {', '.join(rejected) or 'none'}")
    else:
        print("  not probed")

    return Build(source, native, unprobed, av)


def interrogate(ffmpeg: str) -> tuple[str, str, dict[str, str]]:
    """Return the version text, the full help and the probe results of a live ffmpeg."""
    version_text = run(ffmpeg, ["-hide_banner", "-version"], cwd=None)
    help_text = run(ffmpeg, ["-hide_banner", "-h", "full"], cwd=None)
    info, listed, av_scopes = parse_help(help_text)
    unlisted = [
        name
        for name in [
            *UNLISTED_OPTIONS,
            *LEGACY_VALUED_OPTIONS,
            *executable_strings(ffmpeg),
        ]
        if name not in info and name not in listed and name not in PARSER_OPTIONS
    ]

    # Probes run in a scratch directory because options such as -report write
    # files into the working directory.
    with tempfile.TemporaryDirectory(prefix="cfmpeg-option-probe-") as scratch:
        probed = probe_native(ffmpeg, list(listed), list(dict.fromkeys(unlisted)), scratch)
        # A name that is also one of ffmpeg's own options cannot be probed as
        # an AVOption, because ffmpeg resolves its own option first.
        confirmations = probe_all(
            ffmpeg,
            {name: [f"-{name}", "0"] for name in av_scopes if name not in probed},
            scratch,
        )

    probed.update(
        {
            name: "Rejected"
            for name, output in confirmations.items()
            if "Unrecognized option" in output
        }
    )

    return version_text, help_text, probed


def run(ffmpeg: str, arguments: list[str], cwd: str | None) -> str:
    """Return ffmpeg's combined output; probes are expected to exit non-zero."""
    completed = subprocess.run(
        [ffmpeg, *arguments],
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=60,
        check=False,
    )
    return completed.stdout.decode("utf-8", errors="replace")


def parse_help(help_text: str) -> tuple[list[str], dict[str, bool], dict[str, str]]:
    """Split `ffmpeg -h full` into info options, other options and AVOptions.

    The other options map to whether the help names an argument for them.
    """
    for marker in (CODEC_SECTION, FORMAT_SECTION, FILTER_SECTION):
        if marker not in help_text.splitlines():
            raise SystemExit(f"`ffmpeg -h full` has no {marker!r} section")

    info: list[str] = []
    native: dict[str, bool] = {}
    av: dict[str, str] = {}
    section = ""
    area = "native"

    for line in help_text.splitlines():
        if line == FILTER_SECTION:
            break

        if line.endswith(":") and not line.startswith((" ", "-")):
            section = line
            if line == CODEC_SECTION:
                area = "codec"
            elif line == FORMAT_SECTION:
                area = "format"
            continue

        if area == "native":
            match = NATIVE_OPTION.match(line)
            if not match:
                continue
            name = SPECIFIER_SUFFIX.sub("", match.group(1))
            if name in info or name in native:
                continue
            if "information / capabilities" in section:
                info.append(name)
            else:
                native[name] = match.group(2) is not None
            continue

        match = AV_OPTION.match(line)
        if not match:
            continue
        if area == "format":
            scope = "Exact"
        elif section == CODEC_SECTION:
            scope = "CodecGeneric"
        else:
            scope = "CodecPrivate"
        name = match.group(1)
        if SCOPE_RANK[scope] >= SCOPE_RANK[av.get(name, "Exact")]:
            av[name] = scope

    return info, native, av


def probe_all(ffmpeg: str, commands: dict[str, list[str]], scratch: str) -> dict[str, str]:
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        outputs = pool.map(
            lambda arguments: run(ffmpeg, ["-nostdin", "-hide_banner", *arguments], scratch),
            commands.values(),
        )
        return dict(zip(commands.keys(), outputs))


def executable_strings(ffmpeg: str) -> list[str]:
    """Return the option-like strings stored in the ffmpeg executable."""
    path = shutil.which(ffmpeg)
    if path is None:
        raise SystemExit(f"{ffmpeg} was not found")
    data = pathlib.Path(path).read_bytes()
    return sorted(
        {match.group(1).decode("ascii") for match in EXECUTABLE_STRING.finditer(data)}
    )


def probe_native(
    ffmpeg: str, listed: list[str], unlisted: list[str], scratch: str
) -> dict[str, str]:
    """Classify ffmpeg's own options as Value, Bool or Flag."""
    alone = probe_all(
        ffmpeg, {name: [f"-{name}"] for name in [*listed, *unlisted]}, scratch
    )
    for name in listed:
        if "Unrecognized option" in alone[name]:
            raise SystemExit(f"ffmpeg lists -{name} in its help but does not accept it")

    accepted = [
        name for name, output in alone.items() if "Unrecognized option" not in output
    ]
    kinds = {
        name: "Value"
        for name in accepted
        if f"Missing argument for option '{name}'" in alone[name]
    }

    without_argument = [name for name in accepted if name not in kinds]
    negated = probe_all(
        ffmpeg, {name: [f"-no{name}"] for name in without_argument}, scratch
    )
    for name, output in negated.items():
        kinds[name] = "Flag" if "Unrecognized option" in output else "Bool"

    # `-nostdin` is the negated spelling of the boolean `-stdin`, not an option.
    return {
        name: kind
        for name, kind in kinds.items()
        if not (name.startswith("no") and kinds.get(name[2:]) == "Bool")
    }


def render(
    sources: list[str],
    native: dict[str, str],
    av: dict[str, str],
    from_help_text: list[str],
    legacy_only: list[str],
) -> str:
    def entries(table: dict[str, str]) -> str:
        return "".join(
            f"    ({json.dumps(name)}, {kind}),\n" for name, kind in sorted(table.items())
        )

    return (
        "// @generated by scripts/generate-ffmpeg-options.py; do not edit by hand.\n"
        "// How to regenerate it is described at the top of that script.\n"
        "//\n"
        + "//\n".join(sources)
        + "//\n"
        "// Whether these take an argument was read from the help text, because no\n"
        f"// probed build has them: {', '.join(from_help_text) or 'none'}\n"
        "//\n"
        "// Kept from the hand-written list of cfmpeg <= 0.1.6 although no source build\n"
        f"// knows them: {', '.join(legacy_only) or 'none'}\n"
        "\n"
        "use super::AvScope::{self, CodecGeneric, CodecPrivate, Exact};\n"
        "use super::NativeKind::{self, Bool, Flag, Info, Value};\n"
        "\n"
        "#[rustfmt::skip]\n"
        f"pub(super) const NATIVE_OPTIONS: &[(&str, NativeKind)] = &[\n{entries(native)}];\n"
        "\n"
        "#[rustfmt::skip]\n"
        f"pub(super) const AV_OPTIONS: &[(&str, AvScope)] = &[\n{entries(av)}];\n"
    )


if __name__ == "__main__":
    sys.exit(main())
