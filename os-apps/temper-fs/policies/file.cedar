// TemperFS File Authorization Policies

// Agents can create, update, lock, and unlock files in their own workspace
permit(
    principal is Agent,
    action in [Action::"StreamUpdated", Action::"Create", Action::"Lock", Action::"Unlock"],
    resource is File
) when {
    resource.workspaceId == principal.workspaceId
};

// Locked files reject StreamUpdated (defense in depth — transition table also rejects)
forbid(
    principal,
    action == Action::"StreamUpdated",
    resource is File
) when {
    resource.status == "Locked"
};

// Archive requires workspace admin
permit(
    principal is Agent,
    action == Action::"FileArchive",
    resource is File
) when {
    principal in Group::"WorkspaceAdmins" && resource.workspaceId == principal.workspaceId
};
