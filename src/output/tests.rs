use std::io::{self, Write};

use super::*;

struct FailingWriter(io::ErrorKind);

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(self.0))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn output_is_written_exactly() {
    let mut output = Vec::new();

    write_output(&mut output, "result\n").expect("output writes");

    assert_eq!(output, b"result\n");
}

#[test]
fn broken_pipes_are_successful_early_termination() {
    write_output(&mut FailingWriter(io::ErrorKind::BrokenPipe), "result\n")
        .expect("closed consumer is not a command failure");
}

#[test]
fn other_output_failures_are_reported() {
    let error = write_output(
        &mut FailingWriter(io::ErrorKind::PermissionDenied),
        "result\n",
    )
    .expect_err("real output failure propagates")
    .to_string();

    assert!(error.contains("failed to write command output"), "{error}");
}

#[test]
fn tsv_fields_preserve_one_record_per_line() {
    assert_eq!(
        tsv_field("one\\two\tthree\nfour\rfive"),
        "one\\\\two\\tthree\\nfour\\rfive"
    );
}

#[test]
fn json_is_pretty_printed_with_a_trailing_newline() {
    let text = json_text(&serde_json::json!({"items": [1, 2]})).expect("JSON serializes");

    assert_eq!(text, "{\n  \"items\": [\n    1,\n    2\n  ]\n}\n");
}
