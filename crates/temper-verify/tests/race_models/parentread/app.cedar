permit(principal, action in [Action::"Raise", Action::"Clear"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
