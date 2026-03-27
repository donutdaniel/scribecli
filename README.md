# scribecli

A Rust CLI for local meeting recording and transcription on macOS. Native
`ScreenCaptureKit` capture, local `whisper.cpp`, no backend.

Default output is human-readable. Use `--output json` or `--output yaml` for
machine-stable formats.

## Install

Pick one:

| Method | Command / Steps |
|---|---|
| Homebrew (macOS) | `brew install donutdaniel/homebrew-tap/scribecli` |
| Binary ([GitHub Releases](https://github.com/donutdaniel/scribecli/releases)) | Download the archive for your platform, extract it, and move `scribecli` somewhere on your `PATH`. |
| Cargo / local checkout | `cargo install --path .` or `cargo build --release` |

Homebrew and release archives bundle `whisper-cli`. Raw Cargo or repo builds do
not. For those, install `whisper-cpp` yourself first, for example:

```bash
brew install whisper-cpp
```

For a prod-like local package layout:

```bash
./scripts/package_local_release.sh
./dist/local/scribecli-local/bin/scribecli --help
```

## Quickstart

1. Bootstrap the local runtime:

```bash
scribecli setup
```

2. Verify capture and model readiness:

```bash
scribecli --output json doctor
```

3. Record a meeting:

```bash
scribecli record
```

Or use the split lifecycle if an agent wants explicit start/stop control:

```bash
scribecli start
scribecli stop
```

## Global Flags

| Flag | Description |
|---|---|
| `--output <format>` | `human` (default), `json`, or `yaml` |
| `--verbose` | Increase log verbosity. Repeat for more detail |

## Commands

### setup

```bash
scribecli setup
scribecli setup --model large-v3-turbo
scribecli setup --model medium-en --force-download
```

### model

```bash
scribecli model list
scribecli model use small-en
scribecli model use large-v3-turbo --force-download
scribecli model remove small-en
scribecli model remove --all
```

### record

```bash
scribecli record
scribecli --output json record --duration-seconds 30
scribecli record --input-mode single-device
scribecli record --input-mode mic-system-mix
scribecli record --display-id 1 --duration-seconds 15
```

### start / stop

```bash
scribecli start
scribecli --output json start --session-name weekly-sync
scribecli start --duration-seconds 1800
scribecli stop
```

### transcribe

```bash
scribecli transcribe ./meeting.wav
scribecli transcribe weekly-sync-20260326T120000Z-1234
scribecli transcribe ~/Library/Application\ Support/com.scribecli.scribecli/sessions/<session-id>
```

### doctor / devices / config / cleanup

```bash
scribecli doctor
scribecli devices list
scribecli config get
scribecli config set input-mode single_device
scribecli cleanup --sessions
scribecli cleanup --all
```

## Storage

Managed state lives under:

```text
~/Library/Application Support/com.scribecli.scribecli
```

Important subdirectories:

- `models/`: managed Whisper model files
- `sessions/`: recordings, transcripts, and event logs
- `bin/`: managed `whisper-cli` wrapper
- `native-helper/`: compiled native capture helper

`brew uninstall scribecli` removes the package, but not this app data. To purge
managed state:

```bash
scribecli cleanup --all
```

## Notes

- macOS only.
- Native macOS capture is the only backend.
- Terminal installs show permissions under the terminal app that launched
  `scribecli` (for example, Ghostty).
- `mic-system-mix` still uses `ffmpeg` for post-capture audio mixing.
- Speaker labels are heuristic only. They are segmentation hints, not identity.
