use std::cell::Cell;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use super::*;

const TEST_IMAGE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NOT_STARTED_HINT: &str =
    "Ensure container system service has been started with `container system start`.";

fn inspection_json(id: &str, state: &str, mounts: &str) -> String {
    format!(
        r#"[{{
            "id":"{id}",
            "configuration":{{
                "id":"{id}",
                "labels":{{"dev.silo.owner":"silo"}},
                "image":{{"descriptor":{{"digest":"{TEST_IMAGE_DIGEST}"}}}},
                "mounts":{mounts}
            }},
            "status":{{
                "state":"{state}",
                "networks":[{{"ipv4Address":"192.168.64.2/24"}}]
            }}
        }}]"#
    )
}

#[test]
fn inspection_rejects_unknown_runtime_states() {
    let json = inspection_json("silo-test", "paused", "[]");
    let error = parse_container_inspection(json.as_bytes(), "silo-test")
        .expect_err("unknown states cannot be managed")
        .to_string();
    assert!(error.contains("unsupported state `paused`"), "{error}");
}

#[test]
fn inspection_requires_complete_current_mount_inventory() {
    let valid = inspection_json(
        "silo-test",
        "running",
        r#"[{"source":"/state/cache","destination":"/cache"}]"#,
    );
    let parsed = parse_container_inspection(valid.as_bytes(), "silo-test")
        .expect("current inspect shape parses");
    assert_eq!(parsed.mount_sources, [PathBuf::from("/state/cache")]);
    assert_eq!(parsed.ipv4_address, Some(Ipv4Addr::new(192, 168, 64, 2)));

    let missing_mounts = inspection_json("silo-test", "running", "null");
    assert!(parse_container_inspection(missing_mounts.as_bytes(), "silo-test").is_err());

    let missing_source = inspection_json("silo-test", "running", r#"[{"destination":"/cache"}]"#);
    assert!(parse_container_inspection(missing_source.as_bytes(), "silo-test").is_err());

    let empty_source = inspection_json("silo-test", "running", r#"[{"source":""}]"#);
    let error = parse_container_inspection(empty_source.as_bytes(), "silo-test")
        .expect_err("empty mount sources fail closed")
        .to_string();
    assert!(error.contains("without a source"), "{error}");
}

#[test]
fn runtime_json_helpers_require_exact_machine_readable_shapes() {
    assert_eq!(
        parse_container_ids(br#"[{"id":"one"},{"id":"two"}]"#).expect("container IDs parse"),
        ["one", "two"]
    );
    assert!(parse_container_ids(br#"[{"name":"one"}]"#).is_err());
    assert!(container_not_found(
        "Error: container not found: silo-test",
        "silo-test"
    ));
    assert!(!container_not_found(
        "Error: image not found while inspecting silo-test",
        "silo-test"
    ));
}

#[test]
fn system_start_classification_is_narrow() {
    assert_eq!(
        start_system_for_error(NOT_STARTED_HINT, || true),
        SystemStart::Started
    );
    assert_eq!(
        start_system_for_error(NOT_STARTED_HINT, || false),
        SystemStart::Failed
    );
    assert_eq!(
        start_system_for_error("Error: image not found", || {
            panic!("ordinary errors must not start the runtime")
        }),
        SystemStart::NotNeeded
    );
}

#[test]
fn stopped_runtime_probes_start_and_retry_once() {
    let probes = Cell::new(0);
    let starts = Cell::new(0);
    let (value, _) = probe_with_system_start(
        || {
            probes.set(probes.get() + 1);
            if probes.get() == 1 {
                Ok((None, NOT_STARTED_HINT.to_string()))
            } else {
                Ok((Some("ready"), String::new()))
            }
        },
        || {
            starts.set(starts.get() + 1);
            true
        },
    )
    .expect("probe succeeds after starting the runtime");
    assert_eq!(value, Some("ready"));
    assert_eq!(probes.get(), 2);
    assert_eq!(starts.get(), 1);

    let (missing, stderr) = probe_with_system_start(
        || Ok::<_, anyhow::Error>((None::<()>, "Error: image not found".to_string())),
        || panic!("ordinary misses must not start the runtime"),
    )
    .expect("ordinary miss is preserved");
    assert!(missing.is_none());
    assert_eq!(stderr, "Error: image not found");
}
