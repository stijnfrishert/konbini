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

use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, Mutex, RwLock},
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
        }
    }

    /// Access the state immutably
    pub fn read(&self) -> impl Deref<Target = T> + use<'_, T, Mutation> {
        self.state
            .read()
            .expect("Could not lock editor state for reading")
    }

    /// Mutate the state, notifying subscribers in the process
    pub fn write(&self, mutation: &Mutation, args: &Mutation::Args) {
        // First, mutate the state
        let output = {
            let mut state = self
                .state
                .write()
                .expect("Could not lock editor state for writing");

            mutation.apply(&mut state, args)
        };

        // Lock the state again, immutably this time
        let state = self.read();

        // Lock the subscriptions
        let subscriptions = self
            .subscriptions
            .lock()
            .expect("Could not lock subscriptions for output distribution");

        // Then, send out the outputs
        for subscription in subscriptions.map.values() {
            subscription(&state, &output);
        }
    }

    /// Register a new subscriber to the store
    ///
    /// The resulting type, [`StoreSubscription`], is a handle to the subscription.
    /// When it is dropped, the subscription is removed.
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

    /// Mutate the state
    fn apply(&self, state: &mut T, args: &Self::Args) -> Self::Output;
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
        self.subscriptions
            .lock()
            .expect("Could not lock subscriptions for unsubscription")
            .remove(self.key);
    }
}
