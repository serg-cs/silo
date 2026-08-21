use std::path::{Path, PathBuf};

use super::*;

fn mount(id: &str, name: &str, owner: StateOwner) -> ManagedMount {
    ManagedMount {
        id: id.to_string(),
        name: name.to_string(),
        path: PathBuf::from(format!("/state/{name}")),
        owner,
    }
}

#[test]
fn selectors_prefer_exact_ids_and_require_unique_names() {
    let mounts = [
        mount("state-user", "cache", StateOwner::User),
        mount(
            "state-project",
            "cache",
            StateOwner::Project(Path::new("/work/one").into()),
        ),
    ];

    assert_eq!(
        select_mount(&mounts, "state-user").unwrap().id,
        "state-user"
    );
    assert!(
        select_mount(&mounts, "cache")
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
}

#[test]
fn list_output_is_compact_and_preserves_full_identity() {
    let mounts = [mount("state-full-id", "cache", StateOwner::User)];

    assert_eq!(
        render_state_list(&mounts),
        "ID\tSCOPE\tNAME\tPROJECT\tSOURCE\nstate-full-id\tuser\tcache\t-\t/state/cache"
    );
}
