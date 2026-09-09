// DSF Deploy — DeployRun Cedar Policies
//
// Howl and TemperPaw both drive this machine. The walk is the gate:
// Cedar permits the named actions; the IOA refuses skipped states.

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is DeployRun
);

permit(
  principal is Agent,
  action in [
    Action::"create",
    Action::"Arm",
    Action::"ProbeHealth",
    Action::"MarkReady",
    Action::"StartDeploy",
    Action::"RecordDeploy",
    Action::"StartVerify",
    Action::"MarkHealthy",
    Action::"MarkFailed",
    Action::"Cancel",
    Action::"ResumeOnComputer"
  ],
  resource is DeployRun
);

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  context.module == "dsf_deploy"
};

permit(
  principal is Agent,
  action == Action::"access_secret",
  resource is Secret
) when {
  context.module == "dsf_deploy"
};
