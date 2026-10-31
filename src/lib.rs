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
//! Subscribers are called synchronously, while the store holds its state for reading. A subscriber
//! must therefore not read or write the store that is notifying it, whether directly or by waiting
//! on another thread that does; either would deadlock. Debug builds panic instead when the
//! subscriber does so directly. Subscribing to and unsubscribing from it are allowed.

use std::{
    collections::BTreeMap,
    ops::Deref,
    sync::{
        Arc, Mutex, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
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

        // Declared first so it drops last, after every lock is released: it may hold the last
        // reference to a callback unsubscribed meanwhile, whose captures may touch the store
        let subscribers;

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

        // Snapshot the subscribers, so they can subscribe and unsubscribe while being notified
        subscribers = self
            .subscriptions
            .lock()
            .expect("Could not lock subscriptions for output distribution")
            .snapshot();

        // Then, send out the outputs, skipping subscriptions dropped since the snapshot
        let _notifying = notifying::enter(&self.subscriptions);
        for subscriber in &subscribers {
            if subscriber.alive.load(Ordering::Relaxed) {
                (subscriber.callback)(&state, &output);
            }
        }

        Ok(output)
    }

    /// Register a new subscriber to the store, called for every write from the next notification on
    ///
    /// The resulting type, [`StoreSubscription`], is a handle to the subscription.
    /// When it is dropped, the subscription is removed.
    ///
    /// Subscribers are notified in the order they subscribed.
    ///
    /// May be called from inside a notification, including this store's own.
    pub fn subscribe(
        &self,
        listener: impl Fn(&T, &Mutation::Output) + Send + Sync + 'static,
    ) -> StoreSubscription<T, Mutation::Output> {
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

struct Subscriber<T, Output> {
    callback: SubscriptionCallback<T, Output>,

    /// Cleared when the subscription is dropped, so notifications already holding a snapshot skip it
    ///
    /// Relaxed suffices: the flag publishes no data, and a load ordered after the store by happens-before,
    /// as on the dropping thread, sees it.
    alive: AtomicBool,
}

struct SubscriptionMap<T, Output> {
    /// Keyed by subscription order, so iterating notifies in that order
    map: BTreeMap<u64, Arc<Subscriber<T, Output>>>,
    next_key: u64,
}

impl<T, Output> SubscriptionMap<T, Output> {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_key: 0,
        }
    }

    pub fn insert(&mut self, callback: SubscriptionCallback<T, Output>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        let subscriber = Subscriber {
            callback,
            alive: AtomicBool::new(true),
        };
        self.map.insert(key, Arc::new(subscriber));

        key
    }

    /// Removes and kills the subscriber, returning it so the caller can drop it outside the lock
    pub fn remove(&mut self, key: u64) -> Option<Arc<Subscriber<T, Output>>> {
        let subscriber = self.map.remove(&key)?;
        subscriber.alive.store(false, Ordering::Relaxed);
        Some(subscriber)
    }

    pub fn snapshot(&self) -> Vec<Arc<Subscriber<T, Output>>> {
        self.map.values().cloned().collect()
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
/// The subscription gets removed when it is dropped. From then on its callback is not called by
/// notifications that start later, nor later in a notification on the thread that dropped it. A
/// notification already under way on another thread may still call it after the drop returns.
///
/// The callback and its captures are released when the last reference goes: at the drop, or, if a
/// notification already holding the callback is under way, when that notification finishes, on the
/// thread running it. A release inside a notification is subject to the same rules as a subscriber.
#[must_use = "Subscriptions are removed when dropped"]
pub struct StoreSubscription<T, Output> {
    subscriptions: Arc<Mutex<SubscriptionMap<T, Output>>>,
    key: u64,
}

impl<T, Output> Drop for StoreSubscription<T, Output> {
    fn drop(&mut self) {
        // The callback's captures may touch the store when dropped, so they must not be dropped under the lock
        let _removed = self
            .subscriptions
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

    /// A subscription another subscriber can drop from inside a notification
    type Slot = Arc<Mutex<Option<StoreSubscription<u32, u32>>>>;

    /// Writes once to a store whose only subscriber runs `effect` with that same store
    fn notify_into_self(effect: impl Fn(&Counter) + Send + Sync + 'static) {
        let store = Arc::new(Counter::new(0));
        let _subscription = store.subscribe({
            let store = Arc::downgrade(&store);
            move |_, _| effect(&store.upgrade().unwrap())
        });
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
    fn subscribing_from_own_notification_takes_effect_from_the_next_write() {
        let store = Arc::new(Counter::new(0));
        let late = Arc::new(Mutex::new(Vec::new()));
        let added = Arc::new(Mutex::new(Vec::new()));
        let _subscription = store.subscribe({
            let store = Arc::downgrade(&store);
            let late = late.clone();
            let added = added.clone();
            move |_, _| {
                let late = late.clone();
                let subscription = store
                    .upgrade()
                    .unwrap()
                    .subscribe(move |_, &value| late.lock().unwrap().push(value));
                added.lock().unwrap().push(subscription);
            }
        });

        store.write(Add(1), &()).unwrap();
        store.write(Add(1), &()).unwrap();

        // The subscriber added during write 1 sees only write 2; the one added during write 2 sees nothing yet
        assert_eq!(*late.lock().unwrap(), vec![2]);
    }

    #[test]
    fn subscribers_are_notified_in_subscription_order() {
        let store = Counter::new(0);
        let order = Arc::new(Mutex::new(Vec::new()));
        let _subscriptions: Vec<_> = (0..16)
            .map(|index| {
                let order = order.clone();
                store.subscribe(move |_, _| order.lock().unwrap().push(index))
            })
            .collect();

        store.write(Add(1), &()).unwrap();

        assert_eq!(*order.lock().unwrap(), (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn unsubscribing_from_own_notification_skips_the_rest_of_it() {
        let store = Counter::new(0);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let later: Slot = Arc::default();
        let _earlier = store.subscribe({
            let calls = calls.clone();
            let later = later.clone();
            move |_, &value| {
                calls.lock().unwrap().push(("earlier", value));
                drop(later.lock().unwrap().take());
            }
        });
        *later.lock().unwrap() = Some(store.subscribe({
            let calls = calls.clone();
            move |_, &value| calls.lock().unwrap().push(("later", value))
        }));

        store.write(Add(1), &()).unwrap();
        store.write(Add(1), &()).unwrap();

        // The earlier subscriber drops the later one before its turn in the first notification
        assert_eq!(*calls.lock().unwrap(), [("earlier", 1), ("earlier", 2)]);
    }

    #[test]
    fn captures_released_by_a_notification_may_write_the_store() {
        struct WritesOnDrop(std::sync::Weak<Counter>);

        impl Drop for WritesOnDrop {
            fn drop(&mut self) {
                if let Some(store) = self.0.upgrade() {
                    store.write(Add(10), &()).unwrap();
                }
            }
        }

        let store = Arc::new(Counter::new(0));
        let doomed: Slot = Arc::default();
        let _dropper = store.subscribe({
            let doomed = doomed.clone();
            move |_, _| drop(doomed.lock().unwrap().take())
        });
        *doomed.lock().unwrap() = Some(store.subscribe({
            let on_drop = WritesOnDrop(Arc::downgrade(&store));
            move |_, _| {
                let _ = &on_drop;
            }
        }));

        // The notification holds the last reference to the doomed callback, so releasing it writes
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn({
            let store = store.clone();
            move || {
                store.write(Add(1), &()).unwrap();
                done_tx.send(()).unwrap();
            }
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("releasing the captures deadlocked");

        assert_eq!(*store.read(), 11);
    }

    #[test]
    fn dropping_a_subscription_does_not_wait_for_another_threads_notification() {
        let store = Arc::new(Counter::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let _blocking = store.subscribe(move |_, _| {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        });
        let doomed_called = Arc::new(AtomicBool::new(false));
        let doomed = store.subscribe({
            let doomed_called = doomed_called.clone();
            move |_, _| doomed_called.store(true, Ordering::SeqCst)
        });

        let writer = std::thread::spawn({
            let store = store.clone();
            move || store.write(Add(1), &()).unwrap()
        });
        entered_rx.recv().unwrap();

        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(doomed);
            dropped_tx.send(()).unwrap();
        });
        let dropped = dropped_rx.recv_timeout(std::time::Duration::from_secs(5));

        // Release before asserting, so a failure can't leave the writer blocked in its subscriber
        release_tx.send(()).unwrap();
        dropped.expect("dropping the subscription waited for the notification");
        writer.join().unwrap();

        // The drop happens-before the release, so the notification skips the doomed subscriber
        assert!(!doomed_called.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_a_subscription_that_owns_another_does_not_deadlock() {
        let store = Arc::new(Counter::new(0));
        let inner = store.subscribe(|_, _| {});
        let outer = store.subscribe(move |_, _| {
            let _ = &inner;
        });

        // Run the drop on another thread, so a deadlock fails the test instead of hanging it
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(outer);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("dropping the outer subscription deadlocked");
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
        let store = Counter::new(0);
        let panicked = AtomicBool::new(false);
        let _subscription = store.subscribe(move |_, _| {
            if !panicked.swap(true, Ordering::SeqCst) {
                panic!("subscriber failed");
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.write(Add(1), &());
        }));
        assert!(result.is_err());

        // The panic must leave the store neither marked as notifying nor with unusable locks
        assert_eq!(store.write(Add(1), &()), Ok(2));
        notifying::assert_not::<u32>(&store.subscriptions, "read");
    }
}
