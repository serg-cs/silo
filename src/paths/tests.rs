use std::path::{Path, PathBuf};

use super::*;

#[test]
fn join_project_skips_absolute_and_home_paths() {
    let root = Path::new("/project");
    assert_eq!(
        join_project(Path::new("cache"), root),
        Path::new("/project/cache")
    );
    assert_eq!(
        join_project(Path::new("./cache"), root),
        Path::new("/project/./cache")
    );
    assert_eq!(
        join_project(Path::new("~/cache"), root),
        Path::new("~/cache")
    );
    assert_eq!(
        join_project(Path::new("~user/cache"), root),
        Path::new("~user/cache")
    );
    assert_eq!(
        join_project(Path::new("/var/cache"), root),
        Path::new("/var/cache")
    );
}

#[test]
fn expand_home_uses_host_home() {
    let home = Some(Path::new("/home/user"));
    assert_eq!(
        expand_home(Path::new("~/.cache"), home).expect("expands"),
        Path::new("/home/user/.cache")
    );
    assert_eq!(
        expand_home(Path::new("~"), home).expect("bare home"),
        Path::new("/home/user")
    );
    assert_eq!(
        expand_home(Path::new("/abs"), home).expect("absolute"),
        Path::new("/abs")
    );
    let missing = expand_home(Path::new("~/.cache"), None)
        .expect_err("missing home")
        .to_string();
    assert!(missing.contains("HOME"), "{missing}");
    let other = expand_home(Path::new("~other/cache"), home)
        .expect_err("~user")
        .to_string();
    assert!(other.contains("unsupported"), "{other}");
}

#[test]
fn resolve_host_joins_then_expands() {
    let root = Path::new("/project");
    let home = Some(Path::new("/home/user"));
    assert_eq!(
        resolve_host(Path::new("containers/Dockerfile"), root, home).expect("relative"),
        Path::new("/project/containers/Dockerfile")
    );
    assert_eq!(
        resolve_host(Path::new("~/images/Dockerfile"), root, home).expect("home"),
        Path::new("/home/user/images/Dockerfile")
    );
    assert_eq!(
        resolve_host(Path::new("/opt/Dockerfile"), root, home).expect("absolute"),
        Path::new("/opt/Dockerfile")
    );
}

#[test]
fn is_host_source_rejects_other_users() {
    assert!(is_host_source(Path::new("/tmp/cache")));
    assert!(is_host_source(Path::new("cache")));
    assert!(is_host_source(Path::new("./cache")));
    assert!(is_host_source(Path::new("~/cache")));
    assert!(is_host_source(Path::new("~")));
    assert!(!is_host_source(Path::new("~user/cache")));
}

#[test]
fn container_target_uses_guest_home() {
    let project = Path::new("/home/silo/repo");
    let home = Path::new("/home/silo");
    assert_eq!(
        container_target(Path::new("~/.cargo"), Some(project), home),
        Some(PathBuf::from("/home/silo/.cargo"))
    );
    assert_eq!(
        container_target(Path::new("./target"), Some(project), home),
        Some(PathBuf::from("/home/silo/repo/target"))
    );
    assert_eq!(
        container_target(Path::new("/output"), Some(project), home),
        Some(PathBuf::from("/output"))
    );
    assert_eq!(container_target(Path::new("./target"), None, home), None);
    assert_eq!(container_target(Path::new("~"), Some(project), home), None);
    assert_eq!(
        container_target(Path::new("target"), Some(project), home),
        None
    );
    assert_eq!(
        container_target(Path::new("~//.cargo"), Some(project), home),
        Some(PathBuf::from("/home/silo/.cargo"))
    );
    assert_eq!(
        container_target(Path::new(".//target"), Some(project), home),
        Some(PathBuf::from("/home/silo/repo/target"))
    );
}
