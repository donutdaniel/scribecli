# Benchmarks

This file captures the current latency snapshot for `scribecli`.

Important: these are single-machine observations from real local runs, not a
formal benchmark suite. Read them as directional latency numbers for the
current implementation and current machine.

## Environment

- Snapshot date: `2026-03-26`
- Machine: `Apple M4 Max`
- OS: `macOS`
- CLI binary: local development build
- Capture backend: native ScreenCaptureKit on macOS
- Transcription backend: local `whisper.cpp`
- Model: `ggml-large-v3-turbo`

## Snapshot

| Case | Audio length | Command shape | Latency |
|---|---:|---|---:|
| Foreground record finalize | 5 minutes | `record` stop -> final transcript | about `31s` |
| Recovery transcription | 53m35s | `transcribe` / manual recovery pass | about `83s` |
| Background lifecycle smoke | short smoke sample | `start` -> `stop` -> final transcript | fast enough for interactive stop; real transcript returned during smoke validation |

## Notes

- `stop` currently blocks until transcription finishes. On this hardware, that
  is acceptable for the intended agent workflow.
- The numbers above are dominated by local Whisper inference time, not network.
- The exact latency will move with model choice, machine speed, and audio
  length.
- `large-v3-turbo` is a reasonable default for local notes, but smaller models
  will be faster and usually less accurate.
- These numbers do not claim anything about other Macs, Windows, Linux, or a
  future packaged app build.
