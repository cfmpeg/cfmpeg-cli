# cfmpeg

Cloud ffmpeg — run ffmpeg commands on powerful remote containers.

```bash
# Instead of this (runs locally, takes 45 minutes):
ffmpeg -i input.mov -c:v libx265 -crf 28 output.mp4

# Run this (runs in the cloud, takes 3 minutes):
cfmpeg -i input.mov -c:v libx265 -crf 28 output.mp4
```

cfmpeg is a drop-in replacement for `ffmpeg`. Same arguments, same behavior — but processing happens on remote containers. The output file appears locally as if ffmpeg ran on your machine.

No Docker. No Python. No cloud credentials. Install, authenticate, go.

## Install

```bash
# macOS and Linux: install script
curl -fsSL https://cfmpeg.com/install | sh

# macOS and Linux: Homebrew
brew install cfmpeg/tap/cfmpeg

# From source
cargo install cfmpeg
```

The install script downloads the release for your platform, verifies its checksum, and installs under `/usr/local`, or `~/.local` when `/usr/local` isn't writable.
Set `CFMPEG_INSTALL_DIR` to pick another prefix and `CFMPEG_VERSION` to pin a release.

### Prebuilt binaries

Every [release](https://github.com/cfmpeg/cfmpeg-cli/releases/latest) carries one archive per platform, each with a `.sha256` checksum next to it:

| Platform | Archive |
| --- | --- |
| macOS, Apple silicon | `cfmpeg-darwin-arm64.tar.gz` |
| macOS, Intel | `cfmpeg-darwin-x64.tar.gz` |
| Linux x86_64 (glibc) | `cfmpeg-linux-x64.tar.gz` |
| Linux arm64 (glibc) | `cfmpeg-linux-arm64.tar.gz` |
| Linux x86_64 (static, musl) | `cfmpeg-linux-x64-musl.tar.gz` |
| Linux arm64 (static, musl) | `cfmpeg-linux-arm64-musl.tar.gz` |
| Windows x86_64 | `cfmpeg-windows-x64.zip` |

Each archive holds `bin/cfmpeg` and `libexec/ffmpeg` (`cfmpeg.exe` and `ffmpeg.exe` on Windows).
Keep the two directories side by side, because cfmpeg looks for its helper in `libexec` next to its own `bin` directory.
A `licenses` directory carries the licences of everything in the archive, including the FFmpeg build's source and configure flags.

The musl archives are statically linked. Use them on Alpine and other images without glibc, and on distributions whose glibc is too old for the regular Linux builds:

```bash
# x86_64; use cfmpeg-linux-arm64-musl.tar.gz on arm64
wget -qO- https://github.com/cfmpeg/cfmpeg-cli/releases/latest/download/cfmpeg-linux-x64-musl.tar.gz | tar -xz -C /usr/local bin libexec
```

On Windows, extract `cfmpeg-windows-x64.zip` and add its `bin` folder to your `PATH`.

Prebuilt releases bundle a helper `ffmpeg` binary, used for direct-stream remuxing, segmented uploads, and joining progressive output batches, and for local runs only when no other ffmpeg is installed.
If you install from source instead, keep `ffmpeg` available on `PATH` or point cfmpeg at an explicit helper with `CFMPEG_FFMPEG_BINARY`.

## Quick Start

```bash
# Authenticate (one time)
cfmpeg auth login

# See cfmpeg-specific commands and flags
cfmpeg --help

# Use it exactly like ffmpeg
cfmpeg -i input.mov -c:v libx265 -crf 28 output.mp4

# Request larger remote hardware without changing ffmpeg args
cfmpeg --cf-profile highcpu -i input.mov -c:v libx264 output.mp4

# Run with local ffmpeg instead when the cloud is unavailable (off by default)
cfmpeg --allow-fallback -i input.mov -c:v libx264 output.mp4
```

`cfmpeg auth login` opens the API key page and then prompts you to paste the key back into the CLI for local storage.

## How It Works

1. You run `cfmpeg` with standard ffmpeg arguments
2. cfmpeg uploads your input files to cloud storage
3. A remote container runs your ffmpeg command
4. The output is downloaded to your local filesystem
5. Temporary cloud files are cleaned up automatically

URL inputs skip the upload entirely — the container fetches directly:

```bash
cfmpeg --remote -i https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4 -c:v libx264 -crf 30 -preset veryfast output.mp4
```

## Smoke Tests

Use a synthetic input when you want to verify remote execution without preparing a media file:

```bash
cfmpeg --remote -f lavfi -i testsrc=size=128x128:rate=1 -t 1 -pix_fmt yuv420p /tmp/cfmpeg-smoke.mp4
```

Use a small public URL to verify URL passthrough:

```bash
cfmpeg --remote -i https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4 -c:v libx264 -crf 30 -preset veryfast /tmp/cfmpeg-url-smoke.mp4
```

## Features

- **ffmpeg-compatible arguments** — ffmpeg 8 arguments pass through unchanged; what cannot run remotely is rejected before anything is uploaded (see [Not Supported Yet](#not-supported-yet))
- **Parallel multipart uploads** — large files upload fast with chunked parallel transfers
- **Real-time progress** — streaming progress display, just like local ffmpeg
- **Opt-in local fallback** — with `--allow-fallback`, cfmpeg runs local ffmpeg when remote execution is unavailable (see [Local Fallback](#local-fallback))
- **Automatic output delivery** — completed outputs are downloaded back to your local filesystem, and a file only appears at its destination once it is complete
- **Ctrl-C cancels the job** — interrupting cfmpeg cancels the remote job, so nothing keeps running or billing (see [Interrupting a Job](#interrupting-a-job))
- **URL passthrough** — remote URLs are fetched directly by the container, no upload needed

## Authentication

```bash
cfmpeg auth login     # Open the API key page and paste a key
cfmpeg auth status    # Check auth status
cfmpeg auth logout    # Remove saved credentials
```

Or set the API key directly:

```bash
export CFMPEG_API_KEY=cfm_xxxxxxxxxxxx
```

## Configuration

Run `cfmpeg config path` to print the active config path for your platform.
On macOS this is usually `~/Library/Application Support/cfmpeg/config.toml`;
on Linux it is usually `~/.config/cfmpeg/config.toml`.

```toml
api_key = "cfm_xxxxxxxxxxxx"
api_base = "https://cfmpeg.com/v1"
allow_fallback = false
remote_profile = "highcpu"
remote_cpu = 8
remote_memory_mb = 16384
remote_timeout_seconds = 5400
```

```bash
cfmpeg config path    # Print config path
cfmpeg config show    # Print current config with the API key masked
cfmpeg config edit    # Open in $EDITOR
```

Environment variables override the config file:

| Variable | Overrides |
| --- | --- |
| `CFMPEG_API_KEY` | `api_key` |
| `CFMPEG_API_BASE` | `api_base` |
| `CFMPEG_WEB_BASE` | Dashboard URL (derived from `api_base` by default) |
| `CFMPEG_CONFIG_DIR` | Directory holding `config.toml` |
| `CFMPEG_FFMPEG_BINARY` | Path to the local `ffmpeg` |
| `CFMPEG_ALLOW_FALLBACK` | `allow_fallback` (accepts `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`) |
| `CFMPEG_REMOTE_PROFILE` | `remote_profile` |
| `CFMPEG_REMOTE_CPU` | `remote_cpu` |
| `CFMPEG_REMOTE_MEMORY` | `remote_memory_mb` (accepts `4096`, `512m`, `4g`) |
| `CFMPEG_REMOTE_TIMEOUT` | `remote_timeout_seconds` (accepts `3600`, `90s`, `15m`, `2h`) |

## Flags and Commands

| Flag / command | Effect |
| --- | --- |
| `--local` | Run local ffmpeg; skip the API |
| `--allow-fallback` | Run local ffmpeg when cloud execution is unavailable |
| `--remote` | Require cloud execution; never fall back, even when the config allows it |
| `--no-download` | Leave outputs in cloud storage and print signed URLs |
| `--cf-profile <balanced\|highcpu>` | Remote execution profile |
| `--cf-cpu <cores>` | Remote CPU cores |
| `--cf-memory <size>` | Remote memory, e.g. `4096`, `512m`, `16g` |
| `--cf-timeout <duration>` | Remote job timeout, e.g. `3600`, `90s`, `15m`, `2h` |
| `cfmpeg cancel <job_id>` | Cancel a remote job |
| `cfmpeg download [-y\|-n] <job_id> [paths...]` | Download the outputs of a finished job again |

Any `--cf-*` flag also implies `--remote`, unless `--allow-fallback` is given on the same command line.

## Usage & Billing

```bash
cfmpeg usage          # Current billing period summary
```

Billed per second of container wall-clock time.

Remote jobs are priced from the resources reserved for the resolved execution
profile, not from a flat CPU tier. The server combines the current CPU core-minute
and memory GiB-minute rates, applies the configured minimum billable duration,
and records the exact charge against your prepaid balance.

Use the billing dashboard for the current profile quotes, and `cfmpeg usage` for
your current balance and job charges.

## Not Supported Yet

cfmpeg reads the command line the way ffmpeg 8 does, using a table of the options of the remote worker's ffmpeg.
Remote runs refuse the following before anything is uploaded or billed, with an error that says why. Run these commands with `--local`.

- **Options the worker's ffmpeg does not have.** cfmpeg has to know whether an option takes an argument, so it stops at an option it does not recognise instead of guessing. That includes options of other ffmpeg builds, such as VideoToolbox options on macOS. If an option is valid in ffmpeg 8 and still refused, please [report it](https://github.com/cfmpeg/cfmpeg-cli/issues).
- **Two-pass encoding:** `-pass`, `-passlogfile`.
- **Attachments and preset files:** `-attach`, `-fpre`, `-pre`, `-vpre`, `-apre`, `-spre`.
- **Options that write a side file:** `-vstats`, `-vstats_file`, `-sdp_file`, `-dump_attachment`, `-stats_enc_pre`, `-stats_enc_post`, `-stats_mux_pre`, `-print_graphs_file`, and `-progress` to a file.
- **Values read from local files:** `-filter_script`, `-filter_complex_script`, and `-/option file` (for example `-/filter:v graph.txt`).
- **Filters that read a local file,** in `-vf`, `-af`, `-filter`, `-filter_complex` and `-lavfi`: `subtitles`, `ass`, `movie`, `amovie`, `lut3d`/`lut1d` with a file, `drawtext` with `fontfile` or `textfile`, `sofalizer`, `arnndn`, `removelogo`, `sendcmd`/`asendcmd` with a command file, and `curves` with `psfile`.
- **List and stream inputs:** concat file lists (`-f concat`), stdin, `pipe:` and `/dev/` inputs.
- **Most generated inputs:** `-f lavfi -i <graph>` works for graphs that start with `color=`, `testsrc`, `anullsrc` or `nullsrc`; other lavfi sources are refused.
- **Outputs that write more than the named file:** HLS and DASH (`-f hls`, `.m3u8`, `-f dash`, `.mpd`), the segment muxers (`-f segment`, `-f stream_segment`, `-f ssegment`), `-f tee`, and the Smooth Streaming, HDS and WebM chunk muxers.
- **Image sequences and other expanded output names:** a number pattern (`thumb%03d.jpg`) or `-strftime`. A single image such as `-frames:v 1 poster.jpg` works.
- **Outputs that are not files:** stdout and pipes (`-`, `pipe:1`, `/dev/stdout`), network destinations (`rtmp://...`), and `-f null`.
- **Information options:** `-version`, `-encoders`, `-h` and the like print ffmpeg's own details and run no job.
- **GPU execution:** `--cf-gpu` is not available.

Several plain output files in one command work.

## Output Files

Output paths are checked before the job starts:

- With `-n`, an output that already exists stops the run with ffmpeg's own message, `File 'out.mp4' already exists. Exiting.`
- With `-y`, or with neither flag, existing outputs are overwritten. cfmpeg never prompts.
- The directory of each output has to exist and be writable.
- An output that is the same file as one of the inputs is refused, as ffmpeg does: `Output out.mov same as Input #0 - exiting`.

A `file:` prefix on an input or output names the plain path, as in ffmpeg.

Each output is downloaded to a temporary file in the destination directory (`.cfmpeg-<id>-<name>`) and renamed into place when the download is complete. A failed or interrupted download removes the temporary file and leaves an existing output untouched.

## Interrupting a Job

Ctrl-C (SIGINT) and SIGTERM cancel the remote job before cfmpeg exits. cfmpeg prints one line saying that the job was cancelled, or how to cancel it with `cfmpeg cancel <job_id>` when the request failed, and exits with status 130 (SIGINT) or 143 (SIGTERM).

The job is cancelled the same way whenever cfmpeg gives up on a job that has not finished: when the job timeout (`--cf-timeout`, one hour by default) has passed, when an upload fails, or when the job's status cannot be read any more.

A job that has already completed is never cancelled. If its download fails or is interrupted, cfmpeg says so and prints the signed URLs of the outputs, which stay in cloud storage until they expire. Fetch them again with `cfmpeg download`.

## Downloading a Finished Job Again

```bash
cfmpeg download <job_id>                 # into the current directory
cfmpeg download <job_id> final.mp4       # to a path of your choice
cfmpeg download -n <job_id> final.mp4    # never overwrite an existing file
```

The job id is printed when a job starts. Without paths, each output is written under the name the job stored it with (`output_0.mp4`, `output_1.mp4`, ...); with paths, give one per output, in the order of the original command. Files are checked and written like the outputs of a normal run.

The command fails with a clear message when the job does not exist, has not finished, failed or was cancelled, or when its outputs have expired.

## Local Fallback

Force local execution:

```bash
cfmpeg --local -i input.mov output.mp4
```

By default cfmpeg does not fall back: when remote execution is unavailable the command fails and says why.
Allow the fallback with `--allow-fallback`, `allow_fallback = true` in the config file, or `CFMPEG_ALLOW_FALLBACK=1`. cfmpeg then runs local ffmpeg, after a one-line warning that names the reason, when:

- no API key is configured
- the API health check fails (unreachable or non-2xx)
- creating or starting the job returns HTTP 402, 404, 405, 501, or 503 (including insufficient balance)

A job that was refused at start is cancelled before the command runs locally. cfmpeg does not fall back once a remote job has started.

Use `--remote` or any `--cf-*` resource flag to require cloud execution even when the config or environment allows the fallback.

Local runs (`--local` and the fallback) pass your arguments to ffmpeg untouched and exit with ffmpeg's exit code.
They use `CFMPEG_FFMPEG_BINARY` if it is set, otherwise the `ffmpeg` on `PATH`, and the bundled helper only when no other ffmpeg is installed: the helper is built without external libraries, so it has no libx264 or libx265.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --locked
```

The GitHub Actions workflow in `.github/workflows/ci.yml` runs the same checks on pushes and pull requests to `main`.

The table of ffmpeg options in `src/ffmpeg_options/table.rs` is generated and checked in. Its input is the dump in `scripts/ffmpeg-dumps/worker-conda-forge-linux-x64/`: what the remote worker's ffmpeg (conda-forge build, ffmpeg 8.1.2, linux x86_64) prints for `-version` and `-h full`, and how it reacted when each option was probed. `cargo test` fails when the table and the dump disagree.

The worker tracks ffmpeg 8.x, so the dump and the table have to be regenerated whenever the worker's ffmpeg changes:

```bash
# 1. Where the worker's ffmpeg is installed (the script needs Python 3 and nothing else)
python3 generate-ffmpeg-options.py --capture worker-conda-forge-linux-x64 --ffmpeg "$(command -v ffmpeg)"

# 2. Replace scripts/ffmpeg-dumps/worker-conda-forge-linux-x64 with that directory, then
scripts/generate-ffmpeg-options.py
```

The header of `scripts/generate-ffmpeg-options.py` describes the details.

## Releases

Homebrew releases are published from GitHub Actions to `cfmpeg/homebrew-tap` using the release workflows in `.github/workflows/`.

## License

MIT
