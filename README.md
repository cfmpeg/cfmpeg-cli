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
# macOS
brew install aarondfrancis/homebrew-tap/cfmpeg

# Linux
curl -fsSL https://cfmpeg.dev/install.sh | sh

# From source
cargo install cfmpeg
```

Prebuilt releases bundle helper `ffmpeg` and `ffprobe` binaries for cfmpeg's internal remuxing and future media segmentation paths.
If you install from source instead, keep `ffmpeg` available on `PATH` or point cfmpeg at an explicit helper with `CFMPEG_FFMPEG_BINARY`.

## Quick Start

```bash
# Authenticate (one time)
cfmpeg auth login

# Use it exactly like ffmpeg
cfmpeg -i input.mov -c:v libx265 -crf 28 output.mp4

# Request larger remote hardware without changing ffmpeg args
cfmpeg --cf-profile gpu --cf-gpu required -i input.mov -c:v h264_nvenc output.mp4
```

`cfmpeg auth login` opens the API key page and then prompts you to paste the key back into the CLI for local storage.
If you request GPU execution, keep the video encoder on an NVENC codec such as `h264_nvenc`, `hevc_nvenc`, or `av1_nvenc`; the CLI will warn when the command selects a mode that will not use the remote GPU path.

## How It Works

1. You run `cfmpeg` with standard ffmpeg arguments
2. cfmpeg uploads your input files to cloud storage
3. A remote container runs your ffmpeg command
4. The output is downloaded to your local filesystem
5. Temporary cloud files are cleaned up automatically

URL inputs skip the upload entirely — the container fetches directly:

```bash
cfmpeg -i https://example.com/raw.mov -c:v libx264 output.mp4
```

## Features

- **Full ffmpeg compatibility** — any valid ffmpeg command works
- **Parallel multipart uploads** — large files upload fast with chunked parallel transfers
- **Real-time progress** — streaming progress display, just like local ffmpeg
- **Automatic local fallback** — if the API is unreachable, cfmpeg falls back to local ffmpeg
- **Output caching** — same input + same args = instant cached result
- **URL passthrough** — remote URLs are fetched directly by the container, no upload needed

## Authentication

```bash
cfmpeg auth login     # Log in via browser
cfmpeg auth status    # Check auth status
cfmpeg auth logout    # Remove saved credentials
```

Or set the API key directly:

```bash
export CFMPEG_API_KEY=cfm_xxxxxxxxxxxx
```

## Configuration

Config lives at `~/.config/cfmpeg/config.toml`:

```toml
api_key = "cfm_xxxxxxxxxxxx"
api_base = "https://api.cfmpeg.dev/v1"
local_fallback = true
remote_profile = "highcpu"
remote_cpu = 8
remote_memory_mb = 16384
remote_gpu = "off"
remote_timeout_seconds = 5400
```

```bash
cfmpeg config path    # Print config path
cfmpeg config show    # Print current config with the API key masked
cfmpeg config edit    # Open in $EDITOR
```

## Usage & Billing

```bash
cfmpeg usage          # Current billing period summary
```

Billed per second of container wall-clock time:

| Tier | Rate |
|------|------|
| CPU  | $0.01/min |
| GPU  | $0.10/min (coming soon) |

## Local Fallback

Force local execution:

```bash
cfmpeg --local -i input.mov output.mp4
```

If the API is unreachable and `local_fallback = true` (default), cfmpeg automatically falls back to local ffmpeg with a warning.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --locked
```

The GitHub Actions workflow in `.github/workflows/ci.yml` runs the same checks on pushes and pull requests to `main`.

## Releases

Homebrew releases are published from GitHub Actions to `aarondfrancis/homebrew-tap` using the release workflows in `.github/workflows/`.

## License

MIT
