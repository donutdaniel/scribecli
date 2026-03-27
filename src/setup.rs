use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use reqwest::blocking::Client;
use serde::Serialize;

use crate::config::{ConfigStore, EffectiveConfig, detect_whisper_cli_path};
use crate::whisper::whisper_version;

const DEFAULT_MODEL: ManagedModel = ManagedModel::SmallEn;
#[derive(Debug, Clone, Copy, Serialize, ValueEnum, PartialEq, Eq)]
pub enum ManagedModel {
    BaseEn,
    SmallEn,
    MediumEn,
    LargeV3Turbo,
}

impl ManagedModel {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::BaseEn => "base-en",
            Self::SmallEn => "small-en",
            Self::MediumEn => "medium-en",
            Self::LargeV3Turbo => "large-v3-turbo",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Self::BaseEn => "ggml-base.en.bin",
            Self::SmallEn => "ggml-small.en.bin",
            Self::MediumEn => "ggml-medium.en.bin",
            Self::LargeV3Turbo => "ggml-large-v3-turbo.bin",
        }
    }

    pub fn download_url(self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}?download=true",
            self.file_name()
        )
    }

    pub fn from_file_name(path: &Path) -> Option<Self> {
        match path.file_name().and_then(|value| value.to_str()) {
            Some("ggml-base.en.bin") => Some(Self::BaseEn),
            Some("ggml-small.en.bin") => Some(Self::SmallEn),
            Some("ggml-medium.en.bin") => Some(Self::MediumEn),
            Some("ggml-large-v3-turbo.bin") => Some(Self::LargeV3Turbo),
            _ => None,
        }
    }

    pub fn from_cli_input(raw: &str) -> Option<Self> {
        match raw {
            "base-en" => Some(Self::BaseEn),
            "small-en" => Some(Self::SmallEn),
            "medium-en" => Some(Self::MediumEn),
            "large-v3-turbo" => Some(Self::LargeV3Turbo),
            _ => Self::from_file_name(Path::new(raw)),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelAction {
    ReusedManaged,
    CopiedExisting,
    Downloaded,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliAction {
    ReusedManaged,
    WrappedExisting,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupReport {
    pub object: &'static str,
    pub ok: bool,
    pub managed_bin_dir: PathBuf,
    pub managed_model_dir: PathBuf,
    pub whisper_cli_path: Option<PathBuf>,
    pub whisper_cli_action: CliAction,
    pub whisper_model_path: PathBuf,
    pub model_action: ModelAction,
    pub model_name: String,
    pub config_file: PathBuf,
    pub warnings: Vec<String>,
}

#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  scribecli setup\n  scribecli setup --model large-v3-turbo\n  scribecli setup --model medium-en --force-download"
)]
pub struct SetupArgs {
    /// Model to place under scribecli's managed models directory.
    #[arg(long, value_enum)]
    pub model: Option<ManagedModel>,
    /// Re-download the selected model even if a managed copy already exists.
    #[arg(long)]
    pub force_download: bool,
}

struct ModelInstall {
    path: PathBuf,
    action: ModelAction,
    model_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedModelInstall {
    pub path: PathBuf,
    pub action: ModelAction,
    pub model_name: String,
}

#[derive(Debug)]
struct CliInstall {
    path: Option<PathBuf>,
    action: CliAction,
}

pub fn run_setup(store: &ConfigStore, args: &SetupArgs) -> Result<SetupReport> {
    let mut config = store.load()?;
    let effective = store.load_effective()?;
    let managed_bin_dir = store.paths.config_dir.join("bin");
    let managed_model_dir = store.paths.config_dir.join("models");
    fs::create_dir_all(&managed_bin_dir)
        .with_context(|| format!("failed to create {}", managed_bin_dir.display()))?;
    fs::create_dir_all(&managed_model_dir)
        .with_context(|| format!("failed to create {}", managed_model_dir.display()))?;

    let cli_install = install_cli(&managed_bin_dir, existing_cli_source(&effective))?;
    let warnings = Vec::new();

    let installed = ensure_managed_model(
        &managed_model_dir,
        args.model,
        args.force_download,
        existing_model_path(&effective),
    )?;

    config.whisper_model_path = Some(installed.path.clone());
    config.whisper_cli_path = cli_install.path.clone();
    store.save(&config)?;

    Ok(SetupReport {
        object: "setup",
        ok: true,
        managed_bin_dir,
        managed_model_dir,
        whisper_cli_path: cli_install.path,
        whisper_cli_action: cli_install.action,
        whisper_model_path: installed.path,
        model_action: installed.action,
        model_name: installed.model_name,
        config_file: store.paths.config_file.clone(),
        warnings,
    })
}

pub fn ensure_managed_model(
    managed_model_dir: &Path,
    requested_model: Option<ManagedModel>,
    force_download: bool,
    existing_model_path: Option<PathBuf>,
) -> Result<ManagedModelInstall> {
    let installed = install_model(
        managed_model_dir,
        requested_model,
        force_download,
        existing_model_path,
    )?;

    Ok(ManagedModelInstall {
        path: installed.path,
        action: installed.action,
        model_name: installed.model_name,
    })
}

fn existing_cli_path(config: &EffectiveConfig) -> Option<PathBuf> {
    config
        .whisper_cli_path
        .as_ref()
        .filter(|path| path.is_file())
        .cloned()
}

fn existing_cli_source(config: &EffectiveConfig) -> Option<PathBuf> {
    if let Some(path) = existing_cli_path(config).filter(|path| whisper_version(path).is_ok()) {
        return Some(path);
    }

    detect_whisper_cli_path().filter(|path| whisper_version(path).is_ok())
}

pub fn existing_model_path(config: &EffectiveConfig) -> Option<PathBuf> {
    config
        .whisper_model_path
        .as_ref()
        .filter(|path| path.is_file())
        .cloned()
}

pub fn list_managed_model_paths(managed_model_dir: &Path) -> Result<Vec<PathBuf>> {
    if !managed_model_dir.exists() {
        return Ok(Vec::new());
    }

    let mut paths = fs::read_dir(managed_model_dir)
        .with_context(|| format!("failed to read {}", managed_model_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();

    paths.sort_by(|left, right| {
        left.file_name()
            .and_then(|value| value.to_str())
            .cmp(&right.file_name().and_then(|value| value.to_str()))
    });

    Ok(paths)
}

fn install_cli(managed_bin_dir: &Path, existing_cli_path: Option<PathBuf>) -> Result<CliInstall> {
    let managed_names = ["whisper-cli"];
    for name in managed_names {
        let candidate = managed_bin_dir.join(name);
        if candidate.is_file() && whisper_version(&candidate).is_ok() {
            return Ok(CliInstall {
                path: Some(candidate),
                action: CliAction::ReusedManaged,
            });
        }
        if candidate.exists() {
            let _ = fs::remove_file(&candidate);
        }
    }

    if let Some(existing_cli_path) = existing_cli_path {
        return install_cli_wrapper(
            managed_bin_dir,
            &existing_cli_path,
            CliAction::WrappedExisting,
        );
    }

    Err(anyhow!(
        "whisper-cli was not found. Use a bundled scribecli release, or install whisper-cpp first, for example `brew install whisper-cpp`, then rerun `scribecli setup`"
    ))
}

fn install_cli_wrapper(
    managed_bin_dir: &Path,
    source_path: &Path,
    action: CliAction,
) -> Result<CliInstall> {
    let target_path = managed_bin_dir.join("whisper-cli");
    if target_path.exists() {
        fs::remove_file(&target_path)
            .with_context(|| format!("failed to remove {}", target_path.display()))?;
    }
    write_cli_wrapper(source_path, &target_path)?;

    Ok(CliInstall {
        path: Some(target_path),
        action,
    })
}

fn install_model(
    managed_model_dir: &Path,
    requested_model: Option<ManagedModel>,
    force_download: bool,
    existing_model_path: Option<PathBuf>,
) -> Result<ModelInstall> {
    let selection = resolve_model_selection(requested_model, existing_model_path.as_deref())?;
    let target_path = managed_model_dir.join(&selection.file_name);

    if target_path.is_file() && !force_download {
        return Ok(ModelInstall {
            path: target_path,
            action: ModelAction::ReusedManaged,
            model_name: selection.file_name,
        });
    }

    if !force_download {
        if let Some(source_path) = selection.copy_source {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy existing Whisper model from {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
            return Ok(ModelInstall {
                path: target_path,
                action: ModelAction::CopiedExisting,
                model_name: selection.file_name,
            });
        }
    }

    let managed_model = selection
        .download_model
        .ok_or_else(|| anyhow!("no download URL is available for {}", selection.file_name))?;
    download_model(&managed_model.download_url(), &target_path)?;
    Ok(ModelInstall {
        path: target_path,
        action: ModelAction::Downloaded,
        model_name: selection.file_name,
    })
}

struct ModelSelection {
    file_name: String,
    copy_source: Option<PathBuf>,
    download_model: Option<ManagedModel>,
}

fn resolve_model_selection(
    requested_model: Option<ManagedModel>,
    existing_model_path: Option<&Path>,
) -> Result<ModelSelection> {
    if let Some(requested_model) = requested_model {
        let file_name = requested_model.file_name().to_string();
        let copy_source = existing_model_path.and_then(|path| {
            if path.file_name().and_then(|value| value.to_str()) == Some(file_name.as_str()) {
                Some(path.to_path_buf())
            } else {
                None
            }
        });
        return Ok(ModelSelection {
            file_name,
            copy_source,
            download_model: Some(requested_model),
        });
    }

    if let Some(existing_model_path) = existing_model_path {
        let file_name = existing_model_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("existing model path has no file name"))?
            .to_string();
        return Ok(ModelSelection {
            download_model: ManagedModel::from_file_name(existing_model_path),
            file_name,
            copy_source: Some(existing_model_path.to_path_buf()),
        });
    }

    Ok(ModelSelection {
        file_name: DEFAULT_MODEL.file_name().to_string(),
        copy_source: None,
        download_model: Some(DEFAULT_MODEL),
    })
}

fn download_model(url: &str, destination: &Path) -> Result<()> {
    let client = Client::builder()
        .build()
        .context("failed to initialize HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download model from {url}"))?
        .error_for_status()
        .with_context(|| format!("model download request failed for {url}"))?;

    let temp_path = destination.with_extension("download");
    let mut file = File::create(&temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = response
            .read(&mut buffer)
            .context("failed to read model download response")?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", temp_path.display()))?;

    fs::rename(&temp_path, destination).with_context(|| {
        format!(
            "failed to move downloaded model into place at {}",
            destination.display()
        )
    })?;

    Ok(())
}

fn write_cli_wrapper(source: &Path, target: &Path) -> Result<()> {
    let escaped = source.to_string_lossy().replace('\'', "'\\''");
    let script = format!("#!/bin/sh\nexec '{escaped}' \"$@\"\n");
    fs::write(target, script).with_context(|| format!("failed to write {}", target.display()))?;
    make_executable(target)?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn write_fake_cli(path: &Path) {
        fs::write(
            path,
            "#!/bin/sh\nif [ \"$1\" = \"-h\" ]; then echo 'whisper-cli help'; exit 0; fi\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn setup_copies_existing_model_into_managed_dir() {
        let temp = tempdir().unwrap();
        let models_dir = temp.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        let existing = temp.path().join("ggml-large-v3-turbo.bin");
        fs::write(&existing, "model").unwrap();

        let installed = install_model(&models_dir, None, false, Some(existing.clone())).unwrap();
        assert_eq!(installed.action, ModelAction::CopiedExisting);
        assert_eq!(installed.path, models_dir.join("ggml-large-v3-turbo.bin"));
        assert!(installed.path.exists());
    }

    #[test]
    fn requested_model_downloads_when_existing_name_differs() {
        let existing = PathBuf::from("/tmp/ggml-large-v3-turbo.bin");
        let selection =
            resolve_model_selection(Some(ManagedModel::SmallEn), Some(&existing)).unwrap();
        assert_eq!(selection.file_name, "ggml-small.en.bin");
        assert!(selection.copy_source.is_none());
        assert_eq!(selection.download_model, Some(ManagedModel::SmallEn));
    }

    #[test]
    fn existing_managed_model_is_reused() {
        let temp = tempdir().unwrap();
        let models_dir = temp.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        let managed = models_dir.join("ggml-small.en.bin");
        fs::write(&managed, "model").unwrap();

        let installed = install_model(&models_dir, None, false, Some(managed.clone())).unwrap();
        assert_eq!(installed.action, ModelAction::ReusedManaged);
        assert_eq!(installed.path, managed);
    }

    #[test]
    fn setup_copies_existing_cli_into_managed_bin_dir() {
        let temp = tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let existing = temp.path().join("whisper-cli");
        write_fake_cli(&existing);

        let installed = install_cli(&bin_dir, Some(existing.clone())).unwrap();
        assert_eq!(installed.action, CliAction::WrappedExisting);
        assert_eq!(installed.path, Some(bin_dir.join("whisper-cli")));
        assert!(bin_dir.join("whisper-cli").exists());
    }

    #[test]
    fn existing_managed_cli_is_reused() {
        let temp = tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let managed = bin_dir.join("whisper-cli");
        write_fake_cli(&managed);

        let installed = install_cli(&bin_dir, None).unwrap();
        assert_eq!(installed.action, CliAction::ReusedManaged);
        assert_eq!(installed.path, Some(managed));
    }

    #[test]
    fn setup_errors_when_cli_is_missing() {
        let temp = tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let error = install_cli(&bin_dir, None).unwrap_err().to_string();
        assert!(error.contains("bundled scribecli release"));
        assert!(error.contains("brew install whisper-cpp"));
    }

    #[test]
    fn managed_model_accepts_cli_alias_and_file_name() {
        assert_eq!(
            ManagedModel::from_cli_input("small-en"),
            Some(ManagedModel::SmallEn)
        );
        assert_eq!(
            ManagedModel::from_cli_input("ggml-large-v3-turbo.bin"),
            Some(ManagedModel::LargeV3Turbo)
        );
        assert!(ManagedModel::from_cli_input("unknown-model").is_none());
    }
}
