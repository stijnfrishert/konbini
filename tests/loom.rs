//! Exhaustive interleaving tests, run with `RUSTFLAGS="--cfg loom" cargo test --test loom`

#![cfg(loom)]

use konbini::{Store, StoreMutation};
use loom::{
    sync::{Arc, Mutex},
    thread,
};

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

#[test]
fn notifications_match_state_and_arrive_in_write_order() {
    loom::model(|| {
        let store = Arc::new(Counter::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _subscription = store.subscribe({
            let seen = seen.clone();
            move |state, output| seen.lock().unwrap().push((*state, *output))
        });

        let writer = {
            let store = store.clone();
            thread::spawn(move || store.write(Add(1), &()).unwrap())
        };
        store.write(Add(2), &()).unwrap();
        writer.join().unwrap();

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().all(|(state, output)| state == output),
            "{seen:?}"
        );
        assert!(seen.is_sorted(), "{seen:?}");
    });
}

#[test]
fn captures_released_on_either_thread_may_write_the_store() {
    struct WritesOnDrop(Arc<Counter>);

    impl Drop for WritesOnDrop {
        fn drop(&mut self) {
            self.0.write(Add(10), &()).unwrap();
        }
    }

    loom::model(|| {
        let store = Arc::new(Counter::new(0));
        let on_drop = WritesOnDrop(store.clone());
        let subscription = store.subscribe(move |_, _| {
            let _keep = &on_drop;
        });

        let writer = {
            let store = store.clone();
            thread::spawn(move || store.write(Add(1), &()).unwrap())
        };
        drop(subscription);
        writer.join().unwrap();

        assert_eq!(*store.read(), 11);
    });
}
