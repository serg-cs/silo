use super::*;

const INSTANCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn container(id: &str, project: &str) -> ContainerInfo {
    ContainerInfo {
        id: id.to_string(),
        lifecycle: ContainerLifecycle::Shared,
        state: ContainerState::Stopped,
        project: project.into(),
        instance: INSTANCE.to_string(),
    }
}

#[test]
fn selectors_prefer_exact_ids_and_require_unique_project_names() {
    let containers = [
        container("silo-one", "/work/one"),
        container("silo-two", "/other/one"),
    ];

    assert_eq!(
        select_container(&containers, "silo-two").unwrap().id,
        "silo-two"
    );
    assert!(
        select_container(&containers, "one")
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
}

#[test]
fn list_output_is_compact_and_preserves_full_identity() {
    let containers = [container("silo-full-id", "/work/project")];
    assert_eq!(
        render_container_list(&containers),
        "ID\tTYPE\tSTATE\tPROJECT\nsilo-full-id\tshared\tstopped\t/work/project"
    );

    let unusual = [container("silo-escaped", "/work/project\tbranch\nnext")];
    assert_eq!(
        render_container_list(&unusual),
        "ID\tTYPE\tSTATE\tPROJECT\nsilo-escaped\tshared\tstopped\t/work/project\\tbranch\\nnext"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stop_guard_readiness_has_a_bounded_wait() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("sh")
        .args(["-c", "sleep 1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("delayed readiness process starts");
    let stdout = child.stdout.take().expect("stdout is piped");
    let started = Instant::now();
    let error = read_guard_readiness(stdout, Duration::from_millis(20))
        .expect_err("readiness wait times out")
        .to_string();
    let _ = child.kill();
    let _ = child.wait();

    assert!(error.contains("did not become ready"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(500));
}
