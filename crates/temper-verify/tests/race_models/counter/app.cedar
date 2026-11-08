permit(principal, action in [Action::"Ask"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
