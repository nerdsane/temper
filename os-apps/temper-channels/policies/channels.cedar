// Temper Channels — Cedar Authorization Policies

// Admins can do everything
permit(
  principal is Admin,
  action,
  resource is Channel
);

permit(
  principal is Admin,
  action,
  resource is AgentRoute
);

permit(
  principal is Admin,
  action,
  resource is ChannelSession
);

// Supervisors and humans can manage channels and routes
permit(
  principal,
  action in [Action::"create", Action::"Configure", Action::"Connect", Action::"Disconnect", Action::"Reconnect", Action::"Archive"],
  resource is Channel
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

permit(
  principal,
  action in [Action::"create", Action::"Register", Action::"Update", Action::"Disable", Action::"Enable"],
  resource is AgentRoute
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// System agents can handle channel callbacks and session management
permit(
  principal,
  action in [Action::"Ready", Action::"ReceiveMessage", Action::"SendReply", Action::"ReplyDelivered", Action::"ConnectFailed", Action::"RouteFailed", Action::"ReplyFailed"],
  resource is Channel
) when {
  principal.agent_type == "system"
};

permit(
  principal,
  action in [Action::"create", Action::"Create", Action::"Resume", Action::"Expire"],
  resource is ChannelSession
) when {
  ["system", "supervisor", "human"].contains(principal.agent_type)
};

// Any authenticated agent can read
permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is Channel
);

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is AgentRoute
);

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is ChannelSession
);

permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  ["channel_connect", "route_message", "send_reply"].contains(context.module)
};
