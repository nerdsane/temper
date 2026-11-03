//! Sender profiles, answer tables and temporary construction share the host room.

use std::cell::Cell;
use std::fmt;

use super::Room;

pub(in crate::composite::joint) struct Construction<'a> {
    room: &'a dyn Room,
    bytes: Cell<usize>,
}

impl<'a> Construction<'a> {
    pub(in crate::composite::joint) fn new(room: &'a dyn Room) -> Self {
        Self {
            room,
            bytes: Cell::new(0),
        }
    }

    /// Reserve the next peak before allocating, releasing temporary room
    /// when construction ends. The retained bytes live as long as the proof.
    pub(in crate::composite::joint) fn set(&self, bytes: usize) -> Result<(), String> {
        let before = self.bytes.get();
        if bytes > before {
            self.room.take(bytes - before)?;
        } else {
            self.room.give(before - bytes);
        }
        self.bytes.set(bytes);
        Ok(())
    }
}

impl fmt::Debug for Construction<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A proof's allowance is accounting, not an input to its digest.
        f.write_str("Construction")
    }
}

impl Drop for Construction<'_> {
    fn drop(&mut self) {
        self.room.give(self.bytes.get());
    }
}
