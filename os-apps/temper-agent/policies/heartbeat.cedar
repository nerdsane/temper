// HeartbeatMonitor — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is HeartbeatMonitor
);

// System agents manage the monitor lifecycle
permit(
  principal,
  action in [Action::"create", Action::"Start", Action::"ScanComplete", Action::"ScanFailed", Action::"ScheduledScan", Action::"ScheduleFailed"],
  resource is HeartbeatMonitor
) when {
  principal.agent_type == "system"
};

// Any authenticated agent can read
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is HeartbeatMonitor
);
