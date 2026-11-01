//! Store locks.
//!
//! - [`SlottedRwLock`]: reader-writer lock for short structural critical sections. Readers bump a
//!   cache-padded counter picked per thread, so readers on different slots never share a cache
//!   line; the rare writer raises a flag and drains every counter.
//! - [`WriterLock`]: the single-writer lock a write transaction holds for its whole life. Its
//!   guard is `Send` and waits can time out.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Reader counters. Threads map onto them round-robin; sharing a counter between threads is
/// correct (it counts), it only costs cache-line traffic between those threads.
const SLOTS: usize = 64;

/// Round-robin source for per-thread slots.
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's counter.
    static SLOT: usize = NEXT_SLOT.fetch_add(1, Ordering::Relaxed) % SLOTS;
    /// Read locks this thread holds: `(lock address, depth, slot)`. A nested read on the same
    /// thread must not wait for a writer that arrived after the outer read (the writer is
    /// waiting for that outer read, so both would spin forever); with the depth recorded here it
    /// just counts up.
    static HELD: RefCell<Vec<(usize, u32, usize)>> = const { RefCell::new(Vec::new()) };
}

/// Spin, then yield, then sleep: short waits stay on-core, long ones (a writer holding the lock
/// for a bulk load) stop burning a CPU.
struct Backoff(u32);

impl Backoff {
    fn snooze(&mut self) {
        if self.0 < 64 {
            std::hint::spin_loop();
        } else if self.0 < 256 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        self.0 = self.0.saturating_add(1);
    }
}

/// Cache-line padding against false sharing between slots.
#[repr(align(64))]
struct CachePadded<T>(T);

/// RW lock with one reader counter per slot (see the module docs).
///
/// Read locks are reentrant per thread and may be dropped in any order. A thread holding a read
/// lock must not take the write lock: the writer would wait for its own read.
pub struct SlottedRwLock {
    /// Reader counters, each on its own 64-byte line.
    slots: [CachePadded<AtomicU32>; SLOTS],
    /// Set while a structural mutation runs.
    writer: CachePadded<AtomicBool>,
}

impl SlottedRwLock {
    /// Unlocked lock.
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| CachePadded(AtomicU32::new(0))),
            writer: CachePadded(AtomicBool::new(false)),
        }
    }

    fn key(&self) -> usize {
        self as *const Self as usize
    }

    /// Shared lock.
    #[inline]
    pub fn read(&self) -> SlottedReadGuard<'_> {
        let key = self.key();
        let nested = HELD.with(|held| {
            let mut held = held.borrow_mut();
            match held.iter_mut().find(|e| e.0 == key) {
                Some(entry) => {
                    entry.1 += 1;
                    true
                }
                None => false,
            }
        });
        if !nested {
            let slot = SLOT.with(|s| *s);
            let counter = &self.slots[slot].0;
            // Dekker handshake with `write`: both sides publish (counter / flag) and then read the
            // other's, all SeqCst, so at least one of them sees the other
            loop {
                counter.fetch_add(1, Ordering::SeqCst);
                if !self.writer.0.load(Ordering::SeqCst) {
                    break;
                }
                // writer active: step aside until it's done
                counter.fetch_sub(1, Ordering::Release);
                let mut backoff = Backoff(0);
                while self.writer.0.load(Ordering::Relaxed) {
                    backoff.snooze();
                }
            }
            HELD.with(|held| held.borrow_mut().push((key, 1, slot)));
        }
        SlottedReadGuard {
            lock: self,
            _not_send: PhantomData,
        }
    }

    /// Exclusive lock; drains every reader slot.
    #[inline]
    pub fn write(&self) -> SlottedWriteGuard<'_> {
        debug_assert!(
            !HELD.with(|held| held.borrow().iter().any(|e| e.0 == self.key())),
            "write lock taken by a thread that holds a read lock on it"
        );
        let mut backoff = Backoff(0);
        while self
            .writer
            .0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            backoff.snooze();
        }
        let mut backoff = Backoff(0);
        for slot in &self.slots {
            while slot.0.load(Ordering::SeqCst) != 0 {
                backoff.snooze();
            }
        }
        SlottedWriteGuard { lock: self }
    }
}

impl Default for SlottedRwLock {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SlottedRwLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active_slots = self
            .slots
            .iter()
            .filter(|s| s.0.load(Ordering::Relaxed) > 0)
            .count();
        f.debug_struct("SlottedRwLock")
            .field("active_reader_slots", &active_slots)
            .field("writer_active", &self.writer.0.load(Ordering::Relaxed))
            .finish()
    }
}

/// RAII read guard for [`SlottedRwLock`]. Not `Send`: the thread that took it releases it.
pub struct SlottedReadGuard<'a> {
    lock: &'a SlottedRwLock,
    _not_send: PhantomData<*const ()>,
}

impl Drop for SlottedReadGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        let key = self.lock.key();
        let released = HELD.with(|held| {
            let mut held = held.borrow_mut();
            let pos = held
                .iter()
                .position(|e| e.0 == key)
                .expect("read guard without a held entry");
            held[pos].1 -= 1;
            (held[pos].1 == 0).then(|| held.swap_remove(pos).2)
        });
        // the last guard of this thread releases the counter
        if let Some(slot) = released {
            self.lock.slots[slot].0.fetch_sub(1, Ordering::Release);
        }
    }
}

/// RAII write guard for [`SlottedRwLock`].
pub struct SlottedWriteGuard<'a> {
    lock: &'a SlottedRwLock,
}

impl Drop for SlottedWriteGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.writer.0.store(false, Ordering::Release);
    }
}

/// The single-writer lock. Unlike a `std` mutex its guard is `Send`: a write transaction can live
/// in a handle that moves between threads (language bindings, the C ABI) and finish on any of
/// them. Waits can be bounded ([`try_lock_for`](Self::try_lock_for)).
///
/// Uncontended acquire and release are one atomic each. Waiters park on a condvar; a release only
/// touches it when someone waits. Not reentrant: a thread already holding it waits for itself.
pub struct WriterLock {
    held: AtomicBool,
    /// Threads parked, or about to park, in a slow-path acquire.
    waiters: AtomicUsize,
    park: Mutex<()>,
    released: Condvar,
}

impl WriterLock {
    /// Unlocked lock.
    pub const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            waiters: AtomicUsize::new(0),
            park: Mutex::new(()),
            released: Condvar::new(),
        }
    }

    /// Takes the lock if it's free.
    #[inline]
    pub fn try_lock(&self) -> Option<WriterGuard<'_>> {
        self.held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then(|| WriterGuard { lock: self })
    }

    /// Takes the lock, waiting as long as it takes.
    pub fn lock(&self) -> WriterGuard<'_> {
        self.try_lock()
            .or_else(|| self.acquire_slow(None))
            .expect("an unbounded wait only returns with the lock")
    }

    /// Takes the lock, waiting at most `timeout`; `None` if it stayed taken.
    pub fn try_lock_for(&self, timeout: Duration) -> Option<WriterGuard<'_>> {
        // a deadline past what `Instant` can hold is no deadline
        self.try_lock()
            .or_else(|| self.acquire_slow(Instant::now().checked_add(timeout)))
    }

    fn acquire_slow(&self, deadline: Option<Instant>) -> Option<WriterGuard<'_>> {
        let mut parked = self.park.lock().unwrap_or_else(PoisonError::into_inner);
        // announce first, then try: a release after this point sees us and wakes us (SeqCst on
        // both sides; see `release`)
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let got = loop {
            if self
                .held
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                break true;
            }
            parked = match deadline {
                None => self
                    .released
                    .wait(parked)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break false;
                    }
                    self.released
                        .wait_timeout(parked, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        };
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        got.then(|| WriterGuard { lock: self })
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
        // a waiter announced before our store (it'll be woken) or tries after it (it'll get in)
        if self.waiters.load(Ordering::SeqCst) > 0 {
            // under the park mutex: a waiter between its failed try and its wait holds it, so the
            // notify can't fall in that gap
            let _parked = self.park.lock().unwrap_or_else(PoisonError::into_inner);
            self.released.notify_one();
        }
    }
}

impl Default for WriterLock {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WriterLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterLock")
            .field("held", &self.held.load(Ordering::Relaxed))
            .field("waiters", &self.waiters.load(Ordering::Relaxed))
            .finish()
    }
}

/// RAII guard for [`WriterLock`]. `Send`: whichever thread drops it releases the lock.
#[must_use = "the lock is released when the guard drops"]
pub struct WriterGuard<'a> {
    lock: &'a WriterLock,
}

impl WriterGuard<'_> {
    /// Whether this guard holds `lock`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn holds(&self, lock: &WriterLock) -> bool {
        std::ptr::eq(self.lock, lock)
    }
}

impl Drop for WriterGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn held_slots(lock: &SlottedRwLock) -> u32 {
        lock.slots.iter().map(|s| s.0.load(Ordering::Relaxed)).sum()
    }

    #[test]
    fn nested_reads_hold_one_count_and_release_in_any_order() {
        let lock = SlottedRwLock::new();
        let outer = lock.read();
        let inner = lock.read();
        assert_eq!(held_slots(&lock), 1, "nested reads count once");
        // drop the outer guard first: the inner one still holds the lock
        drop(outer);
        assert_eq!(held_slots(&lock), 1);
        drop(inner);
        assert_eq!(held_slots(&lock), 0);
    }

    #[test]
    fn two_locks_on_one_thread_are_independent() {
        let a = SlottedRwLock::new();
        let b = SlottedRwLock::new();
        let ga = a.read();
        let gb = b.read();
        assert_eq!((held_slots(&a), held_slots(&b)), (1, 1));
        drop(ga);
        // b still read-locked: a's writer must not wait for it
        drop(a.write());
        drop(gb);
        assert_eq!(held_slots(&b), 0);
    }

    #[test]
    fn writer_flag_clears() {
        let lock = SlottedRwLock::new();
        {
            let _g = lock.write();
            assert!(lock.writer.0.load(Ordering::Relaxed));
        }
        assert!(!lock.writer.0.load(Ordering::Relaxed));
    }

    #[test]
    fn writer_lock_times_out_while_held_and_opens_after_release() {
        let lock = WriterLock::new();
        let g = lock.lock();
        assert!(lock.try_lock().is_none());
        let t = Instant::now();
        assert!(lock.try_lock_for(Duration::from_millis(30)).is_none());
        assert!(t.elapsed() >= Duration::from_millis(30));
        drop(g);
        assert!(lock.try_lock_for(Duration::from_millis(30)).is_some());
        assert!(
            lock.try_lock_for(Duration::MAX).is_some(),
            "no overflow on huge timeouts"
        );
    }

    #[test]
    fn writer_guard_released_on_another_thread_wakes_a_waiter() {
        let lock = Arc::new(WriterLock::new());
        let guard = lock.lock();
        let waiter = {
            let lock = Arc::clone(&lock);
            std::thread::spawn(move || drop(lock.lock()))
        };
        std::thread::sleep(Duration::from_millis(20));
        // the guard moves to a third thread and is released there
        std::thread::scope(|s| {
            s.spawn(move || drop(guard));
        });
        waiter.join().unwrap();
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn writer_lock_excludes_and_loses_no_wakeups() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 2_000;
        let lock = Arc::new(WriterLock::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let (lock, inside, total) =
                    (Arc::clone(&lock), Arc::clone(&inside), Arc::clone(&total));
                std::thread::spawn(move || {
                    for r in 0..ROUNDS {
                        // mix unbounded and bounded waits; a bounded one may give up
                        let g = if (i + r) % 3 == 0 {
                            match lock.try_lock_for(Duration::from_micros(50)) {
                                Some(g) => g,
                                None => continue,
                            }
                        } else {
                            lock.lock()
                        };
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders");
                        total.fetch_add(1, Ordering::Relaxed);
                        inside.fetch_sub(1, Ordering::SeqCst);
                        drop(g);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(total.load(Ordering::Relaxed) > 0);
        assert!(lock.try_lock().is_some());
        assert_eq!(lock.waiters.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn writer_waits_for_readers_and_readers_for_the_writer() {
        let lock = Arc::new(SlottedRwLock::new());
        let read = lock.read();
        let lock2 = Arc::clone(&lock);
        let entered = Arc::new(AtomicBool::new(false));
        let entered2 = Arc::clone(&entered);
        let writer = std::thread::spawn(move || {
            let _w = lock2.write();
            entered2.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::SeqCst),
            "writer ran under a read lock"
        );
        drop(read);
        writer.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }

    /// Many more threads than slots, nested reads, and a writer that mutates a two-word value
    /// non-atomically (with respect to readers): a reader that ever sees the words disagree ran
    /// concurrently with the writer.
    #[test]
    fn readers_never_overlap_the_writer() {
        let lock = Arc::new(SlottedRwLock::new());
        let a = Arc::new(AtomicU64::new(0));
        let b = Arc::new(AtomicU64::new(0));
        let threads = SLOTS + 16;
        let barrier = Arc::new(std::sync::Barrier::new(threads + 1));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let (lock, a, b, barrier) = (
                Arc::clone(&lock),
                Arc::clone(&a),
                Arc::clone(&b),
                Arc::clone(&barrier),
            );
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..2_000 {
                    let _outer = lock.read();
                    let _inner = (i % 3 == 0).then(|| lock.read());
                    let x = a.load(Ordering::Relaxed);
                    std::hint::spin_loop();
                    let y = b.load(Ordering::Relaxed);
                    assert_eq!(x, y, "read overlapped a write");
                }
            }));
        }
        {
            let (lock, a, b, barrier) = (
                Arc::clone(&lock),
                Arc::clone(&a),
                Arc::clone(&b),
                Arc::clone(&barrier),
            );
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..2_000 {
                    let _w = lock.write();
                    let v = a.load(Ordering::Relaxed) + 1;
                    a.store(v, Ordering::Relaxed);
                    std::hint::spin_loop();
                    b.store(v, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(a.load(Ordering::Relaxed), 2_000);
        assert_eq!(held_slots(&lock), 0);
        assert!(!lock.writer.0.load(Ordering::Relaxed));
    }
}
