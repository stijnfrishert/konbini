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
//! A store is single-threaded: it lives on the thread that created it, and other threads reach it
//! by sending that thread messages. Writes are therefore applied one at a time, and subscribers are
//! notified in write order.
//!
//! Subscribers are called synchronously, while the store's state is borrowed for reading. A
//! subscriber may read the store that is notifying it, but not write, subscribe to or unsubscribe
//! from it; doing so panics. To write it in response, queue the write and apply it once the
//! notification has finished.
//!
//! Likewise, a mutation may not read or write the store it is applied to, and writing a store while
//! holding a [`Store::read()`] of it panics.

use std::{any::type_name, cell::RefCell, collections::BTreeMap, ops::Deref, rc::Rc};

/// A store for some arbitrary state and mutations that can be applied to it
pub struct Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
{
    /// The state that is being managed
    state: RefCell<T>,

    /// The subscriptions to the store
    ///
    /// These are the parties interested in hearing about mutation outputs
    subscriptions: Rc<RefCell<SubscriptionMap<T, Mutation::Output>>>,
}

impl<T, Mutation> Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
{
    /// Create a new store
    pub fn new(initial_state: T) -> Self {
        Self {
            state: RefCell::new(initial_state),
            subscriptions: Rc::new(RefCell::new(SubscriptionMap::new())),
        }
    }

    /// Access the state immutably
    ///
    /// Panics if called from inside one of the store's own mutations.
    pub fn read(&self) -> impl Deref<Target = T> + use<'_, T, Mutation> {
        self.state.try_borrow().unwrap_or_else(|_| {
            panic!(
                "Cannot read the `{}` store from inside one of its own mutations",
                type_name::<T>()
            )
        })
    }

    /// Mutate the state, then notify subscribers before returning
    ///
    /// A mutation that returns an error notifies no one. If the mutation panics, its partial changes
    /// remain; if a subscriber panics, the change remains and later subscribers are not notified.
    ///
    /// Panics if the store is in use: from inside one of its own notifications or mutations, or
    /// while a [`Store::read()`] of it is held.
    pub fn write(
        &self,
        mutation: Mutation,
        args: &Mutation::Args,
    ) -> Result<Mutation::Output, Mutation::Error> {
        // First, mutate the state
        let output = {
            let mut state = self.state.try_borrow_mut().unwrap_or_else(|_| {
                panic!(
                    "Cannot write the `{}` store while it is in use: from inside one of its own notifications or mutations, or while a `read()` of it is held",
                    type_name::<T>()
                )
            });

            mutation.apply(&mut state, args)?
        };

        // Then, send out the outputs
        let state = self.state.borrow();
        let subscriptions = self.subscriptions.borrow();
        for callback in subscriptions.map.values() {
            callback(&state, &output);
        }

        Ok(output)
    }

    /// Register a new subscriber to the store
    ///
    /// The resulting type, [`StoreSubscription`], is a handle to the subscription.
    /// When it is dropped, the subscription is removed.
    ///
    /// Subscribers are notified in the order they subscribed. Panics if called from inside one of
    /// the store's own notifications.
    ///
    /// A listener that owns its own subscription is never dropped, and one holding a strong handle
    /// to its own store keeps the store alive until the subscription is dropped; capture a `Weak`
    /// instead.
    pub fn subscribe(
        &self,
        listener: impl Fn(&T, &Mutation::Output) + 'static,
    ) -> StoreSubscription<T, Mutation::Output> {
        let key = self
            .subscriptions
            .try_borrow_mut()
            .unwrap_or_else(|_| {
                panic!(
                    "Cannot subscribe to the `{}` store from inside one of its own notifications",
                    type_name::<T>()
                )
            })
            .insert(Box::new(listener));

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

type SubscriptionCallback<T, Output> = Box<dyn Fn(&T, &Output)>;

struct SubscriptionMap<T, Output> {
    /// Keyed by subscription order, so iterating notifies in that order
    map: BTreeMap<u64, SubscriptionCallback<T, Output>>,
    next_key: u64,
}

impl<T, Output> SubscriptionMap<T, Output> {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_key: 0,
        }
    }

    fn insert(&mut self, callback: SubscriptionCallback<T, Output>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.map.insert(key, callback);
        key
    }

    /// Removes the callback, returning it so the caller can drop it outside the borrow
    fn remove(&mut self, key: u64) -> Option<SubscriptionCallback<T, Output>> {
        self.map.remove(&key)
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
/// The subscription gets removed when it is dropped. Dropping it from inside one of the store's own
/// notifications panics, which aborts the process if that drop is part of unwinding a subscriber's
/// panic.
#[must_use = "Subscriptions are removed when dropped"]
pub struct StoreSubscription<T, Output> {
    subscriptions: Rc<RefCell<SubscriptionMap<T, Output>>>,
    key: u64,
}

impl<T, Output> Drop for StoreSubscription<T, Output> {
    fn drop(&mut self) {
        // The callback's captures may touch the store when dropped, so they must not be dropped under the borrow
        let _removed = self
            .subscriptions
            .try_borrow_mut()
            .unwrap_or_else(|_| {
                panic!(
                    "Cannot unsubscribe from the `{}` store from inside one of its own notifications",
                    type_name::<T>()
                )
            })
            .remove(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Counter = Store<u32, Add>;

    struct Add(u32);

    impl StoreMutation<u32> for Add {
        type Args = ();
        type Output = u32;
        type Error = ();

        fn apply(self, state: &mut u32, _: &()) -> Result<u32, ()> {
            *state = state.checked_add(self.0).ok_or(())?;
            Ok(*state)
        }
    }

    /// Writes once to a store whose only subscriber runs `effect` with that same store
    fn notify_into_self(effect: impl Fn(&Counter) + 'static) {
        let store = Rc::new(Counter::new(0));
        let _subscription = store.subscribe({
            let store = Rc::downgrade(&store);
            move |_, _| effect(&store.upgrade().unwrap())
        });
        let _ = store.write(Add(1), &());
    }

    type Reentrant = Store<u32, Effect>;

    /// Increments the state, then runs its function with the store it is applied to, which is
    /// passed as the args
    struct Effect(fn(&Reentrant));

    impl StoreMutation<u32> for Effect {
        type Args = Reentrant;
        type Output = u32;
        type Error = ();

        fn apply(self, state: &mut u32, store: &Reentrant) -> Result<u32, ()> {
            *state += 1;
            (self.0)(store);
            Ok(*state)
        }
    }

    #[test]
    #[should_panic(expected = "Cannot read the `u32` store from inside one of its own mutations")]
    fn read_from_own_mutation_panics() {
        let store = Reentrant::new(0);
        let _ = store.write(
            Effect(|store| {
                let _ = store.read();
            }),
            &store,
        );
    }

    #[test]
    #[should_panic(expected = "Cannot write the `u32` store while it is in use")]
    fn write_from_own_mutation_panics() {
        let store = Reentrant::new(0);
        let _ = store.write(
            Effect(|store| {
                let _ = store.write(Effect(|_| {}), store);
            }),
            &store,
        );
    }

    #[test]
    fn panicking_mutation_keeps_its_partial_change_and_store_stays_usable() {
        let store = Reentrant::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.write(Effect(|_| panic!("mutation failed")), &store);
        }));
        assert!(result.is_err());
        assert_eq!(*store.read(), 1);

        assert_eq!(store.write(Effect(|_| {}), &store), Ok(2));
    }

    #[test]
    fn failed_mutation_notifies_no_one() {
        let store = Counter::new(u32::MAX);
        let calls = Rc::new(std::cell::Cell::new(0));
        let _subscription = store.subscribe({
            let calls = calls.clone();
            move |_, _| calls.set(calls.get() + 1)
        });

        assert_eq!(store.write(Add(1), &()), Err(()));

        assert_eq!(calls.get(), 0);
        assert_eq!(*store.read(), u32::MAX);
    }

    #[test]
    #[should_panic(expected = "Cannot write the `u32` store while it is in use")]
    fn write_from_own_notification_panics() {
        notify_into_self(|store| {
            let _ = store.write(Add(1), &());
        });
    }

    #[test]
    #[should_panic(expected = "Cannot write the `u32` store while it is in use")]
    fn write_while_reading_panics() {
        let store = Counter::new(0);
        let _state = store.read();
        let _ = store.write(Add(1), &());
    }

    #[test]
    fn read_from_own_notification_sees_the_written_state() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        notify_into_self({
            let seen = seen.clone();
            move |store| seen.borrow_mut().push(*store.read())
        });

        assert_eq!(*seen.borrow(), [1]);
    }

    #[test]
    #[should_panic(
        expected = "Cannot subscribe to the `u32` store from inside one of its own notifications"
    )]
    fn subscribe_from_own_notification_panics() {
        notify_into_self(|store| {
            let _ = store.subscribe(|_, _| {});
        });
    }

    #[test]
    #[should_panic(
        expected = "Cannot unsubscribe from the `u32` store from inside one of its own notifications"
    )]
    fn unsubscribe_from_own_notification_panics() {
        let store = Counter::new(0);
        let doomed = RefCell::new(Some(store.subscribe(|_, _| {})));
        let _dropper = store.subscribe(move |_, _| drop(doomed.borrow_mut().take()));
        let _ = store.write(Add(1), &());
    }

    #[test]
    fn writing_another_store_from_a_notification_is_allowed() {
        let downstream = Rc::new(Counter::new(0));
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
    fn subscribers_are_notified_in_subscription_order() {
        let store = Counter::new(0);
        let order = Rc::new(RefCell::new(Vec::new()));
        let _subscriptions: Vec<_> = (0..16)
            .map(|index| {
                let order = order.clone();
                store.subscribe(move |_, _| order.borrow_mut().push(index))
            })
            .collect();

        store.write(Add(1), &()).unwrap();

        assert_eq!(*order.borrow(), (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn dropped_subscription_is_no_longer_notified() {
        let store = Counter::new(0);
        let calls = Rc::new(std::cell::Cell::new(0));
        let subscription = store.subscribe({
            let calls = calls.clone();
            move |_, _| calls.set(calls.get() + 1)
        });

        store.write(Add(1), &()).unwrap();
        drop(subscription);
        store.write(Add(1), &()).unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn dropping_a_subscription_that_owns_another_is_allowed() {
        let store = Counter::new(0);
        let inner = store.subscribe(|_, _| {});
        let outer = store.subscribe(move |_, _| {
            let _ = &inner;
        });

        drop(outer);
    }

    #[test]
    fn panicking_subscriber_skips_later_subscribers_and_store_stays_usable() {
        let store = Counter::new(0);
        let panicked = std::cell::Cell::new(false);
        let _panicking = store.subscribe(move |_, _| {
            if !panicked.replace(true) {
                panic!("subscriber failed");
            }
        });
        let later_calls = Rc::new(std::cell::Cell::new(0));
        let _later = store.subscribe({
            let later_calls = later_calls.clone();
            move |_, _| later_calls.set(later_calls.get() + 1)
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.write(Add(1), &());
        }));
        assert!(result.is_err());
        assert_eq!(later_calls.get(), 0);
        assert_eq!(*store.read(), 1);

        assert_eq!(store.write(Add(1), &()), Ok(2));
        assert_eq!(later_calls.get(), 1);
    }
}
