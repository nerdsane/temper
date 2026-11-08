permit(principal, action in [Action::"Pick", Action::"Finish"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
