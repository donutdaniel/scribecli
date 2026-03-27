use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Yaml,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    object: &'static str,
    status: u16,
    code: &'a str,
    message: &'a str,
}

impl OutputFormat {
    pub fn print_success<T: Serialize + ?Sized>(&self, value: &T) -> Result<()> {
        let rendered = self.render(value)?;
        println!("{rendered}");
        Ok(())
    }

    pub fn print_error(&self, status: u16, code: &str, message: &str) -> Result<()> {
        if matches!(self, Self::Human) {
            if message.contains('\n') {
                eprintln!("{code} ({status})");
                eprintln!("{message}");
            } else {
                eprintln!("{code} ({status}): {message}");
            }
            return Ok(());
        }

        let envelope = ErrorEnvelope {
            object: "error",
            status,
            code,
            message,
        };
        let rendered = self.render(&envelope)?;
        eprintln!("{rendered}");
        Ok(())
    }

    fn render<T: Serialize + ?Sized>(&self, value: &T) -> Result<String> {
        match self {
            Self::Human => {
                serde_json::to_string_pretty(value).context("failed to render human-readable JSON")
            }
            Self::Json => serde_json::to_string(value).context("failed to render JSON output"),
            Self::Yaml => yaml_serde::to_string(value).context("failed to render YAML output"),
        }
    }
}
