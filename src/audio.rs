use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn ffmpeg_version(ffmpeg_path: &Path) -> Result<String> {
    let output = Command::new(ffmpeg_path)
        .arg("-version")
        .output()
        .with_context(|| format!("failed to execute {}", ffmpeg_path.display()))?;

    if !output.status.success() {
        bail!(
            "{} exited with status {}",
            ffmpeg_path.display(),
            output.status
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().next().unwrap_or("").to_string())
}

pub fn mix_audio_files(
    ffmpeg_path: &Path,
    input_paths: &[&Path],
    output_path: &Path,
) -> Result<()> {
    if input_paths.is_empty() {
        bail!("no audio inputs were provided for mixing");
    }

    if input_paths.len() == 1 {
        fs::copy(input_paths[0], output_path).with_context(|| {
            format!(
                "failed to copy {} to {}",
                input_paths[0].display(),
                output_path.display()
            )
        })?;
        return Ok(());
    }

    let mut command = Command::new(ffmpeg_path);
    command.args(["-hide_banner", "-loglevel", "error", "-y"]);
    for input_path in input_paths {
        command.arg("-i").arg(input_path);
    }

    let filter = format!(
        "{}amix=inputs={}:duration=longest:dropout_transition=0[aout]",
        (0..input_paths.len())
            .map(|index| format!("[{index}:a]"))
            .collect::<String>(),
        input_paths.len()
    );
    let output = command
        .args(["-filter_complex", &filter, "-map", "[aout]"])
        .arg(output_path)
        .output()
        .with_context(|| format!("failed to execute {}", ffmpeg_path.display()))?;

    if !output.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!("failed to mix audio files: {}", combined.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {}
