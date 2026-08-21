mod inventory;
mod lifecycle;
mod management;
mod mounts;
mod process;
mod runtime;

pub(crate) use lifecycle::run_session;
pub(crate) use management::{delete_selected_container, print_containers};

/// Validates config values and project-relative targets without runtime state.
pub(crate) fn validate_config(
    config: &crate::config::Config,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    runtime::validate_config(&config.container)?;
    match crate::project::shared_dir_name(project_root) {
        Ok(project_dir) => {
            crate::apple::mount_argument_path(project_root)?;
            crate::apple::mount_argument_path(&project_dir)?;
            let read_only =
                mounts::resolve_read_only_paths(project_root, &config.workspace.read_only);
            mounts::validate_project_targets(config, &project_dir, &read_only)
        }
        // The filesystem root has no container workdir. Config-only
        // inspection from `/` still validates context-independent syntax.
        Err(_) => mounts::validate_config(config),
    }
}

/// Resolves every host-backed configuration source without creating managed
/// state or contacting the container runtime.
pub(crate) fn validate_project_filesystem(
    config: &crate::config::Config,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    mounts::resolve_config_mounts(config, project_root, None).map(|_| ())
}

#[cfg(test)]
mod tests;
