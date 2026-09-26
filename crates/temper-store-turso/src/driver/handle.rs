//! A safe auto-trait boundary around embedded-engine handles.

use std::ops::{Deref, DerefMut};

trait ErasedHandle<T>: Send + Sync {
    fn get(&self) -> &T;
    fn get_mut(&mut self) -> &mut T;
    fn into_inner(self: Box<Self>) -> T;
}

impl<T: Send + Sync> ErasedHandle<T> for T {
    fn get(&self) -> &T {
        self
    }

    fn get_mut(&mut self) -> &mut T {
        self
    }

    fn into_inner(self: Box<Self>) -> T {
        *self
    }
}

/// Prevent callers' Send/Sync proofs from traversing the embedded SQL engine.
///
/// Unlike `Box<T>`, the trait object states these bounds explicitly. Construction
/// checks them without unsafe code. The lifetime also supports borrowed native
/// transactions; ownership and the wrapped value's Drop behavior are unchanged.
/// This allocates once per handle/result stream, never once per result row.
pub(crate) struct DriverHandle<'a, T>(Box<dyn ErasedHandle<T> + 'a>);

impl<'a, T: Send + Sync + 'a> DriverHandle<'a, T> {
    pub(super) fn new(value: T) -> Self {
        Self(Box::new(value))
    }
}

impl<T> DriverHandle<'_, T> {
    pub(super) fn into_inner(self) -> T {
        self.0.into_inner()
    }
}

impl<T> Deref for DriverHandle<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.as_ref().get()
    }
}

impl<T> DerefMut for DriverHandle<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0.as_mut().get_mut()
    }
}
