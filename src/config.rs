use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const APP_NAME: &str = "scribecli";
const APP_QUALIFIER: &str = "com";
const APP_ORGANIZATION: &str = "scribecli";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        if let Ok(raw) = env::var("SCRIBECLI_CONFIG_DIR") {
            return Ok(Self::from_base(PathBuf::from(raw)));
        }
        project_config_dir(APP_QUALIFIER, APP_ORGANIZATION, APP_NAME)
    }

    pub fn from_base(base: PathBuf) -> Self {
        let config_file = base.join("config.toml");
        Self {
            config_dir: base,
            config_file,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir).with_context(|| {
            format!(
                "failed to create config directory at {}",
                self.config_dir.display()
            )
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    MicSystemMix,
    SingleDevice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default = "default_ffmpeg_path")]
    pub ffmpeg_path: String,
    #[serde(default)]
    pub whisper_cli_path: Option<PathBuf>,
    #[serde(default)]
    pub whisper_model_path: Option<PathBuf>,
    #[serde(default)]
    pub artifacts_dir: Option<PathBuf>,
    #[serde(default = "default_partial_interval_seconds")]
    pub partial_interval_seconds: u64,
    #[serde(default)]
    pub input_mode: InputMode,
    #[serde(default)]
    pub display_id: Option<u32>,
    #[serde(default)]
    pub microphone_device: Option<String>,
    #[serde(default)]
    pub system_device: Option<String>,
    #[serde(default)]
    pub single_input_device: Option<String>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            ffmpeg_path: default_ffmpeg_path(),
            whisper_cli_path: None,
            whisper_model_path: None,
            artifacts_dir: None,
            partial_interval_seconds: default_partial_interval_seconds(),
            input_mode: InputMode::MicSystemMix,
            display_id: None,
            microphone_device: None,
            system_device: None,
            single_input_device: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub ffmpeg_path: PathBuf,
    pub whisper_cli_path: Option<PathBuf>,
    pub whisper_model_path: Option<PathBuf>,
    pub artifacts_dir: PathBuf,
    pub partial_interval_seconds: u64,
    pub input_mode: InputMode,
    pub display_id: Option<u32>,
    pub microphone_device: Option<String>,
    pub system_device: Option<String>,
    pub single_input_device: Option<String>,
    pub config_file: PathBuf,
}

impl ConfigFile {
    pub fn into_effective(self, paths: &AppPaths) -> EffectiveConfig {
        let artifacts_dir = self
            .artifacts_dir
            .map(expand_home)
            .unwrap_or_else(|| paths.config_dir.join("sessions"));
        let whisper_cli_path = self
            .whisper_cli_path
            .map(expand_home)
            .or_else(detect_whisper_cli_path);
        let whisper_model_path = self
            .whisper_model_path
            .map(expand_home)
            .or_else(|| detect_whisper_model_path(whisper_cli_path.as_deref()));

        EffectiveConfig {
            ffmpeg_path: expand_home(PathBuf::from(self.ffmpeg_path)),
            whisper_cli_path,
            whisper_model_path,
            artifacts_dir,
            partial_interval_seconds: self.partial_interval_seconds.max(1),
            input_mode: self.input_mode,
            display_id: self.display_id,
            microphone_device: self.microphone_device,
            system_device: self.system_device,
            single_input_device: self.single_input_device,
            config_file: paths.config_file.clone(),
        }
    }

    pub fn set_value(&mut self, key: ConfigKey, raw: &str) -> Result<()> {
        match key {
            ConfigKey::FfmpegPath => self.ffmpeg_path = raw.to_string(),
            ConfigKey::WhisperCliPath => self.whisper_cli_path = Some(PathBuf::from(raw)),
            ConfigKey::WhisperModelPath => self.whisper_model_path = Some(PathBuf::from(raw)),
            ConfigKey::ArtifactsDir => self.artifacts_dir = Some(PathBuf::from(raw)),
            ConfigKey::PartialIntervalSeconds => {
                let seconds = raw
                    .parse::<u64>()
                    .context("partial interval must be an integer number of seconds")?;
                if seconds == 0 {
                    bail!("partial interval must be at least 1 second");
                }
                self.partial_interval_seconds = seconds;
            }
            ConfigKey::InputMode => {
                self.input_mode = match raw {
                    "mic_system_mix" => InputMode::MicSystemMix,
                    "single_device" => InputMode::SingleDevice,
                    _ => bail!("input mode must be `mic_system_mix` or `single_device`"),
                };
            }
            ConfigKey::DisplayId => {
                self.display_id = Some(
                    raw.parse::<u32>()
                        .context("display ID must be an integer")?,
                );
            }
            ConfigKey::MicrophoneDevice => self.microphone_device = Some(raw.to_string()),
            ConfigKey::SystemDevice => self.system_device = Some(raw.to_string()),
            ConfigKey::SingleInputDevice => self.single_input_device = Some(raw.to_string()),
        }

        Ok(())
    }

    pub fn unset_value(&mut self, key: ConfigKey) {
        match key {
            ConfigKey::FfmpegPath => self.ffmpeg_path = default_ffmpeg_path(),
            ConfigKey::WhisperCliPath => self.whisper_cli_path = None,
            ConfigKey::WhisperModelPath => self.whisper_model_path = None,
            ConfigKey::ArtifactsDir => self.artifacts_dir = None,
            ConfigKey::PartialIntervalSeconds => {
                self.partial_interval_seconds = default_partial_interval_seconds()
            }
            ConfigKey::InputMode => self.input_mode = InputMode::MicSystemMix,
            ConfigKey::DisplayId => self.display_id = None,
            ConfigKey::MicrophoneDevice => self.microphone_device = None,
            ConfigKey::SystemDevice => self.system_device = None,
            ConfigKey::SingleInputDevice => self.single_input_device = None,
        }
    }
}

impl EffectiveConfig {
    pub fn get_value(&self, key: ConfigKey) -> Value {
        match key {
            ConfigKey::FfmpegPath => json!(self.ffmpeg_path),
            ConfigKey::WhisperCliPath => json!(self.whisper_cli_path),
            ConfigKey::WhisperModelPath => json!(self.whisper_model_path),
            ConfigKey::ArtifactsDir => json!(self.artifacts_dir),
            ConfigKey::PartialIntervalSeconds => json!(self.partial_interval_seconds),
            ConfigKey::InputMode => json!(self.input_mode),
            ConfigKey::DisplayId => json!(self.display_id),
            ConfigKey::MicrophoneDevice => json!(self.microphone_device),
            ConfigKey::SystemDevice => json!(self.system_device),
            ConfigKey::SingleInputDevice => json!(self.single_input_device),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConfigKey {
    FfmpegPath,
    WhisperCliPath,
    WhisperModelPath,
    ArtifactsDir,
    PartialIntervalSeconds,
    InputMode,
    DisplayId,
    MicrophoneDevice,
    SystemDevice,
    SingleInputDevice,
}

impl ConfigKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FfmpegPath => "ffmpeg_path",
            Self::WhisperCliPath => "whisper_cli_path",
            Self::WhisperModelPath => "whisper_model_path",
            Self::ArtifactsDir => "artifacts_dir",
            Self::PartialIntervalSeconds => "partial_interval_seconds",
            Self::InputMode => "input_mode",
            Self::DisplayId => "display_id",
            Self::MicrophoneDevice => "microphone_device",
            Self::SystemDevice => "system_device",
            Self::SingleInputDevice => "single_input_device",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    pub paths: AppPaths,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            paths: AppPaths::discover()?,
        })
    }

    pub fn load(&self) -> Result<ConfigFile> {
        if !self.paths.config_file.exists() {
            return Ok(ConfigFile::default());
        }

        let raw = fs::read_to_string(&self.paths.config_file).with_context(|| {
            format!(
                "failed to read config file {}",
                self.paths.config_file.display()
            )
        })?;

        toml::from_str(&raw).with_context(|| {
            format!(
                "failed to parse config file {}",
                self.paths.config_file.display()
            )
        })
    }

    pub fn save(&self, config: &ConfigFile) -> Result<()> {
        self.paths.ensure()?;
        let raw = toml::to_string_pretty(config).context("failed to serialize config")?;
        fs::write(&self.paths.config_file, raw).with_context(|| {
            format!(
                "failed to write config file {}",
                self.paths.config_file.display()
            )
        })
    }

    pub fn load_effective(&self) -> Result<EffectiveConfig> {
        Ok(self.load()?.into_effective(&self.paths))
    }
}

fn default_ffmpeg_path() -> String {
    "ffmpeg".to_string()
}

fn default_partial_interval_seconds() -> u64 {
    15
}

fn project_config_dir(qualifier: &str, organization: &str, app_name: &str) -> Result<AppPaths> {
    let dirs = ProjectDirs::from(qualifier, organization, app_name)
        .ok_or_else(|| anyhow!("failed to determine config directory for {app_name}"))?;
    Ok(AppPaths::from_base(dirs.config_dir().to_path_buf()))
}

pub fn expand_home(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home);
        }
    }

    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    path
}

pub(crate) fn detect_whisper_cli_path() -> Option<PathBuf> {
    if let Ok(raw) = env::var("WHISPER_CLI_PATH") {
        let path = expand_home(PathBuf::from(raw));
        if path.is_file() {
            return Some(path);
        }
    }

    if let Some(path) = detect_bundled_whisper_cli_path() {
        return Some(path);
    }

    if let Some(path) = find_binary_on_path("whisper-cli") {
        return Some(path);
    }

    for path in [
        "/opt/homebrew/bin/whisper-cli",
        "/usr/local/bin/whisper-cli",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    None
}

fn detect_bundled_whisper_cli_path() -> Option<PathBuf> {
    bundled_whisper_cli_candidates(std::env::current_exe().ok()?.as_path())
        .into_iter()
        .find(|path| path.is_file())
}

fn bundled_whisper_cli_candidates(exe_path: &Path) -> Vec<PathBuf> {
    let exe = fs::canonicalize(exe_path).unwrap_or_else(|_| exe_path.to_path_buf());
    let mut candidates = Vec::new();

    if let Some(dir) = exe.parent() {
        candidates.push(dir.join("whisper-cli"));
        candidates.push(dir.join("../libexec/whisper-cli"));
        candidates.push(dir.join("../libexec/scribecli/whisper-cli"));
    }

    candidates
}

fn detect_whisper_model_path(whisper_cli_path: Option<&Path>) -> Option<PathBuf> {
    if let Ok(raw) = env::var("WHISPER_MODEL_PATH") {
        let path = expand_home(PathBuf::from(raw));
        if path.is_file() {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(model) = select_model_from_dir(&path) {
                return Some(model);
            }
        }
    }

    let mut candidate_dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        append_unique_dir(
            &mut candidate_dirs,
            home.join("Library/Application Support/superwhisper"),
        );
        append_unique_dir(
            &mut candidate_dirs,
            home.join("Library/Application Support/whisper.cpp"),
        );
        append_unique_dir(&mut candidate_dirs, home.join(".cache/whisper.cpp"));
        append_unique_dir(&mut candidate_dirs, home.join(".cache/whisper"));
    }

    if let Some(cli_path) = whisper_cli_path {
        for dir in model_dirs_near_cli(cli_path) {
            append_unique_dir(&mut candidate_dirs, dir);
        }
    }

    for dir in candidate_dirs {
        if let Some(model) = select_model_from_dir(&dir) {
            return Some(model);
        }
    }

    None
}

fn append_unique_dir(dirs: &mut Vec<PathBuf>, candidate: PathBuf) {
    if candidate.is_dir() && !dirs.iter().any(|existing| existing == &candidate) {
        dirs.push(candidate);
    }
}

fn model_dirs_near_cli(cli_path: &Path) -> Vec<PathBuf> {
    let base = fs::canonicalize(cli_path).unwrap_or_else(|_| cli_path.to_path_buf());
    let mut dirs = Vec::new();

    for ancestor in base.ancestors().take(6) {
        append_unique_dir(&mut dirs, ancestor.join("models"));
        append_unique_dir(&mut dirs, ancestor.join("share/whisper/models"));
        append_unique_dir(&mut dirs, ancestor.join("share/whisper.cpp/models"));
    }

    dirs
}

fn find_binary_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn select_model_from_dir(dir: &Path) -> Option<PathBuf> {
    let mut models = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| is_whisper_model_file(path))
        .collect::<Vec<_>>();

    models.sort_by_key(|path| model_sort_key(path));
    models.into_iter().next()
}

pub(crate) fn is_whisper_model_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let lower = file_name.to_ascii_lowercase();
    (lower.ends_with(".bin") || lower.ends_with(".gguf"))
        && (lower.contains("ggml") || lower.contains("whisper"))
}

fn model_sort_key(path: &Path) -> (u8, u8, String) {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let size_rank = if file_name.contains("large-v3-turbo") {
        0
    } else if file_name.contains("large-v3") {
        1
    } else if file_name.contains("large-v2") {
        2
    } else if file_name.contains("large") {
        3
    } else if file_name.contains("medium") {
        4
    } else if file_name.contains("small") {
        5
    } else if file_name.contains("base") {
        6
    } else if file_name.contains("tiny") {
        7
    } else {
        8
    };
    let format_rank = if file_name.ends_with(".bin") { 0 } else { 1 };
    (size_rank, format_rank, file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn config_roundtrip_and_effective_defaults() {
        let temp = tempdir().expect("tempdir");
        let paths = AppPaths::from_base(temp.path().join("cfg"));
        let store = ConfigStore {
            paths: paths.clone(),
        };

        let mut config = ConfigFile::default();
        config
            .set_value(ConfigKey::WhisperCliPath, "/tmp/whisper-cli")
            .unwrap();
        config
            .set_value(ConfigKey::PartialIntervalSeconds, "5")
            .unwrap();
        store.save(&config).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.partial_interval_seconds, 5);
        assert_eq!(
            loaded.whisper_cli_path,
            Some(PathBuf::from("/tmp/whisper-cli"))
        );

        let effective = loaded.into_effective(&paths);
        assert_eq!(effective.ffmpeg_path, PathBuf::from("ffmpeg"));
        assert_eq!(effective.artifacts_dir, paths.config_dir.join("sessions"));
    }

    #[test]
    fn unset_resets_to_defaults() {
        let mut config = ConfigFile::default();
        config
            .set_value(ConfigKey::SystemDevice, "System Capture")
            .unwrap();
        config.unset_value(ConfigKey::SystemDevice);
        assert_eq!(config.system_device, None);
    }

    #[test]
    fn effective_config_auto_detects_whisper_paths_from_env() {
        let _guard = env_lock().lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempdir().unwrap();
        let paths = AppPaths::from_base(temp.path().join("cfg"));
        let whisper_cli = temp.path().join("whisper-cli");
        let whisper_model = temp.path().join("ggml-large-v3-turbo.bin");
        fs::write(&whisper_cli, "#!/bin/sh\n").unwrap();
        fs::write(&whisper_model, "model").unwrap();

        unsafe {
            env::set_var("WHISPER_CLI_PATH", &whisper_cli);
            env::set_var("WHISPER_MODEL_PATH", &whisper_model);
        }

        let effective = ConfigFile::default().into_effective(&paths);
        assert_eq!(effective.whisper_cli_path, Some(whisper_cli.clone()));
        assert_eq!(effective.whisper_model_path, Some(whisper_model.clone()));

        unsafe {
            env::remove_var("WHISPER_CLI_PATH");
            env::remove_var("WHISPER_MODEL_PATH");
        }
    }

    #[test]
    fn prefers_best_model_in_known_directory() {
        let temp = tempdir().unwrap();
        let models = temp.path().join("models");
        fs::create_dir_all(&models).unwrap();
        let small = models.join("ggml-small.en.bin");
        let turbo = models.join("ggml-large-v3-turbo.bin");
        fs::write(&small, "small").unwrap();
        fs::write(&turbo, "turbo").unwrap();

        let selected = select_model_from_dir(&models).unwrap();
        assert_eq!(selected, turbo);
    }

    #[test]
    fn bundled_whisper_candidates_cover_release_layouts() {
        let base = PathBuf::from("/tmp/scribecli-release/bin/scribecli");
        let candidates = bundled_whisper_cli_candidates(&base);

        assert!(candidates.contains(&PathBuf::from("/tmp/scribecli-release/bin/whisper-cli")));
        assert!(candidates.contains(&PathBuf::from(
            "/tmp/scribecli-release/bin/../libexec/whisper-cli"
        )));
        assert!(candidates.contains(&PathBuf::from(
            "/tmp/scribecli-release/bin/../libexec/scribecli/whisper-cli"
        )));
    }
}
