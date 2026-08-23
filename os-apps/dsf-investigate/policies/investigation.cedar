// DSF Investigate — Investigation Cedar Policies
//
// Howl and TemperPaw both drive this machine. The walk is the gate:
// Cedar permits the named actions; the IOA refuses skipped states.

permit(
  principal,
  action in [Action::"create", Action::"read", Action::"list"],
  resource is Investigation
);

permit(
  principal,
  action in [
    Action::"Arm",
    Action::"StartGather",
    Action::"RecordFindings",
    Action::"MarkReady",
    Action::"MarkFailed",
    Action::"Cancel",
    Action::"Resume"
  ],
  resource is Investigation
);

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  context.module == "dsf_investigate"
};

permit(
  principal is Agent,
  action == Action::"access_secret",
  resource is Secret
) when {
  context.module == "dsf_investigate"
};
