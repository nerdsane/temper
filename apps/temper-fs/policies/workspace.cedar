// TemperFS Workspace Authorization Policies

// Agents can create workspaces
permit(
    principal is Agent,
    action == Action::"Create",
    resource is Workspace
);

// Workspace members can read and use their workspace
permit(
    principal is Agent,
    action in [Action::"IncrementUsage", Action::"DecrementUsage", Action::"UpdateQuota"],
    resource is Workspace
) when {
    resource.id == principal.workspaceId
};

// Only workspace admins can freeze, thaw, or archive
permit(
    principal is Agent,
    action in [Action::"Freeze", Action::"Thaw", Action::"WorkspaceArchive"],
    resource is Workspace
) when {
    principal in Group::"WorkspaceAdmins" && resource.id == principal.workspaceId
};
