//! Bootstrap helpers (entity creation during install).
//!
//! Creates App entities, skills, ADRs, and seed data in the tenant after an
//! OS app's specs are registered. TemperFS workspace/directory/file plumbing
//! lives in `fs`.

mod adrs;
mod app;
mod fs;
mod seed;
mod skills;

pub(in crate::os_apps) use adrs::bootstrap_adrs;
pub(in crate::os_apps) use app::bootstrap_app_entity;
pub(in crate::os_apps) use fs::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, content_sha256, ensure_app_docs_workspace, ensure_directory,
    ensure_markdown_file, slug_fragment, state_field_str,
};
pub(in crate::os_apps) use seed::bootstrap_seed_data;
pub(in crate::os_apps) use skills::bootstrap_skills;
