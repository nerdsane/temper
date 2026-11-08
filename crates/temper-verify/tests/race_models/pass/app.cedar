permit(principal, action in [Action::"Stage", Action::"TimedOut"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
