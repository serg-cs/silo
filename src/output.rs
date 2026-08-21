//! Reliable user-facing command output.

use std::io::{self, Write};

use anyhow::{Context, Result};

/// Writes exact text while treating a closed downstream pipe as success.
pub(crate) fn write_stdout(text: &str) -> Result<()> {
    write_output(&mut io::stdout().lock(), text)
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
