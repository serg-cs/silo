//! Reliable user-facing command output.

use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;

/// Writes exact text while treating a closed downstream pipe as success.
pub(crate) fn write_stdout(text: &str) -> Result<()> {
    write_output(&mut io::stdout().lock(), text)
}

/// Serializes one value as pretty JSON with a trailing newline.
pub(crate) fn write_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    write_stdout(&json_text(value)?)
}

fn json_text<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let mut text =
        serde_json::to_string_pretty(value).context("failed to serialize JSON output")?;
    text.push('\n');
    Ok(text)
}

/// Escapes one value without changing the one-record-per-line TSV shape.
pub(crate) fn tsv_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn write_output(writer: &mut impl Write, text: &str) -> Result<()> {
    match writer.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err).context("failed to write command output"),
    }
}

#[cfg(test)]
mod tests;
