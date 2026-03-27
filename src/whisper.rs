use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::session::{Transcript, TranscriptSegment};

#[derive(Debug, Clone)]
pub struct WhisperConfig {
    pub cli_path: PathBuf,
    pub model_path: PathBuf,
}

pub fn whisper_version(cli_path: &Path) -> Result<String> {
    let output = Command::new(cli_path)
        .arg("-h")
        .output()
        .with_context(|| format!("failed to execute {}", cli_path.display()))?;

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if !output.status.success() {
        bail!(
            "failed to run whisper.cpp CLI at {}: {}",
            cli_path.display(),
            combined.trim()
        );
    }

    let first_line = combined.lines().next().unwrap_or("").trim().to_string();
    Ok(first_line)
}

pub fn transcribe_audio(config: &WhisperConfig, audio_path: &Path) -> Result<Transcript> {
    let output = Command::new(&config.cli_path)
        .args(["-m"])
        .arg(&config.model_path)
        .args(["-f"])
        .arg(audio_path)
        .output()
        .with_context(|| format!("failed to execute {}", config.cli_path.display()))?;

    if !output.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!("whisper.cpp transcription failed: {}", combined.trim());
    }

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(parse_transcript_output(&combined))
}

pub fn parse_transcript_output(output: &str) -> Transcript {
    let segment_re = Regex::new(
        r"^\[(\d{2}):(\d{2}):(\d{2})\.(\d{3}) --> (\d{2}):(\d{2}):(\d{2})\.(\d{3})\]\s*(.+)$",
    )
    .expect("regex");
    let mut segments = Vec::new();

    for line in output.lines() {
        let Some(caps) = segment_re.captures(line.trim()) else {
            continue;
        };

        let start_ms = parse_timestamp_ms(&caps, 1);
        let end_ms = parse_timestamp_ms(&caps, 5);
        let text = caps
            .get(9)
            .map(|value| value.as_str().trim().to_string())
            .unwrap_or_default();

        segments.push(TranscriptSegment {
            start_ms,
            end_ms,
            text,
            speaker: "speaker_1".to_string(),
            speaker_confidence: 0.2,
        });
    }

    if segments.is_empty() {
        let text = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return Transcript {
            text,
            segments: Vec::new(),
        };
    }

    let text = segments
        .iter()
        .map(|segment| normalize_segment_text(&segment.text))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    Transcript { text, segments }
}

pub fn apply_basic_speaker_labels(transcript: &mut Transcript) {
    let mut active_speaker = 1;
    let mut saw_turn_marker = false;

    for segment in &mut transcript.segments {
        if segment.text.contains("[SPEAKER_TURN]") {
            saw_turn_marker = true;
        }

        segment.text = normalize_segment_text(&segment.text);
        segment.speaker = format!("speaker_{active_speaker}");
        segment.speaker_confidence = if saw_turn_marker { 0.45 } else { 0.2 };

        if saw_turn_marker {
            active_speaker = if active_speaker == 1 { 2 } else { 1 };
        }
    }

    transcript.text = transcript
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
}

fn normalize_segment_text(text: &str) -> String {
    text.replace("[SPEAKER_TURN]", "").trim().to_string()
}

fn parse_timestamp_ms(caps: &regex::Captures<'_>, start_index: usize) -> u64 {
    let hours = caps
        .get(start_index)
        .and_then(|value| value.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let minutes = caps
        .get(start_index + 1)
        .and_then(|value| value.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let seconds = caps
        .get(start_index + 2)
        .and_then(|value| value.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let millis = caps
        .get(start_index + 3)
        .and_then(|value| value.as_str().parse::<u64>().ok())
        .unwrap_or(0);

    (((hours * 60 + minutes) * 60) + seconds) * 1000 + millis
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamped_output() {
        let output = r#"
[00:00:00.000 --> 00:00:01.250]   Hello there [SPEAKER_TURN]
[00:00:01.250 --> 00:00:02.500]   General Kenobi
"#;

        let mut transcript = parse_transcript_output(output);
        apply_basic_speaker_labels(&mut transcript);

        assert_eq!(transcript.segments.len(), 2);
        assert_eq!(transcript.segments[0].speaker, "speaker_1");
        assert_eq!(transcript.segments[1].speaker, "speaker_2");
        assert_eq!(transcript.text, "Hello there General Kenobi");
    }
}
