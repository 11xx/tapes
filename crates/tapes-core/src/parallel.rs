//! Work spread over the machine's cores, with its results in the order of
//! its inputs, so what a read returns never depends on how it was scheduled.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;

/// The most threads one map runs.
pub(crate) const MAX_WORKERS: usize = 16;

/// `work` applied to each of `items` on up to [`MAX_WORKERS`] threads, each
/// taking the next item as it finishes one, so one slow item holds up only
/// its own thread. The results come back in the items' order.
pub(crate) fn map<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    map_on(MAX_WORKERS, items, work)
}

/// As [`map`], on at most `workers` threads.
pub(crate) fn map_on<T: Sync, R: Send>(
    workers: usize,
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let workers = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(workers)
        .min(items.len());
    if workers <= 1 {
        return items.iter().map(work).collect();
    }
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(at) else {
                    break;
                };
                let result = work(item);
                *results[at].lock().expect("no worker holds a result lock") = Some(result);
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("no worker holds a result lock")
                .expect("every item was worked")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::map;

    #[test]
    fn results_keep_the_order_of_their_items() {
        let items: Vec<u64> = (0..200).collect();
        let doubled = map(&items, |item| {
            // Later items finish first, so completion order is not item order.
            std::thread::sleep(std::time::Duration::from_micros(200 - item));
            item * 2
        });
        assert_eq!(
            doubled,
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );
    }
}
