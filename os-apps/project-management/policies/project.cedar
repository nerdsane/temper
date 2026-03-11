// Project Management — Project, Cycle, Label, Comment Cedar Policies
//
// Permit-all for these supporting entity types.
// Fine-grained access control can be added per entity type as needed.

permit(
  principal,
  action,
  resource is Project
);

permit(
  principal,
  action,
  resource is Cycle
);

permit(
  principal,
  action,
  resource is Label
);

permit(
  principal,
  action,
  resource is Comment
);
