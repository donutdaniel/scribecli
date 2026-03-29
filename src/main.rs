mod audio;
mod config;
#[cfg(target_os = "macos")]
mod native_macos;
mod output;
mod session;
mod setup;
mod whisper;

use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::parse;
use tracing_subscriber::EnvFilter;

use crate::audio::{ffmpeg_version, mix_audio_files};
use crate::config::{
    ConfigKey, ConfigStore, EffectiveConfig, InputMode, expand_home, is_whisper_model_file,
    select_model_from_dir,
};
#[cfg(target_os = "macos")]
use crate::native_macos::{
    NativeDisplay, NativeRecordRequest, ensure_helper as ensure_native_helper,
    spawn_record as spawn_native_record, stop_record_process as stop_native_record_process,
    wait_for_record_output as wait_for_native_record_output,
};
#[cfg(target_os = "macos")]
use crate::native_macos::{doctor as native_doctor, list_displays as native_list_displays};
use crate::output::OutputFormat;
use crate::session::{EventLogger, SessionPaths, SessionResult, now_rfc3339, write_pretty_json};
use crate::setup::{
    ManagedModel, ModelAction, SetupArgs, ensure_managed_model,
    existing_model_path as current_model_path, list_managed_model_paths, run_setup,
};
use crate::whisper::{
    WhisperConfig, apply_basic_speaker_labels, transcribe_audio, whisper_version,
};
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Serialize)]
struct NativeDisplay {
    display_id: u32,
    width: i64,
    height: i64,
    is_main: bool,
}

#[derive(Parser)]
#[command(
    name = "scribecli",
    version,
    about = "Local meeting recording and transcription for agents",
    long_about = "Local meeting recording and transcription for agents.\n\nRun `scribecli setup` once to resolve a bundled or existing `whisper-cli` and manage a model under scribecli's config directory. Then use `scribecli doctor` to verify native macOS capture readiness. The primary workflow is `scribecli record` for a one-shot foreground session. `scribecli start` / `scribecli stop` are optional wrappers for agents that prefer split lifecycle control, and `scribecli transcribe` is the recovery path for existing audio."
)]
struct Cli {
    /// Output format for command results. The default is human-readable.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    output: OutputFormat,
    /// Increase log verbosity. Repeat for more detail.
    #[arg(long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone)]
struct GlobalOptions {
    output: OutputFormat,
}

#[derive(Subcommand)]
enum Commands {
    #[command(
        about = "Primary one-shot workflow: record audio, transcribe locally, persist artifacts, and print the final session result"
    )]
    Record(Box<RecordArgs>),
    #[command(
        about = "Transcribe an existing session directory or audio file and write a transcript artifact"
    )]
    Transcribe(TranscribeArgs),
    #[command(about = "Optional split-lifecycle wrapper: start a background recording session")]
    Start(Box<StartArgs>),
    #[command(
        about = "Optional split-lifecycle wrapper: stop the active background recording and print the final session result"
    )]
    Stop(StopArgs),
    #[command(about = "List available capture devices")]
    Devices(DevicesArgs),
    #[command(about = "Check local prerequisites and configured devices")]
    Doctor,
    #[command(
        about = "Prepare scribecli by resolving whisper-cli and managing a Whisper model under its own config directory"
    )]
    Setup(SetupArgs),
    #[command(about = "List, select, and remove managed Whisper models")]
    Model(ModelArgs),
    #[command(about = "Remove managed scribecli state such as models, sessions, or helpers")]
    Cleanup(CleanupArgs),
    #[command(about = "Read and update persisted local configuration")]
    Config(ConfigArgs),
}

#[derive(Args)]
struct DevicesArgs {
    #[command(subcommand)]
    command: DevicesCommand,
}

#[derive(Subcommand)]
enum DevicesCommand {
    #[command(about = "List native macOS displays available for capture")]
    List,
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    #[command(about = "Print effective config or a single stored config value")]
    Get(ConfigGetArgs),
    #[command(about = "Set a persisted config value")]
    Set(ConfigSetArgs),
    #[command(about = "Unset a persisted config value and fall back to the default")]
    Unset(ConfigUnsetArgs),
}

#[derive(Args)]
struct ConfigGetArgs {
    #[arg(value_enum)]
    key: Option<ConfigKey>,
}

#[derive(Args)]
struct ConfigSetArgs {
    #[arg(value_enum)]
    key: ConfigKey,
    value: String,
}

#[derive(Args)]
struct ConfigUnsetArgs {
    #[arg(value_enum)]
    key: ConfigKey,
}

#[derive(Args)]
struct ModelArgs {
    #[command(subcommand)]
    command: ModelCommand,
}

#[derive(Subcommand)]
enum ModelCommand {
    #[command(about = "List managed models under scribecli's app data directory")]
    List,
    #[command(about = "Select a managed model by name, or point at an existing local model file")]
    Use(ModelUseArgs),
    #[command(about = "Remove one managed model, or all managed models")]
    Remove(ModelRemoveArgs),
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli model use small-en\n  scribecli model use large-v3-turbo --force-download\n  scribecli model use ~/Downloads/ggml-large-v3-turbo.bin"
)]
struct ModelUseArgs {
    /// Managed model name like `small-en`, or a path to an existing model file.
    target: String,
    /// Re-download the managed model if it already exists locally.
    #[arg(long)]
    force_download: bool,
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli model remove small-en\n  scribecli model remove ggml-large-v3-turbo.bin\n  scribecli model remove --all"
)]
struct ModelRemoveArgs {
    /// Managed model name or file name to remove.
    target: Option<String>,
    /// Remove every managed model under scribecli's models directory.
    #[arg(long)]
    all: bool,
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli cleanup --sessions\n  scribecli cleanup --models --helper\n  scribecli cleanup --all"
)]
struct CleanupArgs {
    /// Remove all managed scribecli state under the app data directory.
    #[arg(long)]
    all: bool,
    /// Remove managed model files under the app data directory.
    #[arg(long)]
    models: bool,
    /// Remove managed session artifacts under the app data directory.
    #[arg(long)]
    sessions: bool,
    /// Remove the managed whisper-cli wrapper directory.
    #[arg(long)]
    bin: bool,
    /// Remove the compiled native helper directory.
    #[arg(long)]
    helper: bool,
    /// Remove config.toml and active-recording state.
    #[arg(long)]
    config: bool,
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli record\n  scribecli --output json record --duration-seconds 30\n  scribecli record --input-mode microphone\n  scribecli record --input-mode mic-system-mix\n  scribecli record --input-mode system-audio\n  scribecli record --display-id 1 --duration-seconds 15"
)]
struct RecordArgs {
    /// Override the configured input mode for this session.
    #[arg(long, value_enum)]
    input_mode: Option<InputMode>,
    /// Override the configured display ID used for native macOS system audio capture.
    #[arg(long)]
    display_id: Option<u32>,
    /// Override the configured microphone device name.
    #[arg(long)]
    microphone_device: Option<String>,
    /// Override the configured system capture device name.
    #[arg(long)]
    system_device: Option<String>,
    /// Override the configured single input device for microphone mode.
    #[arg(long)]
    single_input_device: Option<String>,
    /// Override the partial chunk interval in seconds.
    #[arg(long)]
    partial_interval_seconds: Option<u64>,
    /// Override the artifact root directory.
    #[arg(long)]
    artifacts_dir: Option<PathBuf>,
    /// Override the ffmpeg binary path used for mic-system-mix audio mixing.
    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,
    /// Override the whisper.cpp CLI binary path.
    #[arg(long)]
    whisper_cli_path: Option<PathBuf>,
    /// Override the whisper.cpp model path.
    #[arg(long)]
    whisper_model_path: Option<PathBuf>,
    /// Optional session name prefix to make artifact folders easier to identify.
    #[arg(long)]
    session_name: Option<String>,
    /// Optional non-default stop condition for automation and smoke tests.
    #[arg(long)]
    duration_seconds: Option<u64>,
    /// Internal session ID override used by background start/stop flows.
    #[arg(long, hide = true)]
    session_id: Option<String>,
    /// Internal stop request path used by background start/stop flows.
    #[arg(long, hide = true)]
    stop_request_path: Option<PathBuf>,
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli start\n  scribecli --output json start --session-name weekly-sync\n  scribecli start --duration-seconds 1800"
)]
struct StartArgs {
    #[command(flatten)]
    record: RecordArgs,
}

#[derive(Args, Clone)]
struct StopArgs {
    /// Maximum time to wait for transcription and finalization after requesting stop.
    #[arg(long, default_value_t = 300)]
    wait_timeout_seconds: u64,
}

#[derive(Args, Clone)]
#[command(
    after_help = "Examples:\n  scribecli transcribe ~/Library/Application\\ Support/com.scribecli.scribecli/sessions/weekly-sync-20260326T120000Z-1234\n  scribecli transcribe ./meeting.wav\n  scribecli transcribe weekly-sync-20260326T120000Z-1234"
)]
struct TranscribeArgs {
    /// Existing session directory, audio file, or session ID under the configured artifacts dir.
    input: PathBuf,
    /// Optional explicit output path for the transcript JSON.
    #[arg(long)]
    transcript_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DevicesReport {
    object: &'static str,
    native_screen_capture_access: bool,
    native_displays: Vec<NativeDisplay>,
}

#[derive(Debug, Serialize)]
struct ConfigReport {
    object: &'static str,
    config: EffectiveConfig,
}

#[derive(Debug, Serialize)]
struct ConfigValueReport {
    object: &'static str,
    key: &'static str,
    value: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ModelEntry {
    name: String,
    file_name: String,
    path: PathBuf,
    size_bytes: u64,
    active: bool,
}

#[derive(Debug, Serialize)]
struct ModelListReport {
    object: &'static str,
    managed_model_dir: PathBuf,
    active_model_path: Option<PathBuf>,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
struct ModelUseReport {
    object: &'static str,
    status: &'static str,
    model_path: PathBuf,
    managed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_action: Option<ModelAction>,
    config_file: PathBuf,
}

#[derive(Debug, Serialize)]
struct ModelRemoveReport {
    object: &'static str,
    status: &'static str,
    removed_paths: Vec<PathBuf>,
    active_model_path: Option<PathBuf>,
    config_file: PathBuf,
}

#[derive(Debug, Serialize)]
struct CleanupReport {
    object: &'static str,
    status: &'static str,
    config_dir: PathBuf,
    removed_paths: Vec<PathBuf>,
    bytes_removed: u64,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    message: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    object: &'static str,
    ok: bool,
    checks: Vec<DoctorCheck>,
    native_displays: Vec<NativeDisplay>,
    config: EffectiveConfig,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let global = GlobalOptions { output: cli.output };

    if let Err(error) = run(&global, cli.command).await {
        let message = format!("{error:#}");
        if let Err(render_error) = global.output.print_error(1, "cli_error", &message) {
            eprintln!("cli_error (1): {message}");
            eprintln!("render_error: {render_error:#}");
        }
        std::process::exit(1);
    }
}

async fn run(global: &GlobalOptions, command: Commands) -> Result<()> {
    match command {
        Commands::Start(args) => {
            let report = run_start(&args.record).await?;
            global.output.print_success(&report)?;
        }
        Commands::Stop(args) => {
            let result = run_stop(args).await?;
            global.output.print_success(&result)?;
        }
        Commands::Record(args) => {
            let store = ConfigStore::discover()?;
            let config = apply_record_overrides(store.load_effective()?, &args);
            let result = run_record(&config, &args).await?;
            global.output.print_success(&result)?;
        }
        Commands::Transcribe(args) => {
            let store = ConfigStore::discover()?;
            let config = store.load_effective()?;
            let report = run_transcribe(&config, &args)?;
            global.output.print_success(&report)?;
        }
        Commands::Devices(args) => match args.command {
            DevicesCommand::List => {
                let config = ConfigStore::discover()?.load_effective()?;
                let report = build_devices_report(&config)?;
                global.output.print_success(&report)?;
            }
        },
        Commands::Doctor => {
            let config = ConfigStore::discover()?.load_effective()?;
            let report = build_doctor_report(&config)?;
            global.output.print_success(&report)?;
        }
        Commands::Setup(args) => {
            let store = ConfigStore::discover()?;
            let report = tokio::task::spawn_blocking(move || run_setup(&store, &args))
                .await
                .context("setup task failed to join")??;
            global.output.print_success(&report)?;
        }
        Commands::Model(args) => {
            let report = handle_model_command(args)?;
            global.output.print_success(&report)?;
        }
        Commands::Cleanup(args) => {
            let report = run_cleanup(args)?;
            global.output.print_success(&report)?;
        }
        Commands::Config(args) => handle_config(global, args)?,
    }

    Ok(())
}

async fn run_start(args: &RecordArgs) -> Result<RecordingStartedReport> {
    let store = ConfigStore::discover()?;
    let config = apply_record_overrides(store.load_effective()?, args);
    validate_record_prerequisites(&config)?;

    #[cfg(target_os = "macos")]
    {
        let helper_path = ensure_native_helper(&store.paths)?;
        let native_status = native_doctor(&helper_path)?;
        if native_status.screen_capture_access && native_status.displays.is_empty() {
            bail!("no shareable displays are available for native capture");
        }
        if config.input_mode != InputMode::SystemAudio
            && (!native_status.microphone_capture_supported
                || native_status.microphone_permission == "denied"
                || native_status.microphone_permission == "restricted")
        {
            bail!(
                "native microphone capture is unavailable: permission={}, supported={}",
                native_status.microphone_permission,
                native_status.microphone_capture_supported
            );
        }
    }

    store.paths.ensure()?;
    let active_state_path = active_recording_state_path(&store.paths);
    if let Some(existing) = load_active_recording_state(&active_state_path)? {
        if process_is_running(existing.pid) {
            bail!(
                "an active recording is already running: {}; run `scribecli stop` first",
                existing.session_id
            );
        }
        clear_active_recording_state(&active_state_path)?;
    }

    fs::create_dir_all(&config.artifacts_dir).with_context(|| {
        format!(
            "failed to create artifacts directory {}",
            config.artifacts_dir.display()
        )
    })?;

    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| build_session_id(args.session_name.as_deref()));
    let session = SessionPaths::create(&config.artifacts_dir, &session_id)?;
    let started_at = now_rfc3339();
    let runner_stdout_path = session.root_dir.join("runner.stdout.json");
    let runner_stderr_path = session.root_dir.join("runner.stderr.json");
    let stop_request_path = session.root_dir.join("stop-request");
    let mut command =
        build_background_record_command(&config, args, &session_id, &stop_request_path)?;

    let stdout = File::create(&runner_stdout_path)
        .with_context(|| format!("failed to create {}", runner_stdout_path.display()))?;
    let stderr = File::create(&runner_stderr_path)
        .with_context(|| format!("failed to create {}", runner_stderr_path.display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .context("failed to spawn background recording runner")?;

    let state = ActiveRecordingState {
        object: "active_recording".to_string(),
        status: "recording".to_string(),
        session_id: session_id.clone(),
        started_at: started_at.clone(),
        pid: child.id(),
        session_dir: session.root_dir.clone(),
        audio_path: session.raw_audio_path.clone(),
        transcript_path: session.transcript_path.clone(),
        event_log_path: session.event_log_path.clone(),
        runner_stdout_path,
        runner_stderr_path,
        stop_request_path,
    };

    if let Err(error) = save_active_recording_state(&active_state_path, &state) {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    Ok(RecordingStartedReport {
        object: "recording_session",
        status: "recording",
        session_id,
        started_at,
        pid: state.pid,
        session_dir: state.session_dir,
        audio_path: state.audio_path,
        transcript_path: state.transcript_path,
        event_log_path: state.event_log_path,
    })
}

async fn run_stop(args: StopArgs) -> Result<serde_json::Value> {
    let store = ConfigStore::discover()?;
    let active_state_path = active_recording_state_path(&store.paths);
    let state = load_active_recording_state(&active_state_path)?
        .ok_or_else(|| anyhow!("no active recording is running"))?;

    if process_is_running(state.pid) && !state.stop_request_path.exists() {
        fs::write(&state.stop_request_path, "stop\n").with_context(|| {
            format!(
                "failed to write stop request {}",
                state.stop_request_path.display()
            )
        })?;
    }

    let deadline = Instant::now() + Duration::from_secs(args.wait_timeout_seconds.max(1));
    while process_is_running(state.pid) {
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for recording {} to stop and transcribe",
                state.session_id
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    clear_active_recording_state(&active_state_path)?;
    read_background_record_result(&state)
}

fn run_transcribe(config: &EffectiveConfig, args: &TranscribeArgs) -> Result<TranscriptionReport> {
    validate_transcribe_prerequisites(config)?;
    let target = resolve_transcription_target(config, args)?;
    let whisper_config = WhisperConfig {
        cli_path: config
            .whisper_cli_path
            .clone()
            .ok_or_else(|| anyhow!("whisper_cli_path is not configured"))?,
        model_path: config
            .whisper_model_path
            .clone()
            .ok_or_else(|| anyhow!("whisper_model_path is not configured"))?,
    };

    let mut transcript = transcribe_audio(&whisper_config, &target.audio_path)?;
    apply_basic_speaker_labels(&mut transcript);
    write_pretty_json(&target.transcript_path, &transcript)?;

    if let Some(event_log_path) = &target.event_log_path {
        let mut event_logger = EventLogger::create(event_log_path)?;
        event_logger.append(
            "transcription_completed",
            "wrote transcript from existing audio",
            Some(json!({
                "audio_path": target.audio_path,
                "transcript_path": target.transcript_path,
                "segments": transcript.segments.len(),
                "text_length": transcript.text.len(),
            })),
        )?;
    }

    Ok(TranscriptionReport {
        object: "transcription",
        status: "completed",
        audio_path: target.audio_path,
        transcript_path: target.transcript_path,
        session_dir: target.session_dir,
        text: transcript.text,
        segments: transcript.segments,
        warnings: target.warnings,
    })
}

fn handle_config(global: &GlobalOptions, args: ConfigArgs) -> Result<()> {
    let store = ConfigStore::discover()?;

    match args.command {
        ConfigCommand::Get(args) => {
            let effective = store.load_effective()?;
            if let Some(key) = args.key {
                let value = effective.get_value(key);
                global.output.print_success(&ConfigValueReport {
                    object: "config_value",
                    key: key.as_str(),
                    value,
                })?;
            } else {
                global.output.print_success(&ConfigReport {
                    object: "config",
                    config: effective,
                })?;
            }
        }
        ConfigCommand::Set(args) => {
            let mut config = store.load()?;
            config.set_value(args.key, &args.value)?;
            store.save(&config)?;
            global.output.print_success(&ConfigReport {
                object: "config",
                config: store.load_effective()?,
            })?;
        }
        ConfigCommand::Unset(args) => {
            let mut config = store.load()?;
            config.unset_value(args.key);
            store.save(&config)?;
            global.output.print_success(&ConfigReport {
                object: "config",
                config: store.load_effective()?,
            })?;
        }
    }

    Ok(())
}

fn handle_model_command(args: ModelArgs) -> Result<serde_json::Value> {
    match args.command {
        ModelCommand::List => {
            serde_json::to_value(run_model_list()?).context("failed to serialize model list report")
        }
        ModelCommand::Use(args) => serde_json::to_value(run_model_use(args)?)
            .context("failed to serialize model use report"),
        ModelCommand::Remove(args) => serde_json::to_value(run_model_remove(args)?)
            .context("failed to serialize model remove report"),
    }
}

fn run_model_list() -> Result<ModelListReport> {
    let store = ConfigStore::discover()?;
    let effective = store.load_effective()?;
    let managed_model_dir = store.paths.config_dir.join("models");
    let active_model_path = effective.whisper_model_path.clone();
    let models = list_managed_model_paths(&managed_model_dir)?
        .into_iter()
        .map(|path| {
            let file_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            let name = ManagedModel::from_file_name(&path)
                .map(|model| model.cli_name().to_string())
                .unwrap_or_else(|| file_name.clone());
            let size_bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            let active = active_model_path
                .as_ref()
                .is_some_and(|active| active == &path);

            ModelEntry {
                name,
                file_name,
                path,
                size_bytes,
                active,
            }
        })
        .collect::<Vec<_>>();

    Ok(ModelListReport {
        object: "model_list",
        managed_model_dir,
        active_model_path,
        models,
    })
}

fn run_model_use(args: ModelUseArgs) -> Result<ModelUseReport> {
    let store = ConfigStore::discover()?;
    let mut config = store.load()?;
    let effective = store.load_effective()?;
    let managed_model_dir = store.paths.config_dir.join("models");
    fs::create_dir_all(&managed_model_dir)
        .with_context(|| format!("failed to create {}", managed_model_dir.display()))?;

    if let Some(model) = ManagedModel::from_cli_input(&args.target) {
        let installed = ensure_managed_model(
            &managed_model_dir,
            Some(model),
            args.force_download,
            current_model_path(&effective),
        )?;
        config.whisper_model_path = Some(installed.path.clone());
        store.save(&config)?;

        return Ok(ModelUseReport {
            object: "model",
            status: "selected",
            model_path: installed.path,
            managed: true,
            model_action: Some(installed.action),
            config_file: store.paths.config_file.clone(),
        });
    }

    if args.force_download {
        bail!("--force-download only applies to managed model names like `small-en`");
    }

    let target_path = expand_home(PathBuf::from(&args.target));
    if !target_path.is_file() {
        bail!(
            "model path does not exist: {}; pass a managed model name or a model file path",
            target_path.display()
        );
    }
    if !is_whisper_model_file(&target_path) {
        bail!(
            "model path does not look like a Whisper model: {}",
            target_path.display()
        );
    }

    config.whisper_model_path = Some(target_path.clone());
    store.save(&config)?;

    Ok(ModelUseReport {
        object: "model",
        status: "selected",
        model_path: target_path,
        managed: false,
        model_action: None,
        config_file: store.paths.config_file.clone(),
    })
}

fn run_model_remove(args: ModelRemoveArgs) -> Result<ModelRemoveReport> {
    if args.all == args.target.is_some() {
        bail!("pass either a managed model name/file name or `--all`");
    }

    let store = ConfigStore::discover()?;
    let managed_model_dir = store.paths.config_dir.join("models");
    let targets = if args.all {
        list_managed_model_paths(&managed_model_dir)?
    } else {
        vec![resolve_managed_model_path(
            &managed_model_dir,
            args.target.as_deref().expect("target or --all validated"),
        )?]
    };

    if targets.is_empty() {
        bail!(
            "no managed models were found under {}",
            managed_model_dir.display()
        );
    }

    let mut removed_paths = Vec::new();
    for path in targets {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed_paths.push(path);
        }
    }

    let mut config = store.load()?;
    let effective = store.load_effective()?;
    if effective
        .whisper_model_path
        .as_ref()
        .is_some_and(|path| removed_paths.iter().any(|removed| removed == path))
    {
        config.whisper_model_path = select_model_from_dir(&managed_model_dir);
        store.save(&config)?;
    }

    Ok(ModelRemoveReport {
        object: "model_remove",
        status: "completed",
        active_model_path: store.load_effective()?.whisper_model_path,
        removed_paths,
        config_file: store.paths.config_file.clone(),
    })
}

fn run_cleanup(args: CleanupArgs) -> Result<CleanupReport> {
    if !args.all && !args.models && !args.sessions && !args.bin && !args.helper && !args.config {
        bail!("pass at least one cleanup scope like `--models`, `--sessions`, or `--all`");
    }

    let store = ConfigStore::discover()?;
    ensure_no_active_recording(&store.paths)?;

    let mut removed_paths = Vec::new();
    let mut bytes_removed = 0_u64;

    if args.all {
        bytes_removed += remove_path_if_exists(&store.paths.config_dir, &mut removed_paths)?;
        return Ok(CleanupReport {
            object: "cleanup",
            status: "completed",
            config_dir: store.paths.config_dir,
            removed_paths,
            bytes_removed,
        });
    }

    let mut config = store.load()?;
    let models_dir = store.paths.config_dir.join("models");
    let sessions_dir = store.paths.config_dir.join("sessions");
    let bin_dir = store.paths.config_dir.join("bin");
    let helper_dir = store.paths.config_dir.join("native-helper");
    let active_state_path = active_recording_state_path(&store.paths);
    let mut save_config = false;

    if args.models {
        bytes_removed += remove_path_if_exists(&models_dir, &mut removed_paths)?;
        config.whisper_model_path = None;
        save_config = true;
    }
    if args.sessions {
        bytes_removed += remove_path_if_exists(&sessions_dir, &mut removed_paths)?;
    }
    if args.bin {
        bytes_removed += remove_path_if_exists(&bin_dir, &mut removed_paths)?;
        config.whisper_cli_path = None;
        save_config = true;
    }
    if args.helper {
        bytes_removed += remove_path_if_exists(&helper_dir, &mut removed_paths)?;
    }
    if args.config {
        bytes_removed += remove_path_if_exists(&store.paths.config_file, &mut removed_paths)?;
        bytes_removed += remove_path_if_exists(&active_state_path, &mut removed_paths)?;
        save_config = false;
    }

    if save_config {
        store.save(&config)?;
    }

    Ok(CleanupReport {
        object: "cleanup",
        status: "completed",
        config_dir: store.paths.config_dir,
        removed_paths,
        bytes_removed,
    })
}

fn resolve_managed_model_path(managed_model_dir: &Path, raw: &str) -> Result<PathBuf> {
    let requested_path = expand_home(PathBuf::from(raw));
    if requested_path.is_file() {
        if !requested_path.starts_with(managed_model_dir) {
            bail!(
                "only managed models under {} can be removed with `scribecli model remove`",
                managed_model_dir.display()
            );
        }
        return Ok(requested_path);
    }

    let file_name = ManagedModel::from_cli_input(raw)
        .map(|model| model.file_name().to_string())
        .unwrap_or_else(|| raw.to_string());
    let candidate = managed_model_dir.join(file_name);
    if candidate.is_file() {
        return Ok(candidate);
    }

    bail!(
        "managed model `{raw}` was not found under {}",
        managed_model_dir.display()
    )
}

fn ensure_no_active_recording(paths: &crate::config::AppPaths) -> Result<()> {
    let active_state_path = active_recording_state_path(paths);
    if let Some(state) = load_active_recording_state(&active_state_path)?
        && process_is_running(state.pid)
    {
        bail!(
            "an active recording is running: {}; stop it before cleanup",
            state.session_id
        );
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path, removed_paths: &mut Vec<PathBuf>) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let bytes = path_size_bytes(path)?;
    if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    removed_paths.push(path.to_path_buf());
    Ok(bytes)
}

fn path_size_bytes(path: &Path) -> Result<u64> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }

    let mut total = 0_u64;
    for entry in
        fs::read_dir(path).with_context(|| format!("failed to inspect {}", path.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", path.display()))?;
        total += path_size_bytes(&entry.path())?;
    }
    Ok(total)
}

fn init_tracing(verbose: u8) {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("scribecli={default_level}")));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .try_init();
}

fn build_devices_report(config: &EffectiveConfig) -> Result<DevicesReport> {
    let _ = config;
    #[cfg(target_os = "macos")]
    {
        let store = ConfigStore::discover()?;
        let helper_path = ensure_native_helper(&store.paths)?;
        let native = native_list_displays(&helper_path)?;
        Ok(DevicesReport {
            object: "device_list",
            native_screen_capture_access: native.screen_capture_access,
            native_displays: native.displays,
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        bail!("scribecli currently supports macOS only");
    }
}

fn build_doctor_report(config: &EffectiveConfig) -> Result<DoctorReport> {
    let _ = &config;
    #[cfg(not(target_os = "macos"))]
    bail!("scribecli currently supports macOS only");

    #[cfg(target_os = "macos")]
    {
        let mut checks = Vec::new();
        let mut native_displays = Vec::new();

        {
            let store = ConfigStore::discover()?;
            match ensure_native_helper(&store.paths) {
                Ok(helper_path) => {
                    checks.push(DoctorCheck {
                        name: "native_helper",
                        ok: true,
                        message: format!("compiled at {}", helper_path.display()),
                    });

                    match native_doctor(&helper_path) {
                        Ok(native) => {
                            native_displays = native.displays.clone();
                            checks.push(DoctorCheck {
                                name: "screen_capture_access",
                                ok: native.screen_capture_access,
                                message: if native.screen_capture_access {
                                    "screen recording permission is granted".to_string()
                                } else {
                                    "screen recording permission is not granted".to_string()
                                },
                            });
                            checks.push(DoctorCheck {
                                name: "native_displays",
                                ok: !native.displays.is_empty(),
                                message: format!(
                                    "found {} shareable display(s)",
                                    native.displays.len()
                                ),
                            });
                            if config.input_mode != InputMode::SystemAudio {
                                checks.push(DoctorCheck {
                                    name: "microphone_capture",
                                    ok: native.microphone_capture_supported
                                        && native.microphone_permission != "denied"
                                        && native.microphone_permission != "restricted",
                                    message: format!(
                                        "microphone permission: {}; supported: {}",
                                        native.microphone_permission,
                                        native.microphone_capture_supported
                                    ),
                                });
                            }
                        }
                        Err(error) => checks.push(DoctorCheck {
                            name: "native_capture",
                            ok: false,
                            message: error.to_string(),
                        }),
                    }
                }
                Err(error) => checks.push(DoctorCheck {
                    name: "native_helper",
                    ok: false,
                    message: error.to_string(),
                }),
            }

            if config.input_mode == InputMode::MicSystemMix {
                match ffmpeg_version(&config.ffmpeg_path) {
                    Ok(version) => checks.push(DoctorCheck {
                        name: "ffmpeg_binary",
                        ok: true,
                        message: format!("used for post-capture audio mixing: {version}"),
                    }),
                    Err(error) => checks.push(DoctorCheck {
                        name: "ffmpeg_binary",
                        ok: false,
                        message: error.to_string(),
                    }),
                }
            }
        }

        match &config.whisper_cli_path {
        Some(path) => match whisper_version(path) {
            Ok(version) => checks.push(DoctorCheck {
                name: "whisper_cli_binary",
                ok: true,
                message: if version.is_empty() {
                    format!("reachable at {}", path.display())
                } else {
                    version
                },
            }),
            Err(error) => checks.push(DoctorCheck {
                name: "whisper_cli_binary",
                ok: false,
                message: error.to_string(),
            }),
        },
        None => checks.push(DoctorCheck {
            name: "whisper_cli_binary",
            ok: false,
            message: "whisper_cli_path is not configured and no bundled or local whisper-cli install was auto-detected; use a bundled release or install whisper-cpp, then run `scribecli setup`".to_string(),
        }),
    }

        match &config.whisper_model_path {
        Some(path) if path.exists() => checks.push(DoctorCheck {
            name: "whisper_model_path",
            ok: true,
            message: format!("found {}", path.display()),
        }),
        Some(path) => checks.push(DoctorCheck {
            name: "whisper_model_path",
            ok: false,
            message: format!("model file does not exist at {}", path.display()),
        }),
        None => checks.push(DoctorCheck {
            name: "whisper_model_path",
            ok: false,
            message:
                "whisper_model_path is not configured and no local Whisper model was auto-detected; run `scribecli setup` to create a managed model"
                    .to_string(),
        }),
    }

        if let Err(error) = std::fs::create_dir_all(&config.artifacts_dir) {
            checks.push(DoctorCheck {
                name: "artifacts_dir",
                ok: false,
                message: error.to_string(),
            });
        } else {
            checks.push(DoctorCheck {
                name: "artifacts_dir",
                ok: true,
                message: format!("writable at {}", config.artifacts_dir.display()),
            });
        }

        checks.push(DoctorCheck {
        name: "capture_configuration",
        ok: true,
        message: match config.input_mode {
            InputMode::Microphone => {
                "native ScreenCaptureKit capture will record microphone audio".to_string()
            }
            InputMode::SystemAudio => {
                "native ScreenCaptureKit capture will record system audio".to_string()
            }
            InputMode::MicSystemMix => {
                "native ScreenCaptureKit capture will record system audio and microphone separately, then mix them".to_string()
            }
        },
    });

        let ok = checks.iter().all(|check| check.ok);

        Ok(DoctorReport {
            object: "doctor_report",
            ok,
            checks,
            native_displays,
            config: config.clone(),
        })
    } // cfg(target_os = "macos")
}

#[derive(Debug, Clone, Serialize)]
struct RecordingStartedReport {
    object: &'static str,
    status: &'static str,
    session_id: String,
    started_at: String,
    pid: u32,
    session_dir: PathBuf,
    audio_path: PathBuf,
    transcript_path: PathBuf,
    event_log_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveRecordingState {
    object: String,
    status: String,
    session_id: String,
    started_at: String,
    pid: u32,
    session_dir: PathBuf,
    audio_path: PathBuf,
    transcript_path: PathBuf,
    event_log_path: PathBuf,
    runner_stdout_path: PathBuf,
    runner_stderr_path: PathBuf,
    stop_request_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RunnerErrorEnvelope {
    #[serde(rename = "object")]
    _object: String,
    #[serde(rename = "status")]
    _status: u16,
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct TranscriptionReport {
    object: &'static str,
    status: &'static str,
    audio_path: PathBuf,
    transcript_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_dir: Option<PathBuf>,
    text: String,
    segments: Vec<crate::session::TranscriptSegment>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedTranscriptionTarget {
    session_dir: Option<PathBuf>,
    audio_path: PathBuf,
    transcript_path: PathBuf,
    event_log_path: Option<PathBuf>,
    warnings: Vec<String>,
}

fn active_recording_state_path(paths: &crate::config::AppPaths) -> PathBuf {
    paths.config_dir.join("active-recording.json")
}

fn save_active_recording_state(path: &Path, state: &ActiveRecordingState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    write_pretty_json(path, state)
}

fn load_active_recording_state(path: &Path) -> Result<Option<ActiveRecordingState>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

fn clear_active_recording_state(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
}

fn build_background_record_command(
    config: &EffectiveConfig,
    args: &RecordArgs,
    session_id: &str,
    stop_request_path: &Path,
) -> Result<Command> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(current_exe);
    command
        .arg("--output")
        .arg("json")
        .arg("record")
        .arg("--input-mode")
        .arg(input_mode_cli_value(config.input_mode))
        .arg("--partial-interval-seconds")
        .arg(config.partial_interval_seconds.to_string())
        .arg("--artifacts-dir")
        .arg(&config.artifacts_dir)
        .arg("--ffmpeg-path")
        .arg(&config.ffmpeg_path)
        .arg("--session-id")
        .arg(session_id)
        .arg("--stop-request-path")
        .arg(stop_request_path);

    if let Some(display_id) = config.display_id {
        command.arg("--display-id").arg(display_id.to_string());
    }
    if let Some(value) = &config.microphone_device {
        command.arg("--microphone-device").arg(value);
    }
    if let Some(value) = &config.system_device {
        command.arg("--system-device").arg(value);
    }
    if let Some(value) = &config.single_input_device {
        command.arg("--single-input-device").arg(value);
    }
    if let Some(value) = &config.whisper_cli_path {
        command.arg("--whisper-cli-path").arg(value);
    }
    if let Some(value) = &config.whisper_model_path {
        command.arg("--whisper-model-path").arg(value);
    }
    if let Some(duration_seconds) = args.duration_seconds {
        command
            .arg("--duration-seconds")
            .arg(duration_seconds.to_string());
    }

    Ok(command)
}

fn read_background_record_result(state: &ActiveRecordingState) -> Result<serde_json::Value> {
    let stdout = fs::read_to_string(&state.runner_stdout_path).unwrap_or_default();
    if !stdout.trim().is_empty() {
        return serde_json::from_str(stdout.trim()).with_context(|| {
            format!(
                "failed to parse session result from {}",
                state.runner_stdout_path.display()
            )
        });
    }

    let stderr = fs::read_to_string(&state.runner_stderr_path).unwrap_or_default();
    if !stderr.trim().is_empty() {
        if let Ok(error) = serde_json::from_str::<RunnerErrorEnvelope>(stderr.trim()) {
            bail!("{} ({})", error.message, error.code);
        }
        bail!("{}", stderr.trim());
    }

    bail!(
        "recording runner for {} exited without writing a result",
        state.session_id
    );
}

fn resolve_transcription_target(
    config: &EffectiveConfig,
    args: &TranscribeArgs,
) -> Result<ResolvedTranscriptionTarget> {
    let input = expand_home(args.input.clone());
    let candidate = if input.exists() {
        input
    } else {
        let relative = config.artifacts_dir.join(&input);
        if relative.exists() {
            relative
        } else {
            bail!(
                "transcription input was not found at {} or {}",
                input.display(),
                relative.display()
            );
        }
    };

    if candidate.is_file() {
        let transcript_path = args
            .transcript_path
            .clone()
            .unwrap_or_else(|| default_transcript_path_for_audio(&candidate));
        return Ok(ResolvedTranscriptionTarget {
            session_dir: None,
            audio_path: candidate,
            transcript_path,
            event_log_path: None,
            warnings: vec!["speaker labels are heuristic only in v1".to_string()],
        });
    }

    if !candidate.is_dir() {
        bail!("transcription input is neither a file nor a directory");
    }

    let transcript_path = args
        .transcript_path
        .clone()
        .unwrap_or_else(|| candidate.join("transcript.json"));
    let event_log_path = Some(candidate.join("events.jsonl"));
    let session_audio_path = candidate.join("session.wav");
    let trimmed_audio_path = candidate.join("system-trimmed.wav");
    let system_audio_path = candidate.join("system.wav");
    let microphone_audio_path = candidate.join("microphone.wav");
    let mut warnings = vec!["speaker labels are heuristic only in v1".to_string()];

    let audio_path = if session_audio_path.exists() {
        session_audio_path
    } else if trimmed_audio_path.exists() {
        warnings.push(
            "session.wav was missing; used system-trimmed.wav for recovery transcription"
                .to_string(),
        );
        trimmed_audio_path
    } else if system_audio_path.exists() && microphone_audio_path.exists() {
        ffmpeg_version(&config.ffmpeg_path)?;
        mix_audio_files(
            &config.ffmpeg_path,
            &[system_audio_path.as_path(), microphone_audio_path.as_path()],
            &session_audio_path,
        )?;
        warnings.push(
            "session.wav was missing; mixed system.wav and microphone.wav before transcription"
                .to_string(),
        );
        session_audio_path
    } else if system_audio_path.exists() {
        warnings.push("session.wav was missing; used system.wav for transcription".to_string());
        system_audio_path
    } else if microphone_audio_path.exists() {
        warnings.push("session.wav was missing; used microphone.wav for transcription".to_string());
        microphone_audio_path
    } else {
        bail!(
            "no transcribable audio file was found under {}; expected session.wav, system.wav, system-trimmed.wav, or microphone.wav",
            candidate.display()
        );
    };

    Ok(ResolvedTranscriptionTarget {
        session_dir: Some(candidate),
        audio_path,
        transcript_path,
        event_log_path,
        warnings,
    })
}

fn default_transcript_path_for_audio(audio_path: &Path) -> PathBuf {
    let stem = audio_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("transcript");
    audio_path.with_file_name(format!("{stem}.transcript.json"))
}

fn input_mode_cli_value(value: InputMode) -> &'static str {
    match value {
        InputMode::MicSystemMix => "mic-system-mix",
        InputMode::Microphone => "microphone",
        InputMode::SystemAudio => "system-audio",
    }
}

#[cfg(target_os = "macos")]
fn stop_requested_by_file(path: Option<&PathBuf>) -> bool {
    path.is_some_and(|value| value.exists())
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

async fn run_record(config: &EffectiveConfig, args: &RecordArgs) -> Result<SessionResult> {
    #[cfg(target_os = "macos")]
    {
        return run_record_native(config, args).await;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (config, args);
        bail!("scribecli currently supports macOS only");
    }
}

#[cfg(target_os = "macos")]
async fn run_record_native(config: &EffectiveConfig, args: &RecordArgs) -> Result<SessionResult> {
    validate_record_prerequisites(config)?;

    let store = ConfigStore::discover()?;
    let helper_path = ensure_native_helper(&store.paths)?;
    let native_status = native_doctor(&helper_path)?;
    if native_status.screen_capture_access && native_status.displays.is_empty() {
        bail!("no shareable displays are available for native capture");
    }
    if config.input_mode != InputMode::SystemAudio
        && (!native_status.microphone_capture_supported
            || native_status.microphone_permission == "denied"
            || native_status.microphone_permission == "restricted")
    {
        bail!(
            "native microphone capture is unavailable: permission={}, supported={}",
            native_status.microphone_permission,
            native_status.microphone_capture_supported
        );
    }

    std::fs::create_dir_all(&config.artifacts_dir).with_context(|| {
        format!(
            "failed to create artifacts directory {}",
            config.artifacts_dir.display()
        )
    })?;

    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| build_session_id(args.session_name.as_deref()));
    let session = SessionPaths::create(&config.artifacts_dir, &session_id)?;
    let mut event_logger = EventLogger::create(&session.event_log_path)?;
    let mut warnings = vec![
        "speaker labels are heuristic only in v1".to_string(),
        "native macOS capture performs final-pass transcription only; partial chunk transcripts are disabled".to_string(),
    ];
    if !native_status.screen_capture_access {
        warnings.push(
            "native capture will request Screen Recording permission the first time it runs"
                .to_string(),
        );
    }
    let whisper_config = WhisperConfig {
        cli_path: config
            .whisper_cli_path
            .clone()
            .ok_or_else(|| anyhow!("whisper_cli_path is not configured"))?,
        model_path: config
            .whisper_model_path
            .clone()
            .ok_or_else(|| anyhow!("whisper_model_path is not configured"))?,
    };

    event_logger.append(
        "session_started",
        "starting native macOS capture session",
        Some(json!({
            "session_id": session_id,
            "artifacts_dir": session.root_dir,
            "input_mode": config.input_mode,
            "display_id": config.display_id,
        })),
    )?;

    let started_wall = now_rfc3339();
    let started_instant = Instant::now();
    let capture_microphone = config.input_mode != InputMode::SystemAudio;
    let request = NativeRecordRequest {
        system_audio_path: session.system_audio_path.clone(),
        microphone_audio_path: if capture_microphone {
            Some(session.microphone_audio_path.clone())
        } else {
            None
        },
        capture_microphone,
        duration_seconds: args.duration_seconds,
        display_id: config.display_id,
    };
    let mut native_child = spawn_native_record(&helper_path, &request)?;
    let mut stop_requested = false;
    loop {
        if native_child
            .try_wait()
            .context("failed to wait for native capture helper")?
            .is_some()
        {
            break;
        }

        if stop_requested_by_file(args.stop_request_path.as_ref()) && !stop_requested {
            stop_requested = true;
            stop_native_record_process(&mut native_child)?;
        }

        tokio::select! {
            _ = wait_for_stop_signal(), if !stop_requested => {
                stop_requested = true;
                stop_native_record_process(&mut native_child)?;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    let native_record = wait_for_native_record_output(&helper_path, native_child)?;

    event_logger.append(
        "native_capture_completed",
        "native ScreenCaptureKit helper finished",
        Some(json!({
            "system_audio_path": native_record.system_audio_path,
            "microphone_audio_path": native_record.microphone_audio_path,
            "display_id": native_record.display_id,
            "captured_microphone": native_record.captured_microphone,
        })),
    )?;

    if config.input_mode == InputMode::MicSystemMix {
        ffmpeg_version(&config.ffmpeg_path)?;
        let microphone_path = session.microphone_audio_path.as_path();
        if !microphone_path.exists() {
            bail!(
                "native microphone capture was requested but no microphone audio file was produced"
            );
        }
        mix_audio_files(
            &config.ffmpeg_path,
            &[session.system_audio_path.as_path(), microphone_path],
            &session.raw_audio_path,
        )?;
        event_logger.append(
            "audio_mixed",
            "mixed system audio and microphone into session audio",
            Some(json!({
                "audio_path": session.raw_audio_path,
                "system_audio_path": session.system_audio_path,
                "microphone_audio_path": session.microphone_audio_path,
            })),
        )?;
    } else {
        let source_path = match config.input_mode {
            InputMode::Microphone => {
                let path = session.microphone_audio_path.as_path();
                if !path.exists() {
                    bail!("native microphone capture was requested but no microphone audio file was produced");
                }
                path
            }
            _ => session.system_audio_path.as_path(),
        };
        std::fs::copy(source_path, &session.raw_audio_path).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.display(),
                session.raw_audio_path.display()
            )
        })?;
        event_logger.append(
            "audio_copied",
            format!("copied native {} audio into final session audio",
                if config.input_mode == InputMode::Microphone { "microphone" } else { "system" }),
            Some(json!({
                "audio_path": session.raw_audio_path,
                "source_audio_path": source_path,
            })),
        )?;
    }

    let mut transcript = transcribe_audio(&whisper_config, &session.raw_audio_path)?;
    apply_basic_speaker_labels(&mut transcript);
    write_pretty_json(&session.transcript_path, &transcript)?;

    event_logger.append(
        "session_completed",
        "final transcription written",
        Some(json!({
            "transcript_path": session.transcript_path,
            "segments": transcript.segments.len(),
            "text_length": transcript.text.len(),
        })),
    )?;

    Ok(SessionResult {
        object: "session",
        status: "completed",
        session_id,
        started_at: started_wall,
        ended_at: now_rfc3339(),
        duration_seconds: started_instant.elapsed().as_secs_f64(),
        audio_path: session.raw_audio_path,
        transcript_path: session.transcript_path,
        event_log_path: session.event_log_path,
        text: transcript.text,
        segments: transcript.segments,
        warnings,
        partial_chunks_transcribed: 0,
    })
}

fn validate_record_prerequisites(config: &EffectiveConfig) -> Result<()> {
    let _ = &config;
    #[cfg(not(target_os = "macos"))]
    bail!("scribecli currently supports macOS only");

    #[cfg(target_os = "macos")]
    {
        if config.input_mode == InputMode::MicSystemMix {
            ffmpeg_version(&config.ffmpeg_path)?;
        }

        let whisper_cli_path = config.whisper_cli_path.as_ref().ok_or_else(|| {
        anyhow!(
            "whisper_cli_path is not configured; use a bundled release or install whisper-cpp, then run `scribecli setup`"
        )
    })?;
        whisper_version(whisper_cli_path)?;

        let whisper_model_path = config.whisper_model_path.as_ref().ok_or_else(|| {
            anyhow!("whisper_model_path is not configured; run `scribecli setup`")
        })?;
        if !whisper_model_path.exists() {
            bail!(
                "whisper model path does not exist: {}; run `scribecli setup` to create a managed model",
                whisper_model_path.display()
            );
        }

        Ok(())
    } // cfg(target_os = "macos")
}

fn validate_transcribe_prerequisites(config: &EffectiveConfig) -> Result<()> {
    let whisper_cli_path = config.whisper_cli_path.as_ref().ok_or_else(|| {
        anyhow!(
            "whisper_cli_path is not configured; use a bundled release or install whisper-cpp, then run `scribecli setup`"
        )
    })?;
    whisper_version(whisper_cli_path)?;

    let whisper_model_path = config
        .whisper_model_path
        .as_ref()
        .ok_or_else(|| anyhow!("whisper_model_path is not configured; run `scribecli setup`"))?;
    if !whisper_model_path.exists() {
        bail!(
            "whisper model path does not exist: {}; run `scribecli setup` to create a managed model",
            whisper_model_path.display()
        );
    }

    Ok(())
}

fn apply_record_overrides(mut config: EffectiveConfig, args: &RecordArgs) -> EffectiveConfig {
    if let Some(input_mode) = args.input_mode {
        config.input_mode = input_mode;
    }
    if let Some(display_id) = args.display_id {
        config.display_id = Some(display_id);
    }
    if let Some(value) = &args.microphone_device {
        config.microphone_device = Some(value.clone());
    }
    if let Some(value) = &args.system_device {
        config.system_device = Some(value.clone());
    }
    if let Some(value) = &args.single_input_device {
        config.single_input_device = Some(value.clone());
    }
    if let Some(value) = args.partial_interval_seconds {
        config.partial_interval_seconds = value.max(1);
    }
    if let Some(value) = &args.artifacts_dir {
        config.artifacts_dir = value.clone();
    }
    if let Some(value) = &args.ffmpeg_path {
        config.ffmpeg_path = value.clone();
    }
    if let Some(value) = &args.whisper_cli_path {
        config.whisper_cli_path = Some(value.clone());
    }
    if let Some(value) = &args.whisper_model_path {
        config.whisper_model_path = Some(value.clone());
    }
    config
}

fn build_session_id(prefix: Option<&str>) -> String {
    let ts = OffsetDateTime::now_utc()
        .format(&parse("[year][month][day]T[hour][minute][second]Z").expect("format"))
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp().to_string());
    let slug = prefix
        .map(slugify)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "session".to_string());
    format!("{slug}-{ts}-{}", std::process::id())
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in input.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !previous_dash && !out.is_empty() {
                out.push('-');
            }
            previous_dash = true;
        } else {
            out.push(normalized);
            previous_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(target_os = "macos")]
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    fn write_fake_whisper(path: &Path) {
        let script = r#"#!/bin/sh
set -eu
file=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-f" ]; then
    file="$arg"
  fi
  prev="$arg"
done
name=$(basename "$file")
case "$name" in
  chunk-00000.wav)
    echo "[00:00:00.000 --> 00:00:01.000] hello [SPEAKER_TURN]"
    ;;
  chunk-00001.wav)
    echo "[00:00:01.000 --> 00:00:02.000] world"
    ;;
  *)
    echo "[00:00:00.000 --> 00:00:01.000] hello [SPEAKER_TURN]"
    echo "[00:00:01.000 --> 00:00:02.000] world"
    ;;
esac
exit 0
"#;
        fs::write(path, script).unwrap();
        make_executable(path);
    }

    #[test]
    fn transcribe_writes_transcript_for_audio_file() {
        let temp = tempdir().unwrap();
        let whisper_path = temp.path().join("whisper-cli");
        let model_path = temp.path().join("model.bin");
        let audio_path = temp.path().join("meeting.wav");
        write_fake_whisper(&whisper_path);
        fs::write(&model_path, "model").unwrap();
        fs::write(&audio_path, "audio").unwrap();

        let config = EffectiveConfig {
            ffmpeg_path: temp.path().join("ffmpeg"),
            whisper_cli_path: Some(whisper_path),
            whisper_model_path: Some(model_path),
            artifacts_dir: temp.path().join("artifacts"),
            partial_interval_seconds: 15,
            input_mode: InputMode::Microphone,
            display_id: None,
            microphone_device: None,
            system_device: None,
            single_input_device: None,
            config_file: temp.path().join("config.toml"),
        };
        let args = TranscribeArgs {
            input: audio_path.clone(),
            transcript_path: None,
        };

        let report = run_transcribe(&config, &args).unwrap();
        assert_eq!(report.status, "completed");
        assert_eq!(report.audio_path, audio_path);
        assert!(report.transcript_path.exists());
        assert_eq!(report.text, "hello world");
    }

    #[test]
    fn transcribe_resolves_relative_session_id_under_artifacts_dir() {
        let temp = tempdir().unwrap();
        let whisper_path = temp.path().join("whisper-cli");
        let model_path = temp.path().join("model.bin");
        write_fake_whisper(&whisper_path);
        fs::write(&model_path, "model").unwrap();

        let artifacts_dir = temp.path().join("artifacts");
        let session = SessionPaths::create(&artifacts_dir, "session-123").unwrap();
        fs::write(&session.system_audio_path, "audio").unwrap();

        let config = EffectiveConfig {
            ffmpeg_path: temp.path().join("ffmpeg"),
            whisper_cli_path: Some(whisper_path),
            whisper_model_path: Some(model_path),
            artifacts_dir,
            partial_interval_seconds: 15,
            input_mode: InputMode::Microphone,
            display_id: None,
            microphone_device: None,
            system_device: None,
            single_input_device: None,
            config_file: temp.path().join("config.toml"),
        };
        let args = TranscribeArgs {
            input: PathBuf::from("session-123"),
            transcript_path: None,
        };

        let target = resolve_transcription_target(&config, &args).unwrap();
        assert_eq!(target.session_dir, Some(session.root_dir.clone()));
        assert_eq!(target.audio_path, session.system_audio_path);
        assert_eq!(target.transcript_path, session.transcript_path);
        assert!(
            target
                .warnings
                .iter()
                .any(|warning| warning.contains("system.wav"))
        );
    }

    #[test]
    fn config_overrides_apply_to_effective_config() {
        let base = EffectiveConfig {
            ffmpeg_path: PathBuf::from("ffmpeg"),
            whisper_cli_path: None,
            whisper_model_path: None,
            artifacts_dir: PathBuf::from("/tmp/artifacts"),
            partial_interval_seconds: 15,
            input_mode: InputMode::MicSystemMix,
            display_id: None,
            microphone_device: Some("Mic".to_string()),
            system_device: Some("System".to_string()),
            single_input_device: None,
            config_file: PathBuf::from("/tmp/config.toml"),
        };

        let args = RecordArgs {
            input_mode: Some(InputMode::Microphone),
            display_id: Some(123),
            microphone_device: None,
            system_device: None,
            single_input_device: Some("System Capture".to_string()),
            partial_interval_seconds: Some(3),
            artifacts_dir: Some(PathBuf::from("/tmp/custom")),
            ffmpeg_path: Some(PathBuf::from("/usr/local/bin/ffmpeg")),
            whisper_cli_path: None,
            whisper_model_path: None,
            session_name: None,
            duration_seconds: None,
            session_id: None,
            stop_request_path: None,
        };

        let applied = apply_record_overrides(base, &args);
        assert_eq!(applied.input_mode, InputMode::Microphone);
        assert_eq!(applied.display_id, Some(123));
        assert_eq!(applied.partial_interval_seconds, 3);
        assert_eq!(applied.artifacts_dir, PathBuf::from("/tmp/custom"));
        assert_eq!(
            applied.single_input_device,
            Some("System Capture".to_string())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn validate_record_prerequisites_skips_ffmpeg_for_native_microphone() {
        let temp = tempdir().unwrap();
        let whisper_path = temp.path().join("whisper-cli");
        let model_path = temp.path().join("model.bin");
        write_fake_whisper(&whisper_path);
        fs::write(&model_path, "model").unwrap();

        let config = EffectiveConfig {
            ffmpeg_path: temp.path().join("missing-ffmpeg"),
            whisper_cli_path: Some(whisper_path),
            whisper_model_path: Some(model_path),
            artifacts_dir: temp.path().join("artifacts"),
            partial_interval_seconds: 15,
            input_mode: InputMode::Microphone,
            display_id: None,
            microphone_device: None,
            system_device: None,
            single_input_device: None,
            config_file: temp.path().join("config.toml"),
        };

        validate_record_prerequisites(&config).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn validate_record_prerequisites_requires_ffmpeg_for_native_mix() {
        let temp = tempdir().unwrap();
        let whisper_path = temp.path().join("whisper-cli");
        let model_path = temp.path().join("model.bin");
        write_fake_whisper(&whisper_path);
        fs::write(&model_path, "model").unwrap();

        let config = EffectiveConfig {
            ffmpeg_path: temp.path().join("missing-ffmpeg"),
            whisper_cli_path: Some(whisper_path),
            whisper_model_path: Some(model_path),
            artifacts_dir: temp.path().join("artifacts"),
            partial_interval_seconds: 15,
            input_mode: InputMode::MicSystemMix,
            display_id: None,
            microphone_device: None,
            system_device: None,
            single_input_device: None,
            config_file: temp.path().join("config.toml"),
        };

        let error = validate_record_prerequisites(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to execute"));
        assert!(error.contains("missing-ffmpeg"));
    }

    #[test]
    fn resolve_managed_model_path_accepts_alias_and_file_name() {
        let temp = tempdir().unwrap();
        let managed_dir = temp.path().join("models");
        fs::create_dir_all(&managed_dir).unwrap();
        let model_path = managed_dir.join("ggml-small.en.bin");
        fs::write(&model_path, "model").unwrap();

        assert_eq!(
            resolve_managed_model_path(&managed_dir, "small-en").unwrap(),
            model_path
        );
        assert_eq!(
            resolve_managed_model_path(&managed_dir, "ggml-small.en.bin").unwrap(),
            model_path
        );
    }

    #[test]
    fn remove_path_if_exists_reports_removed_size() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("model.bin");
        fs::write(&file_path, "12345").unwrap();
        let mut removed = Vec::new();

        let bytes = remove_path_if_exists(&file_path, &mut removed).unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(removed, vec![file_path]);
    }
}
