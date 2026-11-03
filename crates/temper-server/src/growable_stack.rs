//! Recursive dispatch on a stack that grows onto the heap.
//!
//! A boxed future keeps its state on the heap, but polling a future that
//! awaits another boxed future still nests one poll frame per level on the
//! caller's thread stack, and dropping it nests one drop per level. Work that
//! recurses as deep as its data (a reaction chain, an inline integration
//! callback that runs another action) builds, polls and drops each level
//! through [`on_growable_stack`], and synchronous recursion of the same kind
//! (the simulation dispatcher) runs each level through [`grow`], so a deep
//! chain gets more stack instead of overflowing the thread and taking the
//! process down.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::BoxFuture;

// Both sizes are the ones the inline callback boundary has used since
// fa932974, where a self-callback test overflowed a small test thread's stack.
// They refuse nothing: they say when to switch segments and how big a new one
// is, and a segment is only allocated when a chain is that deep.

/// Stack that must be left before a level is entered; with less, the level
/// runs on a new segment.
const RED_ZONE: usize = 2 * 1024 * 1024;
/// Size of each new segment.
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// Build the future `make` returns, and poll and drop it, on a stack that
/// grows when the current one runs low.
pub(crate) fn on_growable_stack<'a, T: 'a>(
    make: impl FnOnce() -> BoxFuture<'a, T>,
) -> BoxFuture<'a, T> {
    Box::pin(OnGrowableStack { future: grow(make) })
}

/// Run `level` on a stack that grows when the current one runs low.
pub(crate) fn grow<R>(level: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, STACK_BYTES, level)
}

/// A level of a chain, polled and dropped through [`grow`].
struct OnGrowableStack<'a, T: 'a> {
    future: BoxFuture<'a, T>,
}

impl<'a, T: 'a> Future for OnGrowableStack<'a, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<T> {
        let future = &mut self.future;
        grow(|| future.as_mut().poll(context))
    }
}

impl<'a, T: 'a> Drop for OnGrowableStack<'a, T> {
    /// A pending level holds the next level, which holds the one after it,
    /// so dropping a chain drops every level one inside another. The level's
    /// future is swapped for one that holds nothing and dropped here, where
    /// the stack can grow.
    fn drop(&mut self) {
        let future = std::mem::replace(&mut self.future, Box::pin(std::future::pending()));
        grow(|| drop(future));
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Waker};

    use futures_util::FutureExt;
    use futures_util::future::BoxFuture;

    use super::on_growable_stack;

    /// A chain of `levels` levels, each awaiting the next on a growable stack,
    /// whose last level never finishes: a reaction chain cancelled part way.
    fn pending_chain(levels: usize) -> BoxFuture<'static, ()> {
        on_growable_stack(move || {
            async move {
                if levels == 0 {
                    std::future::pending::<()>().await;
                } else {
                    pending_chain(levels - 1).await;
                }
            }
            .boxed()
        })
    }

    /// Dropping a chain drops every level it holds, one inside another, so
    /// dropping a deep one needs as much stack as polling it. A thread that
    /// overflows its stack aborts the process, so this fails by crashing.
    #[test]
    fn a_deep_pending_chain_is_dropped_on_a_small_thread() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut chain = pending_chain(20_000);
                let mut context = Context::from_waker(Waker::noop());
                assert!(chain.as_mut().poll(&mut context).is_pending());
                drop(chain);
            })
            .expect("spawn the test thread")
            .join()
            .expect("the chain was polled and dropped");
    }
}
