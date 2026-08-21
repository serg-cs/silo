use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::test_support::test_dir;

fn git_at(path: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(path);
    command
}

fn run_git(mut command: Command) {
    assert!(
        command.status().expect("Git starts").success(),
        "Git fixture command succeeds"
    );
}

#[test]
fn project_ids_are_stable_and_path_specific() {
    let first = project_container_id(Path::new("/work/one/project"));
    assert_eq!(first, project_container_id(Path::new("/work/one/project")));
    assert_ne!(first, project_container_id(Path::new("/work/two/project")));
    assert_eq!(
        first.len(),
        CONTAINER_NAME_PREFIX.len() + PROJECT_DIGEST_HEX_LEN
    );
}

#[test]
fn explicit_marker_is_selected_from_ancestors() {
    let dir = test_dir("marker-priority");
    let outer = dir.path().join("workspace");
    let inner = outer.join("package");
    let cwd = inner.join("src");
    fs::create_dir_all(&cwd).expect("project directories create");
    fs::write(outer.join(PROJECT_MARKER), "").expect("marker creates");

    let project = Project::from_path(&cwd).expect("project resolves");
    assert_eq!(
        project.root,
        fs::canonicalize(&outer).expect("root resolves")
    );
    assert_eq!(project.workdir, PathBuf::from("/home/silo/workspace"));
}

#[test]
fn jujutsu_marker_is_not_discovered_directly() {
    let dir = test_dir("vcs-markers");
    let inner = dir.path().join("inner");
    let cwd = inner.join("src");
    fs::create_dir_all(&cwd).expect("project directories create");
    fs::create_dir(inner.join(".jj")).expect("Jujutsu directory creates");

    let cwd = fs::canonicalize(cwd).expect("project directory resolves");
    assert_eq!(discover_project_root(&cwd), cwd);
}

#[test]
fn git_command_is_scoped_to_the_canonical_working_directory() {
    let command = git_root_command(Path::new("/workspace/project"));

    assert_eq!(command.get_program(), "git");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-C", "/workspace/project", "rev-parse", "--show-toplevel"]
    );
}

#[test]
fn git_output_must_resolve_to_an_ancestor_directory() {
    let dir = test_dir("git-output");
    let root = dir.path().join("workspace");
    let cwd = root.join("src");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&cwd).expect("project directories create");
    fs::create_dir(&outside).expect("outside directory creates");

    let canonical_root = fs::canonicalize(&root).expect("root resolves");
    let canonical_cwd = fs::canonicalize(&cwd).expect("working directory resolves");
    let mut output = canonical_root.as_os_str().as_bytes().to_vec();
    output.push(b'\n');
    assert_eq!(
        git_root_from_stdout(&canonical_cwd, &output),
        Some(canonical_root)
    );

    let canonical_outside = fs::canonicalize(outside).expect("outside directory resolves");
    let mut outside_output = canonical_outside.as_os_str().as_bytes().to_vec();
    outside_output.push(b'\n');
    assert_eq!(git_root_from_stdout(&canonical_cwd, &outside_output), None);
    assert_eq!(git_root_from_stdout(&canonical_cwd, b"\n"), None);
    assert_eq!(git_root_from_stdout(&canonical_cwd, b"/one\n/two\n"), None);
    assert_eq!(git_root_from_stdout(&canonical_cwd, b"/\n"), None);

    let invalid_root = dir.path().join("invalid,root");
    let invalid_cwd = invalid_root.join("src");
    fs::create_dir_all(&invalid_cwd).expect("invalid fixture creates");
    let invalid_root = fs::canonicalize(invalid_root).expect("invalid root resolves");
    let invalid_cwd = fs::canonicalize(invalid_cwd).expect("invalid working directory resolves");
    let mut invalid_output = invalid_root.as_os_str().as_bytes().to_vec();
    invalid_output.push(b'\n');
    assert_eq!(git_root_from_stdout(&invalid_cwd, &invalid_output), None);
}

#[test]
fn git_discovers_a_linked_worktree_without_parsing_its_gitfile() {
    if !Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let dir = test_dir("linked-worktree");
    let repository = dir.path().join("repository");
    let worktree = dir.path().join("worktree");
    let source = worktree.join("src");
    fs::create_dir(&repository).expect("repository directory creates");
    let mut init = git_at(&repository);
    init.args(["init", "--quiet"]);
    run_git(init);
    fs::write(repository.join("README.md"), "workspace\n").expect("fixture file creates");
    let mut add = git_at(&repository);
    add.args(["add", "README.md"]);
    run_git(add);
    let mut commit = git_at(&repository);
    commit.args([
        "-c",
        "user.name=Silo Tests",
        "-c",
        "user.email=silo@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
    run_git(commit);
    let mut add_worktree = git_at(&repository);
    add_worktree.args(["worktree", "add", "--quiet"]);
    add_worktree.arg(&worktree);
    run_git(add_worktree);
    fs::create_dir(&source).expect("worktree source directory creates");

    let canonical_worktree = fs::canonicalize(&worktree).expect("worktree resolves");
    let canonical_source = fs::canonicalize(&source).expect("source resolves");
    assert_eq!(discover_project_root(&canonical_source), canonical_worktree);
    assert!(worktree.join(".git").is_file());
}

#[test]
fn project_mount_paths_follow_structured_syntax() {
    assert!(shared_dir_name(Path::new("/")).is_err());
    assert!(Project::from_root(PathBuf::from("/tmp/a:b")).is_ok());
    assert!(Project::from_root(PathBuf::from("/tmp/a,b")).is_err());
    assert!(Project::from_root(PathBuf::from("/tmp/a=b")).is_err());
}
