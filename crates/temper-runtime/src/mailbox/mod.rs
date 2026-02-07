// Mailbox implementations.
// Currently the ActorCell uses tokio::mpsc::unbounded_channel directly.
// This module will house bounded, priority, and stashing mailbox variants
// as the runtime evolves.
//
// Planned:
// - BoundedMailbox { capacity: usize } — backpressure via bounded channel
// - PriorityMailbox — system signals processed before user messages
// - StashingMailbox — stash messages during state transitions
