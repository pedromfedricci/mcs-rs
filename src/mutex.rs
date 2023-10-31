use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{fence, AtomicBool, AtomicPtr, Ordering};

use crate::pause::pause;

pub struct Slot {
    next: AtomicPtr<AtomicBool>,
}

impl Slot {
    pub const fn new() -> Slot {
        Slot { next: AtomicPtr::new(ptr::null_mut()) }
    }
}

/// A mutual exclusion primitive useful for protecting shared data
///
/// This mutex will block threads waiting for the lock to become available. The
/// mutex can also be statically initialized or created via a `new`
/// constructor. Each mutex has a type parameter which represents the data that
/// it is protecting. The data can only be accessed through the RAII guards
/// returned from `lock` and `try_lock`, which guarantees that the data is only
/// ever accessed when the mutex is locked.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use std::thread;
/// use std::sync::mpsc::channel;
/// use mcs::{Mutex, Slot};
///
/// const N: usize = 10;
///
/// // Spawn a few threads to increment a shared variable (non-atomically), and
/// // let the main thread know once all increments are done.
/// //
/// // Here we're using an Arc to share memory among threads, and the data inside
/// // the Arc is protected with a mutex.
/// let data = Arc::new(Mutex::new(0));
///
/// let (tx, rx) = channel();
/// for _ in 0..N {
///     let (data, tx) = (data.clone(), tx.clone());
///     thread::spawn(move || {
///         let mut slot = Slot::new();
///
///         // The shared state can only be accessed once the lock is held.
///         // Our non-atomic increment is safe because we're the only thread
///         // which can access the shared state when the lock is held.
///         //
///         // We unwrap() the return value to assert that we are not expecting
///         // threads to ever fail while holding the lock.
///         let mut data = data.lock(&mut slot);
///         *data += 1;
///         if *data == N {
///             tx.send(()).unwrap();
///         }
///         // the lock is unlocked here when `data` goes out of scope.
///     });
/// }
///
/// rx.recv().unwrap();
/// ```
pub struct Mutex<T: ?Sized> {
    queue: AtomicPtr<Slot>,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new mutex in an unlocked state ready for use.
    #[inline(always)]
    pub const fn new(value: T) -> Mutex<T> {
        let queue = AtomicPtr::new(ptr::null_mut());
        let data = UnsafeCell::new(value);
        Mutex { queue, data }
    }

    /// Consumes this mutex, returning the underlying data.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Attempts to acquire this lock.
    ///
    /// If the lock could not be acquired at this time, then `Err` is returned.
    /// Otherwise, an RAII guard is returned. The lock will be unlocked when the
    /// guard is dropped.
    ///
    /// This function does not block.
    #[inline(always)]
    pub fn try_lock<'a>(&'a self, slot: &'a mut Slot) -> Option<MutexGuard<'a, T>> {
        slot.next = AtomicPtr::new(ptr::null_mut());

        self.queue
            .compare_exchange(ptr::null_mut(), slot, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| MutexGuard { lock: self, slot })
            .ok()
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// This function will block the local thread until it is available to acquire
    /// the mutex. Upon returning, the thread is the only thread with the mutex
    /// held. An RAII guard is returned to allow scoped unlock of the lock. When
    /// the guard goes out of scope, the mutex will be unlocked.
    #[inline(always)]
    pub fn lock<'a>(&'a self, slot: &'a mut Slot) -> MutexGuard<'a, T> {
        slot.next = AtomicPtr::new(ptr::null_mut());
        let pred = self.queue.swap(slot, Ordering::AcqRel);

        if !pred.is_null() {
            let pred = unsafe { &*pred };
            let locked = AtomicBool::new(true);
            pred.next.store(&locked as *const _ as *mut _, Ordering::Release);
            while locked.load(Ordering::Relaxed) {
                pause();
            }
            fence(Ordering::Acquire);
        }

        MutexGuard { lock: self, slot }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `Mutex` mutably, no actual locking needs to
    /// take place---the mutable borrow statically guarantees no locks exist.
    #[inline(always)]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

impl<T: ?Sized + Default> Default for Mutex<T> {
    /// Creates a `Mutex<T>`, with the `Default` value for T.
    fn default() -> Mutex<T> {
        Mutex::new(Default::default())
    }
}

impl<T> From<T> for Mutex<T> {
    /// Creates a `Mutex<T>` from a instance of `T`.
    fn from(data: T) -> Self {
        Self::new(data)
    }
}

/// An RAII implementation of a "scoped lock" of a mutex. When this structure is
/// dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be access through this guard via its
/// `Deref` and `DerefMut` implementations.
#[must_use]
pub struct MutexGuard<'a, T: ?Sized + 'a> {
    lock: &'a Mutex<T>,
    slot: &'a Slot,
}

impl<'a, T: ?Sized> Deref for MutexGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T: ?Sized> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

/// `MutexGuard` unified `drop` implementation, used for both
/// stable and unstable implementations.
///
/// `MutexGuard` does not own any `T` (so it will not drop any `T`) and it also
/// does not access anything that could be behind `T` (it does not access
/// self.data at all here) during the drop call. So it is safe for `T` to be
/// dangling by the time a instance of `MutexGuard` is dropped.
macro_rules! guard_drop_impl {
    () => {
        fn drop(&mut self) {
            let mut succ = self.slot.next.load(Ordering::Relaxed);
            if succ.is_null() {
                // No one has registered as waiting.
                if self
                    .lock
                    .queue
                    .compare_exchange(
                        self.slot as *const _ as *mut _,
                        ptr::null_mut(),
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // No one was waiting.
                    return;
                }

                // Some thread is waiting, but hasn't registered yet,
                // so spin waiting for them to register themselves.
                loop {
                    succ = self.slot.next.load(Ordering::Relaxed);
                    if !succ.is_null() {
                        break;
                    }
                    pause();
                }
            }

            // Announce to the next waiter that the lock is free.
            fence(Ordering::Acquire);
            let succ = unsafe { &*succ };
            succ.store(false, Ordering::Release);
        }
    };
}

#[cfg(feature = "unstable")]
unsafe impl<'a, #[may_dangle] T: ?Sized> Drop for MutexGuard<'a, T> {
    guard_drop_impl!();
}

#[cfg(not(feature = "unstable"))]
impl<'a, T: ?Sized> Drop for MutexGuard<'a, T> {
    guard_drop_impl!();
}

#[cfg(test)]
mod test {
    use super::{Mutex, Slot};

    // Mostly stoled from the Rust standard Mutex implementation's tests, so

    // Copyright 2014 The Rust Project Developers. See the COPYRIGHT
    // file at http://rust-lang.org/COPYRIGHT.
    //
    // Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
    // http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
    // <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
    // option. This file may not be copied, modified, or distributed
    // except according to those terms.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::thread;

    #[derive(Eq, PartialEq, Debug)]
    struct NonCopy(i32);

    #[test]
    fn smoke() {
        let mut slot = Slot::new();
        let m = Mutex::new(());
        drop(m.lock(&mut slot));
        drop(m.lock(&mut slot));
    }

    #[test]
    fn lots_and_lots() {
        lazy_static! {
            static ref LOCK: Mutex<u32> = Mutex::new(0);
        }

        const ITERS: u32 = 1000;
        const CONCURRENCY: u32 = 3;

        fn inc() {
            let mut slot = Slot::new();
            for _ in 0..ITERS {
                let mut g = LOCK.lock(&mut slot);
                *g += 1;
            }
        }

        let (tx, rx) = channel();
        for _ in 0..CONCURRENCY {
            let tx2 = tx.clone();
            thread::spawn(move || {
                inc();
                tx2.send(()).unwrap();
            });
            let tx2 = tx.clone();
            thread::spawn(move || {
                inc();
                tx2.send(()).unwrap();
            });
        }

        drop(tx);
        for _ in 0..2 * CONCURRENCY {
            rx.recv().unwrap();
        }
        let mut slot = Slot::new();
        assert_eq!(*LOCK.lock(&mut slot), ITERS * CONCURRENCY * 2);
    }

    #[test]
    fn try_lock() {
        let mut slot = Slot::new();
        let m = Mutex::new(());
        *m.try_lock(&mut slot).unwrap() = ();
    }

    #[test]
    fn test_into_inner() {
        let m = Mutex::new(NonCopy(10));
        assert_eq!(m.into_inner(), NonCopy(10));
    }

    #[test]
    fn test_into_inner_drop() {
        struct Foo(Arc<AtomicUsize>);
        impl Drop for Foo {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let num_drops = Arc::new(AtomicUsize::new(0));
        let m = Mutex::new(Foo(num_drops.clone()));
        assert_eq!(num_drops.load(Ordering::SeqCst), 0);
        {
            let _inner = m.into_inner();
            assert_eq!(num_drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(num_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_get_mut() {
        let mut m = Mutex::new(NonCopy(10));
        *m.get_mut() = NonCopy(20);
        assert_eq!(m.into_inner(), NonCopy(20));
    }

    #[test]
    fn test_lock_arc_nested() {
        // Tests nested locks and access
        // to underlying data.
        let arc = Arc::new(Mutex::new(1));
        let arc2 = Arc::new(Mutex::new(arc));
        let (tx, rx) = channel();
        let _t = thread::spawn(move || {
            let mut slot1 = Slot::new();
            let mut slot2 = Slot::new();

            let lock = arc2.lock(&mut slot1);
            let lock2 = lock.lock(&mut slot2);
            assert_eq!(*lock2, 1);
            tx.send(()).unwrap();
        });
        rx.recv().unwrap();
    }

    #[test]
    fn test_lock_arc_access_in_unwind() {
        let arc = Arc::new(Mutex::new(1));
        let arc2 = arc.clone();
        let _ = thread::spawn(move || -> () {
            struct Unwinder {
                i: Arc<Mutex<i32>>,
            }
            impl Drop for Unwinder {
                fn drop(&mut self) {
                    let mut slot = Slot::new();
                    *self.i.lock(&mut slot) += 1;
                }
            }
            let _u = Unwinder { i: arc2 };
            panic!();
        })
        .join();
        let mut slot = Slot::new();
        let lock = arc.lock(&mut slot);
        assert_eq!(*lock, 2);
    }

    #[test]
    fn test_lock_unsized() {
        let mut slot = Slot::new();
        let lock: &Mutex<[i32]> = &Mutex::new([1, 2, 3]);
        {
            let b = &mut *lock.lock(&mut slot);
            b[0] = 4;
            b[2] = 5;
        }
        let comp: &[i32] = &[4, 2, 5];
        assert_eq!(&*lock.lock(&mut slot), comp);
    }
}
