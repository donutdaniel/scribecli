use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::AppPaths;

const HELPER_SOURCE: &str = include_str!("../native/macos/ScribeCapture.swift");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeDisplay {
    #[serde(rename = "displayID")]
    pub display_id: u32,
    pub width: i64,
    pub height: i64,
    #[serde(rename = "isMain")]
    pub is_main: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeStatus {
    pub object: String,
    #[serde(rename = "screenCaptureAccess")]
    pub screen_capture_access: bool,
    #[serde(rename = "microphonePermission")]
    pub microphone_permission: String,
    #[serde(rename = "microphoneCaptureSupported")]
    pub microphone_capture_supported: bool,
    pub displays: Vec<NativeDisplay>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeRecordResult {
    pub object: String,
    pub status: String,
    #[serde(rename = "systemAudioPath")]
    pub system_audio_path: PathBuf,
    #[serde(rename = "microphoneAudioPath")]
    pub microphone_audio_path: Option<PathBuf>,
    #[serde(rename = "capturedMicrophone")]
    pub captured_microphone: bool,
    #[serde(rename = "displayID")]
    pub display_id: u32,
}

#[derive(Debug, Clone)]
pub struct NativeRecordRequest {
    pub system_audio_path: PathBuf,
    pub microphone_audio_path: Option<PathBuf>,
    pub capture_microphone: bool,
    pub duration_seconds: Option<u64>,
    pub display_id: Option<u32>,
}

pub fn ensure_helper(paths: &AppPaths) -> Result<PathBuf> {
    let helper_dir = paths.config_dir.join("native-helper");
    fs::create_dir_all(&helper_dir)
        .with_context(|| format!("failed to create {}", helper_dir.display()))?;

    let source_path = helper_dir.join("ScribeCapture.swift");
    let binary_path = helper_dir.join("scribecapture");

    let needs_write = match fs::read_to_string(&source_path) {
        Ok(existing) => existing != HELPER_SOURCE,
        Err(_) => true,
    };
    if needs_write {
        fs::write(&source_path, HELPER_SOURCE)
            .with_context(|| format!("failed to write {}", source_path.display()))?;
    }

    let needs_compile = if !binary_path.exists() {
        true
    } else {
        let source_meta = fs::metadata(&source_path)
            .with_context(|| format!("failed to read {}", source_path.display()))?;
        let binary_meta = fs::metadata(&binary_path)
            .with_context(|| format!("failed to read {}", binary_path.display()))?;
        source_meta.modified().ok() > binary_meta.modified().ok()
    };

    if needs_compile {
        compile_helper(&source_path, &binary_path)?;
    }

    Ok(binary_path)
}

pub fn doctor(helper_path: &Path) -> Result<NativeStatus> {
    let output = Command::new(helper_path)
        .arg("doctor")
        .output()
        .with_context(|| format!("failed to execute {}", helper_path.display()))?;
    parse_json_output(helper_path, output)
}

pub fn list_displays(helper_path: &Path) -> Result<NativeStatus> {
    let output = Command::new(helper_path)
        .arg("list-displays")
        .output()
        .with_context(|| format!("failed to execute {}", helper_path.display()))?;
    parse_json_output(helper_path, output)
}

#[cfg(test)]
pub fn record(helper_path: &Path, request: &NativeRecordRequest) -> Result<NativeRecordResult> {
    let output = build_record_command(helper_path, request)
        .output()
        .with_context(|| format!("failed to execute {}", helper_path.display()))?;
    parse_json_output(helper_path, output)
}

pub fn spawn_record(helper_path: &Path, request: &NativeRecordRequest) -> Result<Child> {
    build_record_command(helper_path, request)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to execute {}", helper_path.display()))
}

pub fn wait_for_record_output(helper_path: &Path, child: Child) -> Result<NativeRecordResult> {
    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {}", helper_path.display()))?;
    parse_json_output(helper_path, output)
}

pub fn stop_record_process(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to poll native capture helper")?
        .is_some()
    {
        return Ok(());
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }

    #[cfg(not(unix))]
    child
        .kill()
        .context("failed to stop native capture helper")?;

    Ok(())
}

fn build_record_command(helper_path: &Path, request: &NativeRecordRequest) -> Command {
    let mut command = Command::new(helper_path);
    command
        .arg("record")
        .arg("--system-audio-path")
        .arg(&request.system_audio_path)
        .arg("--capture-microphone")
        .arg(if request.capture_microphone {
            "true"
        } else {
            "false"
        });

    if let Some(path) = &request.microphone_audio_path {
        command.arg("--microphone-audio-path").arg(path);
    }
    if let Some(duration) = request.duration_seconds {
        command.arg("--duration-seconds").arg(duration.to_string());
    }
    if let Some(display_id) = request.display_id {
        command.arg("--display-id").arg(display_id.to_string());
    }

    command
}

fn compile_helper(source_path: &Path, binary_path: &Path) -> Result<()> {
    let output = Command::new("xcrun")
        .args([
            "swiftc",
            "-framework",
            "ScreenCaptureKit",
            "-framework",
            "AVFoundation",
            "-framework",
            "CoreMedia",
            "-framework",
            "CoreGraphics",
        ])
        .arg(source_path)
        .arg("-o")
        .arg(binary_path)
        .output()
        .context("failed to run xcrun swiftc")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "failed to compile native ScreenCaptureKit helper:\n{}\n{}",
            stdout.trim(),
            stderr.trim()
        );
    }

    Ok(())
}

fn parse_json_output<T>(helper_path: &Path, output: std::process::Output) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "{} exited with status {}: {} {}",
            helper_path.display(),
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).context("helper output was not valid UTF-8")?;
    serde_json::from_str(&stdout).with_context(|| {
        format!(
            "failed to parse helper JSON output: {}",
            stdout.lines().take(20).collect::<Vec<_>>().join("\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

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

    fn write_helper_script(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        make_executable(path);
    }

    #[test]
    fn doctor_and_list_displays_parse_helper_json() {
        let temp = tempdir().unwrap();
        let helper = temp.path().join("scribecapture");
        write_helper_script(
            &helper,
            r#"#!/bin/sh
set -eu
case "$1" in
  doctor|list-displays)
    printf '%s\n' '{"object":"native_macos_status","screenCaptureAccess":true,"microphonePermission":"authorized","microphoneCaptureSupported":true,"displays":[{"displayID":7,"width":1728,"height":1117,"isMain":true}]}'
    ;;
  *)
    exit 1
    ;;
esac
"#,
        );

        let doctor_status = doctor(&helper).unwrap();
        assert!(doctor_status.screen_capture_access);
        assert_eq!(doctor_status.microphone_permission, "authorized");
        assert_eq!(doctor_status.displays.len(), 1);
        assert_eq!(doctor_status.displays[0].display_id, 7);

        let list_status = list_displays(&helper).unwrap();
        assert_eq!(list_status.displays[0].width, 1728);
        assert!(list_status.displays[0].is_main);
    }

    #[test]
    fn record_passes_expected_arguments_to_helper() {
        let temp = tempdir().unwrap();
        let helper = temp.path().join("scribecapture");
        let arg_log = temp.path().join("args.txt");
        let script = format!(
            r#"#!/bin/sh
set -eu
cmd="$1"
shift
if [ "$cmd" != "record" ]; then
  echo "unexpected command: $cmd" >&2
  exit 1
fi
printf '%s\n' "$@" > "{}"
printf '%s\n' '{{"object":"native_record_result","status":"completed","systemAudioPath":"/tmp/system.wav","microphoneAudioPath":"/tmp/microphone.wav","capturedMicrophone":true,"displayID":99}}'
"#,
            arg_log.display()
        );
        write_helper_script(&helper, &script);

        let result = record(
            &helper,
            &NativeRecordRequest {
                system_audio_path: PathBuf::from("/tmp/system.wav"),
                microphone_audio_path: Some(PathBuf::from("/tmp/microphone.wav")),
                capture_microphone: true,
                duration_seconds: Some(8),
                display_id: Some(42),
            },
        )
        .unwrap();

        let args = fs::read_to_string(&arg_log).unwrap();
        assert!(args.contains("--system-audio-path"));
        assert!(args.contains("/tmp/system.wav"));
        assert!(args.contains("--microphone-audio-path"));
        assert!(args.contains("/tmp/microphone.wav"));
        assert!(args.contains("--capture-microphone"));
        assert!(args.contains("true"));
        assert!(args.contains("--duration-seconds"));
        assert!(args.contains("8"));
        assert!(args.contains("--display-id"));
        assert!(args.contains("42"));
        assert_eq!(result.display_id, 99);
        assert!(result.captured_microphone);
    }

    #[test]
    fn doctor_surfaces_helper_failure_output() {
        let temp = tempdir().unwrap();
        let helper = temp.path().join("scribecapture");
        write_helper_script(
            &helper,
            r#"#!/bin/sh
echo "screen recording permission was not granted" >&2
exit 1
"#,
        );

        let error = doctor(&helper).unwrap_err().to_string();
        assert!(error.contains("screen recording permission was not granted"));
    }

    #[test]
    fn record_rejects_invalid_helper_json() {
        let temp = tempdir().unwrap();
        let helper = temp.path().join("scribecapture");
        write_helper_script(
            &helper,
            r#"#!/bin/sh
printf '%s\n' 'not json'
"#,
        );

        let error = record(
            &helper,
            &NativeRecordRequest {
                system_audio_path: PathBuf::from("/tmp/system.wav"),
                microphone_audio_path: None,
                capture_microphone: false,
                duration_seconds: None,
                display_id: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("failed to parse helper JSON output"));
    }

    #[cfg(unix)]
    #[test]
    fn spawned_record_helper_stops_via_sigterm_and_still_returns_json() {
        let temp = tempdir().unwrap();
        let helper = temp.path().join("scribecapture");
        let ready = temp.path().join("ready");
        let script = format!(
            r#"#!/bin/sh
set -eu
cmd="$1"
shift
if [ "$cmd" != "record" ]; then
  exit 1
fi
trap '' INT
trap 'printf "%s\n" '\''{{"object":"native_record_result","status":"completed","systemAudioPath":"/tmp/system.wav","microphoneAudioPath":null,"capturedMicrophone":false,"displayID":5}}'\''; exit 0' TERM
: > "{}"
while :; do
  sleep 1
done
"#,
            ready.display()
        );
        write_helper_script(&helper, &script);

        let mut child = spawn_record(
            &helper,
            &NativeRecordRequest {
                system_audio_path: PathBuf::from("/tmp/system.wav"),
                microphone_audio_path: None,
                capture_microphone: false,
                duration_seconds: None,
                display_id: None,
            },
        )
        .unwrap();

        for _ in 0..20 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready.exists());
        stop_record_process(&mut child).unwrap();
        let result = wait_for_record_output(&helper, child).unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.display_id, 5);
    }
}
