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
# macOS and Linux
brew tap cfmpeg/cfmpeg-homebrew-tap https://github.com/cfmpeg/cfmpeg-homebrew-tap.git
brew install cfmpeg/cfmpeg-homebrew-tap/cfmpeg

# From source
cargo install cfmpeg
```

Prebuilt releases bundle a helper `ffmpeg` binary, used for local fallback, direct-stream remuxing, segmented uploads, and joining progressive output batches.
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

# Require cloud execution for this run and disable local fallback
cfmpeg --remote -i input.mov -c:v libx264 output.mp4
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

- **ffmpeg-compatible arguments** — ffmpeg arguments pass through unchanged; remote runs reject concat file lists (`-f concat`), `-filter_script`/`-filter_complex_script`, and stdin, `pipe:`, or `/dev/` inputs with an error suggesting `--local`
- **Parallel multipart uploads** — large files upload fast with chunked parallel transfers
- **Real-time progress** — streaming progress display, just like local ffmpeg
- **Automatic local fallback** — if remote execution is unavailable, cfmpeg falls back to local ffmpeg (see [Local Fallback](#local-fallback))
- **Automatic output delivery** — completed outputs are downloaded back to your local filesystem
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
local_fallback = true
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
| `CFMPEG_REMOTE_PROFILE` | `remote_profile` |
| `CFMPEG_REMOTE_CPU` | `remote_cpu` |
| `CFMPEG_REMOTE_MEMORY` | `remote_memory_mb` (accepts `4096`, `512m`, `4g`) |
| `CFMPEG_REMOTE_TIMEOUT` | `remote_timeout_seconds` (accepts `3600`, `90s`, `15m`, `2h`) |

## Flags and Commands

| Flag / command | Effect |
| --- | --- |
| `--local` | Run local ffmpeg; skip the API |
| `--remote` | Require cloud execution; never fall back |
| `--no-download` | Leave outputs in cloud storage and print signed URLs |
| `--cf-profile <balanced\|highcpu>` | Remote execution profile |
| `--cf-cpu <cores>` | Remote CPU cores |
| `--cf-memory <size>` | Remote memory, e.g. `4096`, `512m`, `16g` |
| `--cf-timeout <duration>` | Remote job timeout, e.g. `3600`, `90s`, `15m`, `2h` |
| `cfmpeg cancel <job_id>` | Cancel a remote job |

Any `--cf-*` flag also implies `--remote`.

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

## Local Fallback

Force local execution:

```bash
cfmpeg --local -i input.mov output.mp4
```

With `local_fallback = true` (default), cfmpeg falls back to local ffmpeg with a warning when:

- no API key is configured
- the API health check fails (unreachable or non-2xx)
- creating or starting the job returns HTTP 402, 404, 405, 501, or 503 (including insufficient balance)

Use `--remote` or any `--cf-*` resource flag to require cloud execution and fail instead of falling back locally.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --locked
```

The GitHub Actions workflow in `.github/workflows/ci.yml` runs the same checks on pushes and pull requests to `main`.

## Releases

Homebrew releases are published from GitHub Actions to `cfmpeg/cfmpeg-homebrew-tap` using the release workflows in `.github/workflows/`.

## License

MIT
