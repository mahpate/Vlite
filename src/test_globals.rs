//! The lib test binary's process-wide hooks: its only global allocator and
//! its only logger. Each exists once per binary, so tests share them here
//! rather than define their own.
//!
//! The allocator is `System`, keeping two independent counts of requested
//! heap bytes (`Layout::size`, so allocator headers and size-class rounding
//! are excluded):
//!
//! - **Enrolled threads** (`ENROLLED`) add to process-wide live and most
//!   live bytes and blocks, so one measurement can span a thread pool; the
//!   tests reading them hold `MEASURING`, one at a time. While a block is
//!   reallocated both blocks count. The precision peak tests
//!   (`compact::catalog::tests::peak`) use these.
//! - **A thread inside `counted`** adds to its own net count, which no
//!   other thread touches, so such tests run beside any others. The filter
//!   summary accounting tests (`compact::stats::tests`) use it.
//!
//! A thread may be in both; each count sees all of that thread's requests.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::{Mutex, Once};

/// Live bytes and blocks of enrolled threads, and the most at any point.
pub(crate) static LIVE: AtomicIsize = AtomicIsize::new(0);
pub(crate) static PEAK: AtomicIsize = AtomicIsize::new(0);
pub(crate) static BLOCKS: AtomicIsize = AtomicIsize::new(0);
pub(crate) static MOST_BLOCKS: AtomicIsize = AtomicIsize::new(0);
/// Serializes the measuring tests, which share the enrolled counters.
pub(crate) static MEASURING: Mutex<()> = Mutex::new(());

thread_local! {
    /// This thread adds to `LIVE`, `PEAK`, `BLOCKS` and `MOST_BLOCKS`.
    pub(crate) static ENROLLED: Cell<bool> = const { Cell::new(false) };
    /// This thread's net bytes since `counted` began; `None` outside it.
    static NET: Cell<Option<isize>> = const { Cell::new(None) };
    /// This thread is inside `logged`.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    /// What this thread logged inside `logged`.
    static RECORDS: RefCell<Logged> = const { RefCell::new(Vec::new()) };
}

fn added(size: usize) {
    if ENROLLED.try_with(Cell::get).unwrap_or(false) {
        let live = LIVE.fetch_add(size as isize, Relaxed) + size as isize;
        PEAK.fetch_max(live, Relaxed);
        let blocks = BLOCKS.fetch_add(1, Relaxed) + 1;
        MOST_BLOCKS.fetch_max(blocks, Relaxed);
    }
    net(size as isize);
}

fn removed(size: usize) {
    if ENROLLED.try_with(Cell::get).unwrap_or(false) {
        LIVE.fetch_sub(size as isize, Relaxed);
        BLOCKS.fetch_sub(1, Relaxed);
    }
    net(-(size as isize));
}

fn net(delta: isize) {
    let _ = NET.try_with(|net| {
        if let Some(bytes) = net.get() {
            net.set(Some(bytes + delta));
        }
    });
}

struct Counting;

// SAFETY: every request goes unchanged to `System`, so `Counting` upholds
// the `GlobalAlloc` contract exactly as `System` does. The bookkeeping
// reads const-initialized thread locals without destructors, which never
// allocate, and updates atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `alloc` contract, passed on unchanged.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            added(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `alloc_zeroed` contract, passed on unchanged.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            added(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: `block` was allocated by `System` with `layout`.
        unsafe { System.dealloc(block, layout) };
        removed(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // Both blocks count while the data may be copied.
        added(size);
        // SAFETY: `block` was allocated by `System` with `layout`, and the
        // caller's `realloc` contract holds for `size`.
        let moved = unsafe { System.realloc(block, layout, size) };
        removed(if moved.is_null() { size } else { layout.size() });
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// `body`'s value and the net heap bytes it left allocated on this thread;
/// `live` reads the running count inside `body`. Counts do not nest.
pub(crate) fn counted<T>(body: impl FnOnce() -> T) -> (T, usize) {
    NET.with(|net| assert_eq!(net.replace(Some(0)), None, "nested count"));
    let value = body();
    let bytes = NET.with(Cell::take).expect("counting");
    let bytes = usize::try_from(bytes).expect("freed more than counted");
    (value, bytes)
}

/// The net heap bytes this thread has allocated so far inside `counted`.
pub(crate) fn live() -> usize {
    usize::try_from(NET.with(Cell::get).expect("counting")).unwrap()
}

/// The records `logged` keeps: level, target and message.
pub(crate) type Logged = Vec<(Level, String, String)>;

/// Keeps the `vlite` records of threads inside `logged` only, so tests
/// running in parallel never see each other's records. On other threads
/// it is disabled and touches nothing, so it never allocates there.
struct Capture;

impl Log for Capture {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.target().starts_with("vlite") && CAPTURING.try_with(Cell::get).unwrap_or(false)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let line = (
                record.level(),
                record.target().to_owned(),
                record.args().to_string(),
            );
            RECORDS.with(|records| records.borrow_mut().push(line));
        }
    }

    fn flush(&self) {}
}

/// `body`'s value and the `vlite` records it logged on this thread. The
/// logger is installed on first use, for the rest of the binary.
pub(crate) fn logged<T>(body: impl FnOnce() -> T) -> (T, Logged) {
    static INSTALL: Once = Once::new();
    static LOGGER: Capture = Capture;
    INSTALL.call_once(|| {
        log::set_logger(&LOGGER).expect("the only logger in the lib test binary");
        log::set_max_level(LevelFilter::Trace);
    });
    assert!(!CAPTURING.replace(true), "nested capture");
    let value = body();
    CAPTURING.set(false);
    (value, RECORDS.take())
}

mod tests {
    use super::*;
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicU8, Ordering::SeqCst};
    use std::thread;

    /// `body` with `hold`, which makes another thread, neither enrolled nor
    /// counted, allocate 1 MiB and keep it until `body` returns or panics.
    fn beside_other_thread<T>(body: impl FnOnce(&(dyn Fn() + Sync)) -> T) -> T {
        const ASKED: u8 = 1;
        const HELD: u8 = 2;
        const DONE: u8 = 3;
        struct Done<'a>(&'a AtomicU8);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(DONE, SeqCst);
            }
        }
        let state = AtomicU8::new(0);
        let wait_past = |seen: u8| {
            while state.load(SeqCst) == seen {
                thread::yield_now();
            }
        };
        thread::scope(|scope| {
            scope.spawn(|| {
                wait_past(0);
                if state.load(SeqCst) == ASKED {
                    let block = std::hint::black_box(vec![0u8; 1 << 20]);
                    state.store(HELD, SeqCst);
                    wait_past(HELD);
                    drop(block);
                }
            });
            let _done = Done(&state);
            body(&|| {
                state.store(ASKED, SeqCst);
                wait_past(ASKED);
            })
        })
    }

    /// Grows a `Vec` from `from` to `to` bytes, reallocating it once.
    fn grown(from: usize, to: usize) -> Vec<u8> {
        let mut bytes = Vec::<u8>::with_capacity(from);
        bytes.reserve_exact(to);
        assert_eq!(bytes.capacity(), to);
        bytes
    }

    #[test]
    fn counted_sees_only_its_own_thread() {
        beside_other_thread(|hold| {
            let (bytes, retained) = counted(|| {
                hold();
                let bytes = grown(1_000, 3_000);
                assert_eq!(live(), 3_000);
                bytes
            });
            assert_eq!((bytes.capacity(), retained), (3_000, 3_000));
        });
        let ((), retained) = counted(|| drop(grown(10, 20)));
        assert_eq!(retained, 0);
    }

    /// Enrolled counts include every enrolled thread, and the threads of a
    /// pool that just finished may still allocate or free a little as they
    /// exit; that stays well under the 1 MiB that both blocks of the
    /// reallocation, or the other thread's block, would add.
    #[test]
    fn enrolled_threads_count_both_blocks_of_a_reallocation_and_nothing_else() {
        const SLACK: isize = 64 << 10;
        let _serial = MEASURING.lock().unwrap_or_else(PoisonError::into_inner);
        beside_other_thread(|hold| {
            thread::scope(|scope| {
                scope
                    .spawn(|| {
                        ENROLLED.set(true);
                        let live = LIVE.load(Relaxed);
                        let outer = PEAK.swap(live, Relaxed);
                        let (bytes, net) = counted(|| {
                            hold();
                            grown(1 << 20, 3 << 20)
                        });
                        let peak = PEAK.fetch_max(outer, Relaxed) - live;
                        let retained = LIVE.load(Relaxed) - live;
                        drop(bytes);
                        ENROLLED.set(false);
                        assert_eq!(net, 3 << 20);
                        assert!((retained - (3 << 20)).abs() < SLACK, "{retained}");
                        assert!((peak - (4 << 20)).abs() < SLACK, "{peak}");
                    })
                    .join()
                    .unwrap();
            });
        });
    }
    #[test]
    fn logged_keeps_its_own_threads_vlite_records() {
        let ((), records) = logged(|| {
            log::warn!(target: "vlite::test", "kept");
            log::warn!(target: "other", "not vlite");
            thread::scope(|scope| {
                scope.spawn(|| log::warn!(target: "vlite::test", "other thread"));
            });
        });
        let kept = (Level::Warn, "vlite::test".to_owned(), "kept".to_owned());
        assert_eq!(records, vec![kept]);
        let ((), records) = logged(|| ());
        assert!(records.is_empty());
        // Outside `logged` the installed logger allocates nothing, so it
        // never disturbs a count.
        let ((), bytes) = counted(|| log::warn!(target: "vlite::test", "elsewhere"));
        assert_eq!(bytes, 0);
    }
}
