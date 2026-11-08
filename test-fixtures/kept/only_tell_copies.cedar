// Only the parent's `tell` service may send the child its word.
permit(principal, action == Action::"Close", resource);
permit(principal == Agent::"service:tell", action == Action::"Copy", resource is Child);
