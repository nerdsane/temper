//! The steps an inline execution is inside of.
//!
//! An action can set off more work that its caller waits for: a reaction on
//! another entity, or an inline integration whose callback runs another
//! action. Each of those is a step, and the steps one request is inside of
//! form a chain. A chain stops the first time it would take a step it has
//! already taken: the same action on the same entity. Nothing else limits how
//! long a chain may be.
//!
//! Why a repeat, and not a count: a chain that comes back to a step it is
//! already inside of is a loop, and a loop in work the caller waits for has
//! no end the server can see. The request would never answer, and every lap
//! would commit an action and write events. A chain that never repeats is
//! bounded by the entities it can reach, so it is the app's own work and runs
//! to its end.
//!
//! An entity the chain made itself (the target of a `create` rule, a
//! `create_if_missing` target whose id the rule made up, or a child spawned by
//! one of the chain's actions) is new on every lap of a loop, so its id can
//! never repeat. Steps on such entities are compared by type and action
//! alone; otherwise a loop that makes an entity each lap would never stop.
//!
//! A detached task (a background integration, a state timer, a scheduled
//! action, compensation) starts a new chain: the caller is not waiting for
//! it, so a loop through it is paced by that work and counted by
//! [`MAX_CALLBACK_HOPS`](super::MAX_CALLBACK_HOPS) instead. A spawned child's
//! initial action is the next step of its parent's chain, on an entity the
//! chain made, so a loop through spawns stops at its first repeat too.
//!
//! So a chain is bounded by its data: every entity it makes is one step per
//! type and action the tenant's specs declare, and every other step is on an
//! entity whose id it read (reaction ids and params come only from literals,
//! the source's id and its fields).

use std::fmt;
use std::sync::Arc;

/// One step of a chain: an action on an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStep {
    /// The entity's type.
    pub entity_type: String,
    /// Which entity of that type.
    pub entity: StepEntity,
    /// The action.
    pub action: String,
}

/// Which entity a step acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepEntity {
    /// An entity the chain found, by its id.
    Found(String),
    /// An entity the chain made. Its id is new on every lap of a loop, so
    /// steps on made entities are told apart by type and action alone.
    Made,
}

impl fmt::Display for ChainStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.entity {
            StepEntity::Found(id) => write!(f, "{}({id}).{}", self.entity_type, self.action),
            StepEntity::Made => write!(f, "{}(new).{}", self.entity_type, self.action),
        }
    }
}

/// The steps a chain would go round if it took a step again: from that
/// step's earlier place in the chain to now, then the step itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedStep {
    /// Oldest first; the last step is the first one again.
    pub cycle: Vec<ChainStep>,
}

impl RepeatedStep {
    /// The repeat `step` would make in `steps` (oldest first), if it is
    /// already there.
    pub fn find(steps: &[ChainStep], step: &ChainStep) -> Option<Self> {
        let start = steps.iter().rposition(|taken| taken == step)?;
        let mut cycle = steps[start..].to_vec();
        cycle.push(step.clone());
        Some(Self { cycle })
    }

    /// Each step of the cycle as text.
    pub fn path(&self) -> Vec<String> {
        self.cycle.iter().map(ToString::to_string).collect()
    }
}

impl fmt::Display for RepeatedStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path().join(" -> "))
    }
}

/// The steps an inline execution is inside of, and the entities it made.
///
/// Cloning is cheap: entries are a linked list that clones share.
#[derive(Debug, Clone, Default)]
pub struct InlineChain {
    newest: Option<Arc<Link>>,
}

#[derive(Debug)]
struct Link {
    entry: Entry,
    previous: Option<Arc<Link>>,
}

#[derive(Debug)]
enum Entry {
    Step(ChainStep),
    Made {
        entity_type: String,
        entity_id: String,
    },
}

impl Drop for Link {
    // Unlink iteratively: dropping a long chain recursively would use one
    // stack frame per entry.
    fn drop(&mut self) {
        let mut previous = self.previous.take();
        while let Some(link) = previous {
            match Arc::try_unwrap(link) {
                Ok(mut link) => previous = link.previous.take(),
                Err(_) => break,
            }
        }
    }
}

impl InlineChain {
    /// Whether the chain has taken no step and made nothing.
    pub fn is_empty(&self) -> bool {
        self.newest.is_none()
    }

    /// The steps taken, oldest first.
    pub fn steps(&self) -> Vec<ChainStep> {
        let mut steps: Vec<ChainStep> = self.newest_steps().cloned().collect();
        steps.reverse();
        steps
    }

    /// The step `action` on this entity would be, in this chain.
    pub fn step(&self, entity_type: &str, entity_id: &str, action: &str) -> ChainStep {
        let entity = if self.made(entity_type, entity_id) {
            StepEntity::Made
        } else {
            StepEntity::Found(entity_id.to_string())
        };
        ChainStep {
            entity_type: entity_type.to_string(),
            entity,
            action: action.to_string(),
        }
    }

    /// Take `step` next, unless the chain has already taken it.
    pub fn then(&self, step: ChainStep) -> Result<Self, RepeatedStep> {
        // Only a repeat pays for listing the chain.
        if self.newest_steps().any(|taken| *taken == step)
            && let Some(repeat) = RepeatedStep::find(&self.steps(), &step)
        {
            return Err(repeat);
        }
        Ok(self.push(Entry::Step(step)))
    }

    /// Mark the chain as inside `step`. A step just admitted with
    /// [`then`](Self::then) is already the newest and is left as it is.
    pub fn entering(&self, step: ChainStep) -> Self {
        if self.newest_steps().next() == Some(&step) {
            return self.clone();
        }
        self.push(Entry::Step(step))
    }

    /// Record an entity this chain made.
    pub fn making(&self, entity_type: &str, entity_id: &str) -> Self {
        self.push(Entry::Made {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        })
    }

    fn made(&self, entity_type: &str, entity_id: &str) -> bool {
        self.entries().any(|entry| {
            matches!(entry, Entry::Made { entity_type: made_type, entity_id: made_id }
                if made_type == entity_type && made_id == entity_id)
        })
    }

    fn push(&self, entry: Entry) -> Self {
        Self {
            newest: Some(Arc::new(Link {
                entry,
                previous: self.newest.clone(),
            })),
        }
    }

    fn entries(&self) -> impl Iterator<Item = &Entry> {
        std::iter::successors(self.newest.as_deref(), |link| link.previous.as_deref())
            .map(|link| &link.entry)
    }

    fn newest_steps(&self) -> impl Iterator<Item = &ChainStep> {
        self.entries().filter_map(|entry| match entry {
            Entry::Step(step) => Some(step),
            Entry::Made { .. } => None,
        })
    }
}
