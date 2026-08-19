use std::path::Path;

use anyhow::{Context, Result, anyhow};
use parse_dockerfile::Dockerfile;

use super::{BASE_IMAGE_TAG, MAX_COMPOSED_DOCKERFILE_BYTES};

pub(super) const INTERNAL_BASE_STAGE: &str = "silo_internal_runtime_base";

/// Validates and joins Silo's base with a single trusted extras stage.
pub(in crate::container) fn compose_derivative(
    base: &str,
    derivative: &str,
    source: &Path,
) -> Result<String> {
    // Parser spans are relative to content after an optional UTF-8 BOM.
    let derivative = derivative.strip_prefix('\u{feff}').unwrap_or(derivative);
    let parsed = parse_derivative(derivative, source)?;
    validate_parser_directives(&parsed, derivative, source)?;
    let stage = single_extras_stage(&parsed, derivative, source)?;

    // Preserve directives and global ARGs while inserting the runtime stage.
    let header_end = stage.from.from.span.start;
    let image = &stage.from.image.span;
    let mut combined = String::new();
    combined.push_str(&derivative[..header_end]);
    ensure_trailing_newline(&mut combined);
    combined.push_str(base);
    ensure_trailing_newline(&mut combined);
    combined.push_str(&derivative[header_end..image.start]);
    combined.push_str(INTERNAL_BASE_STAGE);
    combined.push_str(&derivative[image.end..]);
    ensure_trailing_newline(&mut combined);
    validate_composed_size(&combined, source)?;
    Ok(combined)
}

/// Keeps derivative parser settings compatible with the embedded base text.
fn validate_parser_directives(parsed: &Dockerfile, content: &str, source: &Path) -> Result<()> {
    if let Some(directive) = &parsed.parser_directives.escape
        && directive.value.value != '\\'
    {
        return Err(line_error(
            source,
            content,
            directive.span().start,
            "uses an unsupported escape parser directive; Silo extras require `# escape=\\`",
        ));
    }
    Ok(())
}

/// Selects the deliberately narrow custom-image extension point.
fn single_extras_stage<'a, 'b>(
    parsed: &'b Dockerfile<'a>,
    content: &str,
    source: &Path,
) -> Result<parse_dockerfile::Stage<'a, 'b>> {
    let mut stages = parsed.stages();
    if stages.len() != 1 {
        return Err(anyhow!(
            "image dockerfile `{}` must contain exactly one FROM instruction",
            source.display()
        ));
    }
    let stage = stages.next().ok_or_else(|| {
        anyhow!(
            "image dockerfile `{}` has no FROM instruction",
            source.display()
        )
    })?;
    if stage.from.image.value != BASE_IMAGE_TAG {
        return Err(line_error(
            source,
            content,
            stage.from.image.span.start,
            "must start its only stage with the literal `FROM silo-base:latest`",
        ));
    }
    if let Some((_, alias)) = &stage.from.as_
        && alias.value.eq_ignore_ascii_case(INTERNAL_BASE_STAGE)
    {
        return Err(line_error(
            source,
            content,
            alias.span.start,
            "uses Silo's reserved internal base stage name",
        ));
    }
    Ok(stage)
}

/// Enforces Apple's current Dockerfile transport limit before runtime cleanup.
fn validate_composed_size(content: &str, source: &Path) -> Result<()> {
    if content.len() < MAX_COMPOSED_DOCKERFILE_BYTES {
        return Ok(());
    }
    Err(anyhow!(
        "image dockerfile `{}` becomes {} bytes after adding Silo's runtime base, but Apple's container builder currently requires fewer than {MAX_COMPOSED_DOCKERFILE_BYTES} bytes",
        source.display(),
        content.len(),
    ))
}

fn parse_derivative<'a>(content: &'a str, source: &Path) -> Result<Dockerfile<'a>> {
    parse_dockerfile::parse(content)
        .with_context(|| format!("could not parse image dockerfile `{}`", source.display()))
}

fn line_error(source: &Path, content: &str, offset: usize, message: &str) -> anyhow::Error {
    let line = content[..offset].matches('\n').count() + 1;
    anyhow!(
        "image dockerfile `{}` line {line} {message}",
        source.display()
    )
}

fn ensure_trailing_newline(content: &mut String) {
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
}
