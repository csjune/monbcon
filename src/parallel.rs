//! Running blocking hardware calls for several monitors at once.

use std::panic;
use std::thread;

/// Applies `f` to every item and returns the results in order. With more
/// than one item, all but the first run on their own threads, so the total
/// time is that of the slowest item rather than the sum.
pub(crate) fn map_parallel<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 {
        return items.into_iter().map(f).collect();
    }

    thread::scope(|scope| {
        let f = &f;
        let mut items = items.into_iter();
        let first = items.next();
        let handles = items
            .map(|item| scope.spawn(move || f(item)))
            .collect::<Vec<_>>();

        let mut results = Vec::with_capacity(handles.len() + 1);
        results.extend(first.map(f));
        results.extend(handles.into_iter().map(|handle| {
            handle
                .join()
                .unwrap_or_else(|payload| panic::resume_unwind(payload))
        }));
        results
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::map_parallel;

    #[test]
    fn results_keep_the_input_order() {
        assert_eq!(
            map_parallel(vec![3, 1, 2], |value| value * 10),
            [30, 10, 20]
        );
        assert_eq!(map_parallel(Vec::<i32>::new(), |value| value), []);
    }

    #[test]
    fn items_run_concurrently() {
        // Every item waits for all the others, so this only finishes if they
        // all run at the same time.
        let barrier = Barrier::new(3);
        let results = map_parallel(vec![1, 2, 3], |value| {
            barrier.wait();
            value
        });
        assert_eq!(results, [1, 2, 3]);
    }
}
