//! Deterministic dependency-first OS-app ordering.

use std::collections::HashSet;

use super::super::os_app_dependencies;

fn collect_install_order_with_dependencies(
    app_name: &str,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
    dependencies: &impl Fn(&str) -> Vec<String>,
) -> Result<(), String> {
    if visited.contains(app_name) {
        return Ok(());
    }
    if !visiting.insert(app_name.to_string()) {
        return Err(format!("Cyclic OS app dependency detected at '{app_name}'"));
    }
    for dependency in dependencies(app_name) {
        collect_install_order_with_dependencies(
            &dependency,
            visiting,
            visited,
            order,
            dependencies,
        )?;
    }
    visiting.remove(app_name);
    visited.insert(app_name.to_string());
    order.push(app_name.to_string());
    Ok(())
}

/// Resolve a deduplicated dependency-first install order for a set of apps.
pub fn resolve_os_app_install_order(app_names: &[String]) -> Result<Vec<String>, String> {
    resolve_os_app_install_order_with_dependencies(app_names, |app_name| {
        os_app_dependencies(app_name)
    })
}

pub(crate) fn resolve_os_app_install_order_with_dependencies(
    app_names: &[String],
    dependencies: impl Fn(&str) -> Vec<String>,
) -> Result<Vec<String>, String> {
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    let mut order = Vec::new();
    for app_name in app_names {
        collect_install_order_with_dependencies(
            app_name,
            &mut visiting,
            &mut visited,
            &mut order,
            &dependencies,
        )?;
    }
    Ok(order)
}
