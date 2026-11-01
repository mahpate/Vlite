//! The lib test binary's process-wide hooks: its only global allocator and
//! its only logger. Each exists once per binary, so tests share them here
//! rather than define their own.
//!
//! The allocator is `System`, keeping two independent counts of requested
//! heap bytes (`Layout::size`, so allocator headers and size-class rounding
//! are excluded):
//!
//! - **A measuring session** (`measuring`, one open at a time) counts the
//!   blocks that its enrolled threads (`Session::enroll`) allocate while it
//!   is open, from their allocation until any thread frees them, in
//!   process-wide live and most live bytes and blocks, so one measurement
//!   can span a thread pool. Each block keeps the session that counted it,
//!   so freeing a block the open session did not count changes nothing,
//!   and a thread enrolled in a closed session counts nothing: the
//!   process-wide garbage of crossbeam's epoch, which rayon's workers free
//!   whenever they look for work, and the workers of an earlier pool that
//!   have yet to exit, never touch a measurement. A session closes only
//!   once the counter updates in flight for it have landed, so none lands
//!   in a later one. While a block is reallocated both blocks count. What
//!   an enrolled thread allocates for itself counts, even crossbeam's
//!   bookkeeping as an idle worker looks for work, so a serial measurement
//!   enrolls only the thread it measures. The precision peak tests
//!   (`compact::catalog::tests::peak`) use these.
//! - **A thread inside `counted`** adds to its own net count, which no
//!   other thread touches, so such tests run beside any others. The filter
//!   summary accounting tests (`compact::stats::tests`) use it.
//!
//! A thread may be in both: `counted` sees all of its thread's requests,
//! a session only its own blocks.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{
    AtomicBool, AtomicIsize, AtomicUsize, Ordering::Relaxed, Ordering::SeqCst,
};
use std::sync::{Mutex, MutexGuard, Once, PoisonError};

/// Live bytes and blocks counted by the open session, and the most at any
/// point.
pub(crate) static LIVE: AtomicIsize = AtomicIsize::new(0);
pub(crate) static PEAK: AtomicIsize = AtomicIsize::new(0);
pub(crate) static BLOCKS: AtomicIsize = AtomicIsize::new(0);
pub(crate) static MOST_BLOCKS: AtomicIsize = AtomicIsize::new(0);
/// Serializes the measuring sessions, which share the counters above.
static MEASURING: Mutex<()> = Mutex::new(());
/// The open session's number. It advances as each session opens and as
/// it closes, so no thread or block of a closed session matches it again;
/// 0, before any, is no session.
static SESSION: AtomicUsize = AtomicUsize::new(0);
/// Updates of the counters in flight. A session closes only once none is,
/// so every update that began for it lands before it closes and none lands
/// after, in the counts of a later session.
static UPDATING: AtomicUsize = AtomicUsize::new(0);
/// A session is closing, waiting for the updates in flight.
static CLOSING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The session whose counters this thread's new blocks add to while it
    /// is open; 0 for none.
    static ENROLLED: Cell<usize> = const { Cell::new(0) };
    /// This thread's net bytes since `counted` began; `None` outside it.
    static NET: Cell<Option<isize>> = const { Cell::new(None) };
    /// This thread is inside `logged`.
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    /// What this thread logged inside `logged`.
    static RECORDS: RefCell<Logged> = const { RefCell::new(Vec::new()) };
}

/// An open measuring session; dropping it closes it.
pub(crate) struct Measuring {
    session: Session,
    _serial: MutexGuard<'static, ()>,
}

/// Enrolls threads in a measuring session.
#[derive(Clone, Copy)]
pub(crate) struct Session(usize);

/// Opens a measuring session once no other is open.
pub(crate) fn measuring() -> Measuring {
    let serial = MEASURING.lock().unwrap_or_else(PoisonError::into_inner);
    let session = SESSION.fetch_add(1, SeqCst) + 1;
    Measuring {
        session: Session(session),
        _serial: serial,
    }
}

impl Measuring {
    pub(crate) fn session(&self) -> Session {
        self.session
    }
}

impl Drop for Measuring {
    /// Closes the session once the updates in flight for it have landed.
    fn drop(&mut self) {
        SESSION.fetch_add(1, SeqCst);
        CLOSING.store(true, SeqCst);
        while UPDATING.load(SeqCst) != 0 {
            std::thread::yield_now();
        }
        CLOSING.store(false, SeqCst);
    }
}

impl Session {
    /// Counts the blocks this thread allocates while the session is open.
    pub(crate) fn enroll(self) {
        ENROLLED.set(self.0);
    }
}

/// The open session this thread's new blocks count for, or 0.
fn counting() -> usize {
    let session = ENROLLED.try_with(Cell::get).unwrap_or(0);
    if session != 0 && session == SESSION.load(Relaxed) {
        session
    } else {
        0
    }
}

/// A test's stop inside the allocator: the next update of the counters
/// that a thread makes, once it has decided that the update counts, waits
/// until the test releases it or a closing session waits for it, and
/// reports when it has landed.
struct Pause {
    paused: AtomicBool,
    released: AtomicBool,
    landed: AtomicBool,
}

thread_local! {
    /// The pause set on this thread, taken by its next counted update.
    static PAUSE: Cell<Option<&'static Pause>> = const { Cell::new(None) };
}

fn paused() -> Option<&'static Pause> {
    let pause = PAUSE.try_with(Cell::take).ok().flatten()?;
    pause.paused.store(true, SeqCst);
    while !pause.released.load(SeqCst) && !CLOSING.load(SeqCst) {
        std::thread::yield_now();
    }
    Some(pause)
}

/// Makes `change` to the counters if `session` is open, in flight: either
/// this sees the session closed and changes nothing, or the close sees it
/// in flight and waits for it, as both are sequentially consistent.
fn update(session: usize, change: impl FnOnce()) {
    if session == 0 || session != SESSION.load(Relaxed) {
        return;
    }
    UPDATING.fetch_add(1, SeqCst);
    if session == SESSION.load(SeqCst) {
        let pause = paused();
        change();
        if let Some(pause) = pause {
            pause.landed.store(true, SeqCst);
        }
    }
    UPDATING.fetch_sub(1, SeqCst);
}

fn added(session: usize, size: usize) {
    update(session, || {
        let live = LIVE.fetch_add(size as isize, Relaxed) + size as isize;
        PEAK.fetch_max(live, Relaxed);
        let blocks = BLOCKS.fetch_add(1, Relaxed) + 1;
        MOST_BLOCKS.fetch_max(blocks, Relaxed);
    });
    net(size as isize);
}

fn removed(session: usize, size: usize) {
    update(session, || {
        LIVE.fetch_sub(size as isize, Relaxed);
        BLOCKS.fetch_sub(1, Relaxed);
    });
    net(-(size as isize));
}

fn net(delta: isize) {
    let _ = NET.try_with(|net| {
        if let Some(bytes) = net.get() {
            net.set(Some(bytes + delta));
        }
    });
}

/// The bytes before each block: the session that counted it, 0 for none,
/// in the `usize` just before it, padded to keep the block's alignment.
fn prefix(layout: Layout) -> usize {
    layout.align().max(size_of::<usize>())
}

/// What `System` serves for a block of `size` bytes after `prefix`.
fn framed(size: usize, prefix: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(prefix)?, prefix).ok()
}

/// The session of the block at `block`.
///
/// # Safety
///
/// `block` was served by `Counting`.
unsafe fn session_of(block: *mut u8) -> *mut usize {
    // SAFETY: a `usize`-aligned `usize` of the prefix precedes `block`.
    unsafe { block.cast::<usize>().sub(1) }
}

struct Counting;

impl Counting {
    /// # Safety
    ///
    /// The caller's `alloc` contract for `layout`.
    unsafe fn allocate(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        let prefix = prefix(layout);
        let Some(framed) = framed(layout.size(), prefix) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `framed` has a nonzero size and a valid alignment.
        let base = unsafe {
            if zeroed {
                System.alloc_zeroed(framed)
            } else {
                System.alloc(framed)
            }
        };
        if base.is_null() {
            return base;
        }
        let session = counting();
        // SAFETY: the block starts `prefix` bytes into the served one, so
        // `layout.size()` bytes follow it, and is aligned to `prefix`, a
        // multiple of `layout.align()` and of `usize`'s.
        unsafe {
            let block = base.add(prefix);
            session_of(block).write(session);
            added(session, layout.size());
            block
        }
    }
}

// SAFETY: `System` serves each block `prefix` bytes into a longer one
// aligned to `prefix`, a multiple of the requested alignment, with the
// requested size after it; `dealloc` and `realloc` derive the same prefix
// and served layout from the caller's layout, so `System` gets back what it
// served. The bookkeeping reads const-initialized thread locals without
// destructors, which never allocate, updates atomics and at most yields
// the thread, which never allocates either, so it never reenters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `alloc` contract, passed on.
        unsafe { self.allocate(layout, false) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `alloc_zeroed` contract, passed on.
        unsafe { self.allocate(layout, true) }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        let prefix = prefix(layout);
        // SAFETY: `block` was served for `layout`, so `framed` accepted
        // this layout and the session precedes it.
        unsafe {
            let session = session_of(block).read();
            let framed = Layout::from_size_align_unchecked(layout.size() + prefix, prefix);
            System.dealloc(block.sub(prefix), framed);
            removed(session, layout.size());
        }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let prefix = prefix(layout);
        let Some(longer) = framed(size, prefix) else {
            return std::ptr::null_mut();
        };
        // SAFETY: as in `dealloc`, `block` and its session follow `prefix`
        // bytes of a block served for `framed`; `longer` is valid.
        unsafe {
            let earlier = session_of(block).read();
            let served = Layout::from_size_align_unchecked(layout.size() + prefix, prefix);
            let session = counting();
            // Both blocks count while the data may be copied.
            added(session, size);
            let base = System.realloc(block.sub(prefix), served, longer.size());
            if base.is_null() {
                removed(session, size);
                return base;
            }
            let moved = base.add(prefix);
            session_of(moved).write(session);
            removed(earlier, layout.size());
            moved
        }
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

    /// A session counts both blocks of a reallocation by its enrolled
    /// thread, exactly, and nothing else: not another thread's block.
    #[test]
    fn enrolled_threads_count_both_blocks_of_a_reallocation_and_nothing_else() {
        let open = measuring();
        let session = open.session();
        beside_other_thread(|hold| {
            thread::scope(|scope| {
                scope
                    .spawn(|| {
                        session.enroll();
                        let live = LIVE.load(Relaxed);
                        let outer = PEAK.swap(live, Relaxed);
                        let (bytes, net) = counted(|| {
                            hold();
                            grown(1 << 20, 3 << 20)
                        });
                        let peak = PEAK.fetch_max(outer, Relaxed) - live;
                        let retained = LIVE.load(Relaxed) - live;
                        drop(bytes);
                        assert_eq!(net, 3 << 20);
                        assert_eq!((peak, retained), (4 << 20, 3 << 20));
                    })
                    .join()
                    .unwrap();
            });
        });
    }

    /// A thread enrolled for an earlier measurement, such as a worker of
    /// its pool that has yet to exit, counts nothing in a later one: neither
    /// what it allocates nor its frees of what it counted before.
    #[test]
    fn a_thread_enrolled_for_an_earlier_measurement_counts_nothing_later() {
        const KEPT: u8 = 1;
        const ASKED: u8 = 2;
        const DONE: u8 = 3;
        let state = AtomicU8::new(0);
        let wait_for = |wanted: u8| {
            while state.load(SeqCst) != wanted {
                thread::yield_now();
            }
        };
        let earlier = measuring();
        let session = earlier.session();
        thread::scope(|scope| {
            scope.spawn(|| {
                session.enroll();
                let kept = std::hint::black_box(vec![0u8; 2_000]);
                state.store(KEPT, SeqCst);
                wait_for(ASKED);
                drop(std::hint::black_box(vec![0u8; 3_000]));
                drop(kept);
                state.store(DONE, SeqCst);
            });
            wait_for(KEPT);
            drop(earlier);
            let _later = measuring();
            let live = LIVE.load(Relaxed);
            let outer = PEAK.swap(live, Relaxed);
            state.store(ASKED, SeqCst);
            wait_for(DONE);
            let peak = PEAK.fetch_max(outer, Relaxed) - live;
            let retained = LIVE.load(Relaxed) - live;
            assert_eq!((peak, retained), (0, 0));
        });
    }

    /// `body`'s value with the most live bytes and the live bytes it left,
    /// above its start, as the open session counts them.
    fn window<T>(body: impl FnOnce() -> T) -> (T, (isize, isize)) {
        let live = LIVE.load(Relaxed);
        let outer = PEAK.swap(live, Relaxed);
        let value = body();
        let peak = PEAK.fetch_max(outer, Relaxed) - live;
        (value, (peak, LIVE.load(Relaxed) - live))
    }

    /// Releases a pause, should the test fail before it does.
    struct Release(&'static Pause);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.released.store(true, SeqCst);
        }
    }

    /// A thread enrolled in a session stops in the middle of allocating,
    /// freeing or growing a block, after deciding that its update of the
    /// counters counts and before making it, while that session closes and
    /// the next opens. The update must land in its own session, which
    /// waits for it as it closes, and not in the next, which counts
    /// exactly the block its own thread allocates.
    #[test]
    fn an_update_in_flight_as_its_session_closes_leaves_the_next_exact() {
        static PAUSES: [Pause; 3] = [const {
            Pause {
                paused: AtomicBool::new(false),
                released: AtomicBool::new(false),
                landed: AtomicBool::new(false),
            }
        }; 3];
        let steps = ["allocate", "free", "grow"];
        let seen: Vec<_> = steps
            .iter()
            .zip(&PAUSES)
            .map(|(&step, pause)| {
                let first = measuring();
                let session = first.session();
                let done = AtomicBool::new(false);
                thread::scope(|scope| {
                    let _release = Release(pause);
                    scope.spawn(|| {
                        session.enroll();
                        let block = match step {
                            "allocate" => {
                                PAUSE.set(Some(pause));
                                std::hint::black_box(vec![0u8; 3_000])
                            }
                            "free" => {
                                let block = std::hint::black_box(vec![0u8; 3_000]);
                                PAUSE.set(Some(pause));
                                drop(block);
                                Vec::new()
                            }
                            _ => {
                                let mut block = Vec::<u8>::with_capacity(1_000);
                                PAUSE.set(Some(pause));
                                block.reserve_exact(3_000);
                                assert_eq!(block.capacity(), 3_000);
                                block
                            }
                        };
                        drop(block);
                        done.store(true, SeqCst);
                    });
                    while !pause.paused.load(SeqCst) {
                        thread::yield_now();
                    }
                    drop(first);
                    let landed = pause.landed.load(SeqCst);
                    let next = measuring();
                    let session = next.session();
                    let (kept, used) = window(|| {
                        let kept = scope
                            .spawn(move || {
                                session.enroll();
                                std::hint::black_box(vec![0u8; 1_000])
                            })
                            .join()
                            .unwrap();
                        pause.released.store(true, SeqCst);
                        while !done.load(SeqCst) {
                            thread::yield_now();
                        }
                        kept
                    });
                    drop(kept);
                    (step, landed, used)
                })
            })
            .collect();
        // Each update landed as its session closed, which waited for it.
        let expected: Vec<_> = steps
            .iter()
            .map(|&step| (step, true, (1_000, 1_000)))
            .collect();
        assert_eq!(seen, expected);
    }

    /// Threads keep allocating, growing and freeing blocks while enrolled
    /// in sessions that open and close, each counting them until it closes
    /// and followed by a session that enrolls no thread, so counts nothing.
    #[test]
    fn churning_threads_of_closing_sessions_change_no_later_session() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 200;
        let current = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let seen: Vec<(usize, (isize, isize))> = thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    while !stop.load(SeqCst) {
                        Session(current.load(SeqCst)).enroll();
                        let mut block = std::hint::black_box(vec![0u8; 1_000]);
                        block.reserve_exact(3_000);
                        drop(std::hint::black_box(block));
                    }
                });
            }
            let seen = (0..ROUNDS)
                .map(|round| {
                    let churned = measuring();
                    let live = LIVE.load(Relaxed);
                    PEAK.store(live, Relaxed);
                    current.store(churned.session().0, SeqCst);
                    // Until a thread has grown a block in it: both blocks.
                    while PEAK.load(Relaxed) - live < 4_000 {
                        thread::yield_now();
                    }
                    drop(churned);
                    let _quiet = measuring();
                    let ((), used) = window(|| {
                        for _ in 0..64 {
                            thread::yield_now();
                        }
                    });
                    (round, used)
                })
                .filter(|(_, used)| *used != (0, 0))
                .collect();
            stop.store(true, SeqCst);
            seen
        });
        assert_eq!(seen, [], "rounds that counted");
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
