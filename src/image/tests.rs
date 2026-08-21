use super::*;
use std::fs;
use std::path::Path;

use super::runtime_contract::*;
use crate::image::runtime_contract::BASH_PATH;
use crate::test_support::test_dir;

const TEST_IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn embedded_layers_define_an_unversioned_runtime_base() {
    assert!(
        BASE_DOCKERFILE
            .lines()
            .any(|line| line.trim() == "FROM ubuntu:latest AS silo_internal_runtime_base")
    );
    assert_eq!(
        EXTRAS_DOCKERFILE.lines().next(),
        Some("FROM silo-base:latest")
    );
    assert!(!BASE_DOCKERFILE.contains("SILO_IMAGE_CONTRACT"));
    assert!(!BASE_DOCKERFILE.contains("dev.silo.image-contract"));
    assert!(!BASE_DOCKERFILE.contains("/usr/local/share/silo/image-contract"));
    let combined = compose_derivative(
        BASE_DOCKERFILE,
        EXTRAS_DOCKERFILE,
        Path::new("embedded silo-extras.dockerfile"),
    )
    .expect("the embedded derivative fits Apple's Dockerfile transport limit");
    assert!(combined.len() < MAX_COMPOSED_DOCKERFILE_BYTES);
}

#[test]
fn image_tags_separate_base_default_and_custom_layers() {
    assert_eq!(BASE_IMAGE_TAG, "silo-base:latest");
    assert_eq!(DEFAULT_IMAGE_TAG, "silo:latest");

    let dir = test_dir("custom-tag");
    let dockerfile = dir.path().join("Dockerfile");
    fs::write(&dockerfile, "FROM silo-base:latest\n").expect("Dockerfile write succeeds");
    let first = custom_image_reference(&dockerfile).expect("custom tag resolves");
    let second = custom_image_reference(&dockerfile).expect("custom tag resolves again");

    assert_eq!(first, second);
    assert!(first.starts_with("silo:custom-"), "{first}");
    assert_eq!(
        first.len(),
        "silo:custom-".len() + CUSTOM_IMAGE_DIGEST_HEX_LEN
    );
}

#[cfg(unix)]
#[test]
fn shared_dockerfile_symlinks_in_different_contexts_have_distinct_tags() {
    use std::os::unix::fs::symlink;

    let dir = test_dir("context-tags");
    let shared = dir.path().join("shared");
    let first_context = dir.path().join("first");
    let second_context = dir.path().join("second");
    fs::create_dir_all(&shared).expect("shared directory creation succeeds");
    fs::create_dir_all(&first_context).expect("first context creation succeeds");
    fs::create_dir_all(&second_context).expect("second context creation succeeds");
    let shared_dockerfile = shared.join("Dockerfile");
    fs::write(&shared_dockerfile, "FROM silo-base:latest\n")
        .expect("shared Dockerfile write succeeds");
    let first_dockerfile = first_context.join("Dockerfile");
    let second_dockerfile = second_context.join("Dockerfile");
    symlink(&shared_dockerfile, &first_dockerfile).expect("first symlink succeeds");
    symlink(&shared_dockerfile, &second_dockerfile).expect("second symlink succeeds");

    assert_ne!(
        custom_image_reference(&first_dockerfile).expect("first tag resolves"),
        custom_image_reference(&second_dockerfile).expect("second tag resolves")
    );
}

#[cfg(unix)]
#[test]
fn shared_dockerfile_aliases_with_distinct_ignore_rules_have_distinct_tags() {
    use std::os::unix::fs::symlink;

    let dir = test_dir("dockerfile-ignore-tags");
    let shared_dockerfile = dir.path().join("Dockerfile.shared");
    fs::write(&shared_dockerfile, "FROM silo-base:latest\n")
        .expect("shared Dockerfile write succeeds");
    let first_dockerfile = dir.path().join("Dockerfile.first");
    let second_dockerfile = dir.path().join("Dockerfile.second");
    symlink(&shared_dockerfile, &first_dockerfile).expect("first symlink succeeds");
    symlink(&shared_dockerfile, &second_dockerfile).expect("second symlink succeeds");
    fs::write(
        dir.path().join("Dockerfile.first.dockerignore"),
        "first-only\n",
    )
    .expect("first ignore file write succeeds");
    fs::write(
        dir.path().join("Dockerfile.second.dockerignore"),
        "second-only\n",
    )
    .expect("second ignore file write succeeds");

    assert_ne!(
        custom_image_reference(&first_dockerfile).expect("first tag resolves"),
        custom_image_reference(&second_dockerfile).expect("second tag resolves")
    );
}

#[test]
fn derivative_composition_uses_an_internal_base_stage() {
    let source = Path::new("Dockerfile");
    let derivative = "# syntax=docker/dockerfile:1\nARG TOOL_VERSION=latest\nFROM silo-base:latest AS app\nRUN echo \"$TOOL_VERSION\"\n";
    let combined =
        compose_derivative(BASE_DOCKERFILE, derivative, source).expect("valid derivative composes");

    assert!(combined.starts_with("# syntax=docker/dockerfile:1\nARG TOOL_VERSION=latest\n"));
    assert!(combined.contains("FROM ubuntu:latest AS silo_internal_runtime_base"));
    assert!(combined.contains("FROM silo_internal_runtime_base AS app"));
    assert!(!combined.contains("FROM silo-base:latest"));
    assert!(combined.ends_with("RUN echo \"$TOOL_VERSION\"\n"));
}

#[test]
fn derivative_validation_requires_one_literal_base_stage() {
    let source = Path::new("Dockerfile");
    for derivative in [
        "FROM scratch\n",
        "ARG BASE=silo-base:latest\nFROM ${BASE}\n",
        "FROM silo-base\n",
        "FROM silo-base:latest\nFROM scratch\n",
        "FROM silo-base:latest AS silo_internal_runtime_base\n",
    ] {
        assert!(
            compose_derivative(BASE_DOCKERFILE, derivative, source).is_err(),
            "unexpectedly accepted {derivative:?}"
        );
    }
}

#[test]
fn derivative_composition_preserves_modern_dockerfile_syntax() {
    let derivative = "\u{feff}# syntax=docker/dockerfile:1\nFROM silo-base:latest\nRUN <<'EOF'\nprintf '%s\\n' silo-base:latest > /tmp/message\nEOF\nCOPY <<EOF /tmp/content\nhello\nEOF\n";
    let combined = compose_derivative(BASE_DOCKERFILE, derivative, Path::new("Dockerfile"))
        .expect("Dockerfile heredocs compose");

    assert!(combined.contains("FROM silo_internal_runtime_base"));
    assert!(!combined.starts_with('\u{feff}'));
    assert!(combined.contains("RUN <<'EOF'\nprintf '%s\\n' silo-base:latest > /tmp/message\nEOF"));
    assert!(combined.contains("COPY <<EOF /tmp/content\nhello\nEOF"));
}

#[test]
fn derivative_validation_allows_unrelated_base_text_and_external_images() {
    for derivative in [
        "FROM silo-base:latest\nRUN echo silo-base:latest\n",
        "FROM silo-base:latest\nRUN --mount=type=cache,target=/tmp/silo-base-cache true\n",
        "FROM silo-base:latest\nCOPY --from=registry.example/tools/silo-base:latest /tool /tool\n",
    ] {
        compose_derivative(BASE_DOCKERFILE, derivative, Path::new("Dockerfile"))
            .expect("unrelated base text remains valid");
    }
}

#[test]
fn derivative_validation_reports_the_offending_line() {
    let error = compose_derivative(
        BASE_DOCKERFILE,
        "# custom extras\nFROM silo-base:edge\n",
        Path::new("images/Dockerfile"),
    )
    .expect_err("noncanonical base is rejected");

    assert!(error.to_string().contains("line 2"), "{error:#}");
}

#[test]
fn derivative_validation_rejects_escape_directives_that_break_the_base() {
    let source = Path::new("Dockerfile");
    compose_derivative(
        BASE_DOCKERFILE,
        "# escape=\\\nFROM silo-base:latest\n",
        source,
    )
    .expect("the default escape character remains compatible");

    let error = compose_derivative(
        BASE_DOCKERFILE,
        "# escape=`\nFROM silo-base:latest\n",
        source,
    )
    .expect_err("a different escape character would reparse the embedded base");

    assert!(error.to_string().contains("line 1"), "{error:#}");
    assert!(error.to_string().contains("escape=\\"), "{error:#}");
}

#[test]
fn composed_dockerfile_size_is_checked_before_building() {
    let dir = test_dir("oversized-composed-dockerfile");
    let dockerfile = dir.path().join("Dockerfile");
    let derivative = format!(
        "FROM silo-base:latest\n# {}\n",
        "x".repeat(MAX_COMPOSED_DOCKERFILE_BYTES)
    );
    fs::write(&dockerfile, derivative).expect("oversized Dockerfile write succeeds");

    let error = validate_dockerfile(&dockerfile)
        .expect_err("the composed Dockerfile must fit Apple's transport limit");

    assert!(
        error
            .to_string()
            .contains("after adding Silo's runtime base")
    );
    assert!(
        error
            .to_string()
            .contains(&MAX_COMPOSED_DOCKERFILE_BYTES.to_string())
    );
}

#[test]
fn image_digest_parser_ignores_labels_and_requires_a_digest() {
    let without_labels =
        format!(r#"[{{"configuration":{{"descriptor":{{"digest":"{TEST_IMAGE_DIGEST}"}}}}}}]"#);
    assert_eq!(
        parse_image_digest(without_labels.as_bytes()).expect("unlabelled image parses"),
        TEST_IMAGE_DIGEST
    );

    let with_unrelated_label = format!(
        r#"[{{"configuration":{{"descriptor":{{"digest":"{TEST_IMAGE_DIGEST}"}},"config":{{"Labels":{{"example":"value"}}}}}}}}]"#
    );
    assert_eq!(
        parse_image_digest(with_unrelated_label.as_bytes()).expect("labelled image parses"),
        TEST_IMAGE_DIGEST
    );

    assert!(parse_image_digest(br#"[{"configuration":{}}]"#).is_err());
    assert!(parse_image_digest(b"not json").is_err());
}

#[test]
fn inspect_errors_distinguish_missing_images_from_probe_failures() {
    let missing = inspect_error(DEFAULT_IMAGE_TAG, "Error: image not found: silo:latest");
    assert!(missing.to_string().contains("not built yet"));
    let failed = inspect_error(DEFAULT_IMAGE_TAG, "container runtime is unavailable");
    assert!(failed.to_string().contains("could not check"));
    let unrelated = inspect_error(
        DEFAULT_IMAGE_TAG,
        "Error: base image not found while checking silo:latest",
    );
    assert!(unrelated.to_string().contains("could not check"));
}

#[test]
fn embedded_runtime_contains_lifecycle_programs() {
    for asset in RUNTIME_ASSETS {
        assert!(
            BASE_DOCKERFILE.contains(&format!("ARG {}", asset.build_arg)),
            "missing {}",
            asset.build_arg
        );
        assert!(BASE_DOCKERFILE.contains(asset.image_path));
    }
    let directory_install = BASE_DOCKERFILE
        .find("install -d -o root -g root -m 0755 /etc/ssh /usr/local/bin")
        .expect("runtime asset parent directories are created");
    let ssh_config_write = BASE_DOCKERFILE
        .find("> /etc/ssh/silo_sshd_config")
        .expect("SSH configuration is decoded into place");
    assert!(directory_install < ssh_config_write);
    assert!(!BASE_DOCKERFILE.contains("COPY silo-"));
    assert!(ENTRYPOINT.contains("-exec mountpoint -q {} \\; -prune"));
    assert!(ENTRYPOINT.contains("-exec chown -h silo:silo {} +"));
    assert!(ENTRYPOINT.contains("unset SILO_INTERNAL_HOST_PORTS"));
    assert!(LIFECYCLE.contains("flock --exclusive --nonblock"));
    assert!(LIFECYCLE.contains("flock --shared"));
    assert!(LIFECYCLE.contains("count=$((count + 1))"));
}

#[test]
fn embedded_layers_keep_supported_shells_and_default_tools() {
    for package in ["fish", "nushell", "zsh"] {
        assert!(
            BASE_DOCKERFILE
                .lines()
                .any(|line| line.trim().trim_end_matches(" \\").trim() == package),
            "missing base package {package}"
        );
    }
    assert!(BASE_DOCKERFILE.contains(BASH_PATH));
    for name in ["zsh", "fish", "nu"] {
        assert!(
            BASE_DOCKERFILE.contains(&format!("${{BREW_PREFIX}}/bin/{name}")),
            "missing supported shell {name}"
        );
    }
    for package in [
        "actionlint",
        "claude-code",
        "codex",
        "jj",
        "playwright-cli",
        "rust",
        "shellcheck",
        "uv",
    ] {
        assert!(
            EXTRAS_DOCKERFILE
                .lines()
                .any(|line| line.trim().trim_end_matches(" \\").trim() == package),
            "missing extras package {package}"
        );
    }
    assert!(EXTRAS_DOCKERFILE.contains("playwright-cli install-browser --with-deps"));
    assert!(BASE_DOCKERFILE.contains("brew cleanup --prune=all"));
    assert!(EXTRAS_DOCKERFILE.contains("brew cleanup --prune=all"));
}

#[test]
fn base_account_password_is_unusable_without_locking_the_account() {
    assert!(BASE_DOCKERFILE.contains("usermod --password '*' silo"));
    assert!(!BASE_DOCKERFILE.contains("passwd --lock silo"));
    assert!(!BASE_DOCKERFILE.contains("passwd --delete silo"));
}
