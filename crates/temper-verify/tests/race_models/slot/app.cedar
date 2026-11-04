permit(principal, action in [Action::"Take", Action::"Release"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
