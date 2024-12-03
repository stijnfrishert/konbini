use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, Mutex, RwLock},
};

/// A data store for some state, and subscribers to its mutations
///
/// This is a store in the sense of Redux. This struct manages state and encapsulations mutation.
/// Users can access the state immutably, but only mutate through the `write()` method.
///
/// The comparison doesn't hold entirely, we don't use immutable data structures, nor is there
/// an explicit reducer function. We make use of Rust's imperative programming model here, and
/// update the state directly in the `write()` method.
///
/// Interested parties can register subscriptions on this store, so that they'll be notified
/// when the state changes. This is useful to update e.g. your user interface or send out notifications
/// over the network for a REST API.
pub struct Store<T, Mutation>
where
    Mutation: StoreMutation<T>,
{
    /// The state that is being managed
    /// We wrap this in an `RwLock` to allow for concurrent reads and exclusive writes
    state: RwLock<T>,

    /// The subscriptions to the store
    /// These are the parties interested in hearing about changes
    /// We wrap this in a `Mutex` to allow for concurrent access
    subscriptions: Arc<Mutex<SubscriptionMap<T, Mutation::Change>>>,
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
        let change = {
            let mut state = self
                .state
                .write()
                .expect("Could not lock editor state for writing");

            mutation.mutate(&mut state, args)
        };

        // Lock the state again, immutably this time
        let state = self.read();

        // Lock the subscriptions
        let subscriptions = self
            .subscriptions
            .lock()
            .expect("Could not lock subscriptions for change distribution");

        // Then, send out the changes
        for subscription in subscriptions.map.values() {
            subscription(&state, &change);
        }
    }

    /// Register a new subscriber to the store
    ///
    /// The resulting type, [`StoreSubscription`], is a handle to the subscription.
    /// When it is dropped, the subscription is removed.
    pub fn subscribe(
        &self,
        listener: impl Fn(&T, &Mutation::Change) + Send + Sync + 'static,
    ) -> StoreSubscription<T, Mutation::Change> {
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

type SubscriptionCallback<T, Change> = Box<dyn Fn(&T, &Change) + Send + Sync>;

struct SubscriptionMap<T, Change> {
    map: HashMap<u64, SubscriptionCallback<T, Change>>,
    next_key: u64,
}

impl<T, Change> SubscriptionMap<T, Change> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_key: 0,
        }
    }

    pub fn insert(&mut self, callback: SubscriptionCallback<T, Change>) -> u64 {
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
    /// The type of change that is produced by this mutation
    type Change;

    /// Extra arguments that should be passed to the mutation
    type Args;

    /// Mutate the state, and return the change that happened
    fn mutate(&self, state: &mut T, args: &Self::Args) -> Self::Change;
}

/// An active subscription in the [`Store`]
///
/// The subscription gets removed when it is dropped.
#[must_use = "Subscriptions are removed when dropped"]
pub struct StoreSubscription<T, Change> {
    subscriptions: Arc<Mutex<SubscriptionMap<T, Change>>>,
    key: u64,
}

impl<T, Change> Drop for StoreSubscription<T, Change> {
    fn drop(&mut self) {
        self.subscriptions
            .lock()
            .expect("Could not lock subscriptions for unsubscription")
            .remove(self.key);
    }
}
