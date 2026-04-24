// TemperFS FileVersion Authorization Policies

permit(
    principal is Agent,
    action in [Action::"Create", Action::"Supersede"],
    resource is FileVersion
) when {
    principal.agent_type == "file-service"
};
