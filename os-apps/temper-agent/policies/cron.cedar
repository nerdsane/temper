// CronJob + CronScheduler — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is CronJob
);

permit(
  principal is Admin,
  action,
  resource is CronScheduler
);

// Only supervisors/humans can configure and activate cron jobs
permit(
  principal,
  action in [Action::"create", Action::"Configure", Action::"Activate", Action::"Pause", Action::"Resume", Action::"Expire"],
  resource is CronJob
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// System agents can trigger cron jobs (called by CronScheduler WASM)
permit(
  principal,
  action in [Action::"Trigger", Action::"TriggerComplete", Action::"TriggerFailed"],
  resource is CronJob
) when {
  principal.agent_type == "system"
};

// Any authenticated agent can read cron jobs
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is CronJob
);

// System agents manage the scheduler lifecycle
permit(
  principal,
  action in [Action::"create", Action::"Start", Action::"CheckComplete", Action::"CheckFailed", Action::"ScheduledCheck", Action::"ScheduleFailed"],
  resource is CronScheduler
) when {
  principal.agent_type == "system"
};

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is CronScheduler
);
