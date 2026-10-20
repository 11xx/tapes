//! Work spread over the machine's cores, with its results in the order of
//! its inputs, so what a read returns never depends on how it was scheduled.
//!
//! A map runs on its caller and recruits helpers from one pool the process
//! shares, a helper for each core but one, while its items last. A map begun
//! inside another's item therefore spreads only as helpers come free, and maps
//! nested in maps never run more threads than the machine has cores.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

/// The most threads one map runs.
pub(crate) const MAX_WORKERS: usize = 16;

fn cores() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// The helpers no map holds: one per core but the first, which every map's
/// caller stands for.
fn free_helpers() -> &'static AtomicUsize {
    static FREE: OnceLock<AtomicUsize> = OnceLock::new();
    FREE.get_or_init(|| AtomicUsize::new(cores() - 1))
}

/// A helper taken from the shared pool, returned to it when dropped.
struct Helper;

impl Helper {
    fn take() -> Option<Self> {
        free_helpers()
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |free| {
                free.checked_sub(1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        free_helpers().fetch_add(1, Ordering::AcqRel);
    }
}

/// One map's items, the next unclaimed, the results so far, and how many
/// more helpers it may recruit.
struct Shared<'a, T, R, F> {
    items: &'a [T],
    work: &'a F,
    next: AtomicUsize,
    results: Vec<Mutex<Option<R>>>,
    openings: AtomicUsize,
}

impl<T: Sync, R: Send, F: Fn(&T) -> R + Sync> Shared<'_, T, R, F> {
    /// Work items until none is unclaimed, recruiting a helper before each
    /// item that leaves another behind it.
    fn work_items<'scope>(&'scope self, scope: &'scope thread::Scope<'scope, '_>) {
        loop {
            let at = self.next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = self.items.get(at) else {
                return;
            };
            if at + 1 < self.items.len() {
                self.recruit(scope);
            }
            let result = (self.work)(item);
            *self.results[at]
                .lock()
                .expect("no worker holds a result lock") = Some(result);
        }
    }

    /// Start a helper on these items when the map has an opening and the
    /// pool a free helper. One the system cannot start leaves the items to
    /// the threads already on them.
    fn recruit<'scope>(&'scope self, scope: &'scope thread::Scope<'scope, '_>) {
        if self.openings.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(helper) = Helper::take() else {
            return;
        };
        let opened = self
            .openings
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                open.checked_sub(1)
            });
        if opened.is_err() {
            return;
        }
        let _ = thread::Builder::new().spawn_scoped(scope, move || {
            let _helper = helper;
            self.work_items(scope);
        });
    }
}

/// `work` applied to each of `items` on the caller and up to
/// [`MAX_WORKERS`] threads in all, each taking the next item as it finishes
/// one, so one slow item holds up only its own thread. The results come back
/// in the items' order.
pub(crate) fn map<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = cores().min(MAX_WORKERS).min(items.len());
    if threads <= 1 {
        return items.iter().map(work).collect();
    }
    let shared = Shared {
        items,
        work: &work,
        next: AtomicUsize::new(0),
        results: items.iter().map(|_| Mutex::new(None)).collect(),
        openings: AtomicUsize::new(threads - 1),
    };
    thread::scope(|scope| shared.work_items(scope));
    collected(shared.results)
}

/// As [`map`], on up to `workers` threads of its own outside the shared
/// pool: for work that mostly waits on other processes, which would hold
/// helpers the reads beside it need.
pub(crate) fn map_waiting<T: Sync, R: Send>(
    workers: usize,
    items: &[T],
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let workers = cores().min(workers).min(items.len());
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
    collected(results)
}

fn collected<R>(results: Vec<Mutex<Option<R>>>) -> Vec<R> {
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("no worker holds a result lock")
                .expect("every item was worked")
        })
        .collect()
}

/// As [`map`], starting the items `cost` ranks highest first, so the longest
/// work does not begin last; the results still come back in the items'
/// order.
pub(crate) fn map_costliest_first<T: Sync, R: Send, C: Ord>(
    items: &[T],
    cost: impl Fn(&T) -> C,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_cached_key(|at| std::cmp::Reverse(cost(&items[*at])));
    let mut results: Vec<Option<R>> = Vec::with_capacity(items.len());
    results.resize_with(items.len(), || None);
    for (at, result) in order.iter().zip(map(&order, |at| work(&items[*at]))) {
        results[*at] = Some(result);
    }
    results
        .into_iter()
        .map(|result| result.expect("every item was worked"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{cores, map, map_costliest_first};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

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
        let ranked = map_costliest_first(&items, |item| item % 7, |item| item + 1);
        assert_eq!(
            ranked,
            items.iter().map(|item| item + 1).collect::<Vec<_>>()
        );
    }

    /// The most threads seen at once inside `work`, counted by `inside`.
    fn counted<T>(inside: &AtomicUsize, most: &AtomicUsize, work: impl FnOnce() -> T) -> T {
        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
        most.fetch_max(now, Ordering::SeqCst);
        let result = work();
        inside.fetch_sub(1, Ordering::SeqCst);
        result
    }

    // Claim: maps nested in a map share the outer map's threads, so the
    // threads working at once never outnumber the cores. Rejects a nested
    // map that starts threads of its own beside every outer one.
    #[test]
    fn nested_maps_never_run_more_threads_than_cores() {
        let (inside, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let outer: Vec<u64> = (0..32).collect();
        let sums = map(&outer, |item| {
            let inner: Vec<u64> = (0..32).collect();
            map(&inner, |each| {
                counted(&inside, &most, || {
                    std::thread::sleep(Duration::from_millis(1));
                    item + each
                })
            })
            .iter()
            .sum::<u64>()
        });
        assert_eq!(
            sums,
            outer
                .iter()
                .map(|item| item * 32 + (0..32).sum::<u64>())
                .collect::<Vec<_>>()
        );
        assert!(
            most.load(Ordering::SeqCst) <= cores(),
            "{} threads at once on {} cores",
            most.load(Ordering::SeqCst),
            cores()
        );
    }

    // Claim: a map begun inside another map's item takes up the threads the
    // outer map's other items free, so a long item's own work spreads once
    // the rest are done. Rejects a nested map that stays on its caller.
    #[test]
    fn a_nested_map_spreads_as_threads_come_free() {
        if cores() < 2 {
            return;
        }
        let threads = Mutex::new(HashSet::new());
        let outer = [true, false];
        map(&outer, |long| {
            if !long {
                return;
            }
            // Items wait until a second thread has taken one, for up to ten
            // seconds, so helpers other tests hold have time to come free.
            let inner: Vec<u64> = (0..10_000).collect();
            map(&inner, |_| {
                let mut seen = threads.lock().expect("no item holds the lock");
                seen.insert(std::thread::current().id());
                if seen.len() < 2 {
                    drop(seen);
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        });
        let used = threads.lock().expect("no item holds the lock").len();
        assert!(used > 1, "the nested map ran on {used} thread");
    }
}
