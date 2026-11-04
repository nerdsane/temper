permit(principal, action in [Action::"Place", Action::"Pack", Action::"Move", Action::"Stock"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
