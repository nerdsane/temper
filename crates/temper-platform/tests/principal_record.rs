//! A type marked `principal_record` describes a user to Cedar: the status of
//! the record keyed by the user's `sub` is on the principal, read fresh on
//! every call, in person (`principal.Member`) or for an agent's user
//! (`principal.actingForMember`). A record's `principal_record_set` list is
//! there too, as a Cedar set (`principal.Role_roles`).

#[path = "principal_record/fixture.rs"]
mod fixture;

#[path = "principal_record/calls.rs"]
mod calls;

#[path = "principal_record/dst.rs"]
mod dst;

#[path = "principal_record/roles.rs"]
mod roles;
