use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub root_dir: PathBuf,
    #[allow(dead_code)]
    pub system_audio_path: PathBuf,
    #[allow(dead_code)]
    pub microphone_audio_path: PathBuf,
    pub raw_audio_path: PathBuf,
    pub transcript_path: PathBuf,
    pub event_log_path: PathBuf,
}

impl SessionPaths {
    pub fn create(base_dir: &Path, session_id: &str) -> Result<Self> {
        let root_dir = base_dir.join(session_id);
        fs::create_dir_all(&root_dir).with_context(|| {
            format!("failed to create session directory {}", root_dir.display())
        })?;

        Ok(Self {
            system_audio_path: root_dir.join("system.wav"),
            microphone_audio_path: root_dir.join("microphone.wav"),
            raw_audio_path: root_dir.join("session.wav"),
            transcript_path: root_dir.join("transcript.json"),
            event_log_path: root_dir.join("events.jsonl"),
            root_dir,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub speaker: String,
    pub speaker_confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Transcript {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionEvent {
    pub ts: String,
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

pub struct EventLogger {
    file: File,
}

impl EventLogger {
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        Ok(Self { file })
    }

    pub fn append(
        &mut self,
        kind: &str,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Result<()> {
        let event = SessionEvent {
            ts: now_rfc3339(),
            kind: kind.to_string(),
            message: message.into(),
            data,
        };
        let raw = serde_json::to_string(&event).context("failed to serialize event log entry")?;
        writeln!(self.file, "{raw}").context("failed to append event log entry")?;
        self.file.flush().context("failed to flush event log")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub object: &'static str,
    pub status: &'static str,
    pub session_id: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_seconds: f64,
    pub audio_path: PathBuf,
    pub transcript_path: PathBuf,
    pub event_log_path: PathBuf,
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub warnings: Vec<String>,
    pub partial_chunks_transcribed: usize,
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp().to_string())
}

pub fn write_pretty_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let raw = serde_json::to_string_pretty(value).context("failed to serialize JSON artifact")?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
}
