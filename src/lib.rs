//! # Konbini
//!
//! Konbini provides a [`Store`] for managing state in Rust applications. It encapsulates mutation,
//! and allows for subscriptions to be registered, so that interested parties can be notified
//! when changes occur. This is useful to update e.g. your user interface or send out notifications
//! over the network for a REST API.
//!
//! You can think of this store like Redux. It manages state, and allows for mutations to be
//! dispatched using the [`Store::write()`] method. Subscriptions can be registered to listen for changes.
//! If you only need immutable access, you can use [`Store::read()`] to get a reference to the state.
//!
//! The comparison doesn't hold entirely. We don't force the usage of immutable data structures,
//! nor is there an explicit reducer function. We make use of Rust's imperative programming model,
//! and update the state directly in the mutation. Users can still choose to use immutable
//! data types (such as with the [`im` crate](https://lib.rs/crates/im)), and write the new value to
//! the state reference (allowing for old versions of the data to live on immutably).
//!
//! Writes to one store are serialized, so subscribers are notified in the order writes were applied,
//! each with the state exactly as its write left it.
//!
//! Subscribers are called synchronously, while the store holds its state and subscription locks.
//! A subscriber must therefore not read, write, subscribe to or unsubscribe from the store that
//! is notifying it, whether directly or by waiting on another thread that does; either would
//! deadlock. Debug builds panic instead when the subscriber does so directly.

use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, Mutex, PoisonError, RwLock},
};

/// A store for some arbitrary state and mutations that can be applied to it
pub struct Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
{
    /// The state that is being managed
    /// We wrap this in an `RwLock` to allow for concurrent reads and exclusive writes
    state: RwLock<T>,

    /// The subscriptions to the store
    /// These are the parties interested in hearing about mutation outputs
    /// We wrap this in a `Mutex` to allow for concurrent access
    subscriptions: Arc<Mutex<SubscriptionMap<T, Mutation::Output>>>,

    /// Held for the whole of a write, including its notifications
    ///
    /// This serializes writes, so no write can apply while another is notifying
    dispatch: Mutex<()>,
}

impl<T, Mutation> Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
{
    /// Create a new store
    pub fn new(initial_state: T) -> Self {
        Self {
            state: RwLock::new(initial_state),
            subscriptions: Arc::new(Mutex::new(SubscriptionMap::new())),
            dispatch: Mutex::new(()),
        }
    }

    /// Access the state immutably
    pub fn read(&self) -> impl Deref<Target = T> + use<'_, T, Mutation> {
        notifying::assert_not::<T>(&self.subscriptions, "read");

        self.state
            .read()
            .expect("Could not lock editor state for reading")
    }

    /// Mutate the state, then notify subscribers before returning
    ///
    /// Writes to one store are serialized: subscribers see each write's output with the state exactly
    /// as that write left it, in the order writes were applied. A call blocks while another thread's
    /// write is in progress, including its notifications, so slow subscribers delay writers on other
    /// threads.
    pub fn write(
        &self,
        mutation: Mutation,
        args: &Mutation::Args,
    ) -> Result<Mutation::Output, Mutation::Error> {
        notifying::assert_not::<T>(&self.subscriptions, "write");

        // Poison is ignored because this mutex guards no data
        let _dispatch = self.dispatch.lock().unwrap_or_else(PoisonError::into_inner);

        // First, mutate the state
        let output = {
            let mut state = self
                .state
                .write()
                .expect("Could not lock editor state for writing");

            mutation.apply(&mut state, args)?
        };

        // Lock the state again, immutably this time; the dispatch lock keeps other writers out in between
        let state = self.read();

        // Lock the subscriptions
        let subscriptions = self
            .subscriptions
            .lock()
            .expect("Could not lock subscriptions for output distribution");

        // Then, send out the outputs
        let _notifying = notifying::enter(&self.subscriptions);
        for subscription in subscriptions.map.values() {
            subscription(&state, &output);
        }

        Ok(output)
    }

    /// Register a new subscriber to the store
    ///
    /// The resulting type, [`StoreSubscription`], is a handle to the subscription.
    /// When it is dropped, the subscription is removed.
    pub fn subscribe(
        &self,
        listener: impl Fn(&T, &Mutation::Output) + Send + Sync + 'static,
    ) -> StoreSubscription<T, Mutation::Output> {
        notifying::assert_not::<T>(&self.subscriptions, "subscribe");

        let key = {
            let mut subscriptions = self
                .subscriptions
                .lock()
                .expect("Could not lock subscriptions for subscription");

            subscriptions.insert(Box::new(listener))
        };

        StoreSubscription {
            subscriptions: self.subscriptions.clone(),
            key,
        }
    }
}

impl<T, Mutation> Default for Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
    T: Default,
{
    fn default() -> Self {
        Self::new(T::default())
    }
}

type SubscriptionCallback<T, Output> = Box<dyn Fn(&T, &Output) + Send + Sync>;

struct SubscriptionMap<T, Output> {
    map: HashMap<u64, SubscriptionCallback<T, Output>>,
    next_key: u64,
}

impl<T, Output> SubscriptionMap<T, Output> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_key: 0,
        }
    }

    pub fn insert(&mut self, callback: SubscriptionCallback<T, Output>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.map.insert(key, callback);
        key
    }

    pub fn remove(&mut self, key: u64) {
        self.map.remove(&key);
    }
}

/// Interface for store mutations
///
/// This trait is used to model mutations for a [`Store`]. The generic type
/// `T` is the state being mutated on.
pub trait StoreMutation<T> {
    /// Extra arguments that can be passed along when applying the mutation
    type Args;

    /// The type of output that can result from an applying the mutation
    type Output;

    /// The type of error that can happen when applying the mutation
    type Error;

    /// Mutate the state
    fn apply(self, state: &mut T, args: &Self::Args) -> Result<Self::Output, Self::Error>;
}

/// An active subscription in the [`Store`]
///
/// The subscription gets removed when it is dropped.
#[must_use = "Subscriptions are removed when dropped"]
pub struct StoreSubscription<T, Output> {
    subscriptions: Arc<Mutex<SubscriptionMap<T, Output>>>,
    key: u64,
}

impl<T, Output> Drop for StoreSubscription<T, Output> {
    fn drop(&mut self) {
        notifying::assert_not::<T>(&self.subscriptions, "unsubscribe");

        self.subscriptions
            .lock()
            .expect("Could not lock subscriptions for unsubscription")
            .remove(self.key);
    }
}

/// Tracks which stores the current thread is notifying, so re-entrant access panics instead of deadlocking
///
/// A store is identified by the address of its subscription map, which [`StoreSubscription`] shares.
/// Only debug builds track; release builds compile this down to nothing.
#[cfg(debug_assertions)]
mod notifying {
    use std::{cell::RefCell, sync::Arc};

    thread_local! {
        static STORES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    }

    /// Marks the store as notifying on this thread until the guard drops
    pub(crate) fn enter<S>(subscriptions: &Arc<S>) -> Guard {
        STORES.with_borrow_mut(|stores| stores.push(id(subscriptions)));
        Guard
    }

    /// Panics if this thread is inside a notification of the store
    pub(crate) fn assert_not<T>(subscriptions: &Arc<impl ?Sized>, action: &str) {
        if STORES.with_borrow(|stores| stores.contains(&id(subscriptions))) {
            panic!(
                "Cannot {action} the `{}` store from inside one of its own notifications: it would deadlock",
                std::any::type_name::<T>()
            );
        }
    }

    fn id(subscriptions: &Arc<impl ?Sized>) -> usize {
        Arc::as_ptr(subscriptions).cast::<()>() as usize
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            STORES.with_borrow_mut(|stores| stores.pop());
        }
    }
}

#[cfg(not(debug_assertions))]
mod notifying {
    use std::sync::Arc;

    pub(crate) fn enter<S>(_: &Arc<S>) -> Guard {
        Guard
    }

    pub(crate) struct Guard;

    pub(crate) fn assert_not<T>(_: &Arc<impl ?Sized>, _: &str) {}
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    type Counter = Store<u32, Add>;

    struct Add(u32);

    impl StoreMutation<u32> for Add {
        type Args = ();
        type Output = u32;
        type Error = ();

        fn apply(self, state: &mut u32, _: &()) -> Result<u32, ()> {
            *state += self.0;
            Ok(*state)
        }
    }

    /// Writes once to a store whose only subscriber runs `effect` with that same store
    ///
    /// Subscriptions are leaked: a panicking subscriber poisons the subscription mutex, and dropping
    /// a subscription while unwinding would then panic again and abort.
    fn notify_into_self(effect: impl Fn(&Counter) + Send + Sync + 'static) {
        let store = Arc::new(Counter::new(0));
        std::mem::forget(store.subscribe({
            let store = Arc::downgrade(&store);
            move |_, _| effect(&store.upgrade().unwrap())
        }));
        let _ = store.write(Add(1), &());
    }

    #[test]
    #[should_panic(
        expected = "Cannot write the `u32` store from inside one of its own notifications"
    )]
    fn write_from_own_notification_panics() {
        notify_into_self(|store| {
            let _ = store.write(Add(1), &());
        });
    }

    #[test]
    #[should_panic(expected = "Cannot read the `u32` store")]
    fn read_from_own_notification_panics() {
        notify_into_self(|store| {
            let _ = *store.read();
        });
    }

    #[test]
    #[should_panic(expected = "Cannot subscribe the `u32` store")]
    fn subscribe_from_own_notification_panics() {
        notify_into_self(|store| {
            let _ = store.subscribe(|_, _| {});
        });
    }

    #[test]
    #[should_panic(expected = "Cannot unsubscribe the `u32` store")]
    fn unsubscribe_from_own_notification_panics() {
        let store = Arc::new(Counter::new(0));
        let doomed = Arc::new(Mutex::new(Some(store.subscribe(|_, _| {}))));
        std::mem::forget(store.subscribe(move |_, _| drop(doomed.lock().unwrap().take())));
        let _ = store.write(Add(1), &());
    }

    #[test]
    fn writing_another_store_from_a_notification_is_allowed() {
        let downstream = Arc::new(Counter::new(0));
        let upstream = Counter::new(0);
        let _subscription = upstream.subscribe({
            let downstream = downstream.clone();
            move |_, &value| {
                downstream.write(Add(value), &()).unwrap();
            }
        });

        upstream.write(Add(2), &()).unwrap();
        upstream.write(Add(3), &()).unwrap();

        assert_eq!(*downstream.read(), 7);
    }

    #[test]
    fn notifications_match_state_and_arrive_in_write_order() {
        let store = Arc::new(Counter::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _subscription = store.subscribe({
            let seen = seen.clone();
            move |&state, &output| seen.lock().unwrap().push((state, output))
        });

        let writers: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        store.write(Add(1), &()).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let seen = seen.lock().unwrap();
        let expected: Vec<_> = (1..=4000).map(|value| (value, value)).collect();
        assert_eq!(*seen, expected);
    }

    #[test]
    fn store_is_usable_after_a_panicking_subscriber() {
        let store = Arc::new(Counter::new(0));
        std::mem::forget(store.subscribe(|_, _| panic!("subscriber failed")));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.write(Add(1), &());
        }));
        assert!(result.is_err());

        // The panic poisoned the subscription mutex, but must not leave the store marked as notifying
        notifying::assert_not::<u32>(&store.subscriptions, "read");
    }
}
