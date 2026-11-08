permit(principal, action in [Action::"Start", Action::"Keep", Action::"Drop", Action::"Poke", Action::"Prod"], resource);
permit(principal, action == Action::"Ping", resource) when { context has causeAction && context.causeAction == "Drop" };
