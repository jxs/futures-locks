// vim: tw=80

use std::future::Future;
use std::task::{Context, Poll};
use std::pin::Pin;
use futures::channel::oneshot;
use std::cell::UnsafeCell;
use std::clone::Clone;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync;
use super::FutState;
#[cfg(feature = "tokio")] use tokio_executor::{self, current_thread, Executor, SpawnError};
#[cfg(feature = "tokio")] use futures::{future, FutureExt};

/// An RAII mutex guard, much like `std::sync::MutexGuard`.  The wrapped data
/// can be accessed via its `Deref` and `DerefMut` implementations.
pub struct MutexGuard<T: ?Sized> {
    mutex: Mutex<T>
}

impl<T: ?Sized> Drop for MutexGuard<T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: ?Sized> Deref for MutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe {&*self.mutex.inner.data.get()}
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe {&mut *self.mutex.inner.data.get()}
    }
}

/// A `Future` representing a pending `Mutex` acquisition.
pub struct MutexFut<T: ?Sized> {
    state: FutState,
    mutex: Mutex<T>,
}

impl<T: ?Sized> MutexFut<T> {
    fn new(state: FutState, mutex: Mutex<T>) -> Self {
        MutexFut{state, mutex}
    }
}

impl<T: ?Sized> Drop for MutexFut<T> {
    fn drop(&mut self) {
        match &mut self.state {
            &mut FutState::New => {
                // Mutex hasn't yet been modified; nothing to do
            },
            &mut FutState::Pending(ref mut rx) => {
                rx.close();
                match rx.try_recv() {
                    Ok(Some(())) => {
                        // This future received ownership of the mutex, but got
                        // dropped before it was ever polled.  Release the
                        // mutex.
                        self.mutex.unlock()
                    },
                    Ok(None) => {
                        // Dropping the Future before it acquires the Mutex is
                        // equivalent to cancelling it.
                    },
                    Err(oneshot::Canceled) => {
                        // Never received ownership of the mutex
                    }
                }
            },
            &mut FutState::Acquired => {
                // The MutexGuard will take care of releasing the Mutex
            }
        }
    }
}

impl<T: ?Sized> Future for MutexFut<T> {
    type Output = MutexGuard<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let (result, new_state) = match &mut self.state {
            &mut FutState::New => {
                let mut mtx_data = self.mutex.inner.mutex.lock()
                    .expect("sync::Mutex::lock");
                if mtx_data.owned {
                    let (tx, mut rx) = oneshot::channel::<()>();
                    mtx_data.waiters.push_back(tx);
                    // Even though we know it isn't ready, we need to poll the
                    // receiver in order to register our task for notification.
                    assert!(Pin::new(&mut rx).poll(cx).is_pending());
                    (Poll::Pending, FutState::Pending(rx))
                } else {
                    mtx_data.owned = true;
                    let guard = MutexGuard{mutex: self.mutex.clone()};
                    (Poll::Ready(guard), FutState::Acquired)
                }
            },
            &mut FutState::Pending(ref mut rx) => {
                match Pin::new(rx).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    // It's impossible for receiver.poll() to return an error.
                    // The only way that would happen is if the sender got
                    // dropped.  But that can't happen because the Mutex owns
                    // the sender, and the Fut retains a clone of the Mutex
                    Poll::Ready(_) => {
                        let state = FutState::Acquired;
                        let result = Poll::Ready(
                                MutexGuard{mutex: self.mutex.clone()});
                        (result, state)
                    }  //LCOV_EXCL_LINE    kcov false negative
                }
            },
            &mut FutState::Acquired => panic!("Double-poll of ready Future")
        };
        self.state = new_state;
        result
    }
}

#[derive(Debug, Default)]
struct MutexData {
    owned: bool,
    // FIFO queue of waiting tasks.
    waiters: VecDeque<oneshot::Sender<()>>,
}

#[derive(Debug, Default)]
struct Inner<T: ?Sized> {
    mutex: sync::Mutex<MutexData>,
    data: UnsafeCell<T>,
}

/// `MutexWeak` is a non-owning reference to a [`Mutex`].  `MutexWeak` is to
/// [`Mutex`] as [`std::sync::Weak`] is to [`std::sync::Arc`].
///
/// # Examples
/// ```
/// # use futures_locks::{Mutex,MutexGuard};
/// # fn main() {
/// let mutex = Mutex::<u32>::new(0);
/// let mutex_weak = Mutex::downgrade(&mutex);
/// let mutex_new = mutex_weak.upgrade().unwrap();
/// # }
/// ```
///
/// [`Mutex`]: struct.Mutex.html
/// [`std::sync::Weak`]: https://doc.rust-lang.org/std/sync/struct.Weak.html
/// [`std::sync::Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
#[derive(Debug)]
pub struct MutexWeak<T: ?Sized> {
    inner: sync::Weak<Inner<T>>,
}

impl<T: ?Sized> MutexWeak<T> {
    /// Tries to upgrade the `MutexWeak` to `Mutex`. If the `Mutex` was dropped
    /// then the function return `None`.
    pub fn upgrade(&self) -> Option<Mutex<T>> {
        if let Some(inner) = self.inner.upgrade() {
            return Some(Mutex{inner})
        }
        None
    }
}

impl<T: ?Sized> Clone for MutexWeak<T> {
    fn clone(&self) -> MutexWeak<T> {
        MutexWeak {inner: self.inner.clone()}
    }
}

unsafe impl<T: ?Sized + Send> Send for MutexWeak<T> {}
unsafe impl<T: ?Sized + Send> Sync for MutexWeak<T> {}

/// A Futures-aware Mutex.
///
/// `std::sync::Mutex` cannot be used in an asynchronous environment like Tokio,
/// because a mutex acquisition can block an entire reactor.  This class can be
/// used instead.  It functions much like `std::sync::Mutex`.  Unlike that
/// class, it also has a builtin `Arc`, making it accessible from multiple
/// threads.  It's also safe to `clone`.  Also unlike `std::sync::Mutex`, this
/// class does not detect lock poisoning.
///
/// # Examples
///
/// ```
/// # extern crate futures;
/// # extern crate futures_locks;
/// # use futures_locks::*;
/// # use futures::executor::ThreadPool;
/// # use futures::task::SpawnExt;
/// # use futures::FutureExt;
/// # fn main() {
/// let mut rt = ThreadPool::new().unwrap();
///
/// let mtx = Mutex::<u32>::new(0);
/// let fut = mtx.lock().map(|mut guard| { *guard += 5; });
/// rt.run(fut);
/// assert_eq!(mtx.try_unwrap().unwrap(), 5);
/// # }
/// ```
#[derive(Debug, Default)]
pub struct Mutex<T: ?Sized> {
    inner: sync::Arc<Inner<T>>,
}

impl<T: ?Sized> Clone for Mutex<T> {
    fn clone(&self) -> Mutex<T> {
        Mutex { inner: self.inner.clone()}
    }
}

impl<T> Mutex<T> {
    /// Create a new `Mutex` in the unlocked state.
    pub fn new(t: T) -> Mutex<T> {
        let mutex_data = MutexData {
            owned: false,
            waiters: VecDeque::new(),
        };
        let inner = Inner {
            mutex: sync::Mutex::new(mutex_data),
            data: UnsafeCell::new(t)
        };  //LCOV_EXCL_LINE    kcov false negative
        Mutex { inner: sync::Arc::new(inner)}
    }

    /// Consumes the `Mutex` and returns the wrapped data.  If the `Mutex` still
    /// has multiple references (not necessarily locked), returns a copy of
    /// `self` instead.
    pub fn try_unwrap(self) -> Result<T, Mutex<T>> {
        match sync::Arc::try_unwrap(self.inner) {
            Ok(inner) => Ok({
                // `unsafe` is no longer needed as of somewhere around 1.25.0.
                // https://github.com/rust-lang/rust/issues/35067
                #[allow(unused_unsafe)]
                unsafe { inner.data.into_inner() }
            }),
            Err(arc) => Err(Mutex {inner: arc})
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Create a [`MutexWeak`] reference to this `Mutex`.
    ///
    /// [`MutexWeak`]: struct.MutexWeak.html
    pub fn downgrade(this: &Mutex<T>) -> MutexWeak<T> {
        MutexWeak {inner: sync::Arc::<Inner<T>>::downgrade(&this.inner)}
    }

    /// Returns a reference to the underlying data, if there are no other
    /// clones of the `Mutex`.
    ///
    /// Since this call borrows the `Mutex` mutably, no actual locking takes
    /// place -- the mutable borrow statically guarantees no locks exist.
    /// However, if the `Mutex` has already been cloned, then `None` will be
    /// returned instead.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate futures_locks;
    /// # use futures_locks::*;
    /// # fn main() {
    /// let mut mtx = Mutex::<u32>::new(0);
    /// *mtx.get_mut().unwrap() += 5;
    /// assert_eq!(mtx.try_unwrap().unwrap(), 5);
    /// # }
    /// ```
    pub fn get_mut(&mut self) -> Option<&mut T> {
        if let Some(inner) = sync::Arc::get_mut(&mut self.inner) {
            let lock_data = inner.mutex.get_mut().unwrap();
            let data = unsafe { inner.data.get().as_mut() }.unwrap();
            debug_assert!(!lock_data.owned);
            Some(data)
        } else {
            None
        }
    }

    /// Acquires a `Mutex`, blocking the task in the meantime.  When the
    /// returned `Future` is ready, this task will have sole access to the
    /// protected data.
    pub fn lock(&self) -> MutexFut<T> {
        MutexFut::new(FutState::New, self.clone())
    }

    /// Attempts to acquire the lock.
    ///
    /// If the operation would block, returns `Err` instead.  Otherwise, returns
    /// a guard (not a `Future`).
    ///
    /// # Examples
    /// ```
    /// # extern crate futures_locks;
    /// # use futures_locks::*;
    /// # fn main() {
    /// let mut mtx = Mutex::<u32>::new(0);
    /// match mtx.try_lock() {
    ///     Ok(mut guard) => *guard += 5,
    ///     Err(()) => println!("Better luck next time!")
    /// };
    /// # }
    /// ```
    pub fn try_lock(&self) -> Result<MutexGuard<T>, ()> {
        let mut mtx_data = self.inner.mutex.lock().expect("sync::Mutex::lock");
        if mtx_data.owned {
            Err(())
        } else {
            mtx_data.owned = true;
            Ok(MutexGuard{mutex: self.clone()})
        }
    }

    /// Release the `Mutex`
    fn unlock(&self) {
        let mut mtx_data = self.inner.mutex.lock().expect("sync::Mutex::lock");
        assert!(mtx_data.owned);
        if let Some(tx) = mtx_data.waiters.pop_front() {
            // Send ownership to the waiter
            tx.send(()).expect("Sender::send");
        } else {
            // Relinquish ownership
            mtx_data.owned = false;
        }
    }

    /// Returns true if the two `Mutex` point to the same data else false.
    pub fn ptr_eq(this: &Mutex<T>, other: &Mutex<T>) -> bool {
        sync::Arc::ptr_eq(&this.inner, &other.inner)
    }
}

impl<T: 'static + ?Sized> Mutex<T> {
    /// Acquires a `Mutex` and performs a computation on its guarded value in a
    /// separate task.  Returns a `Future` containing the result of the
    /// computation.
    ///
    /// When using Tokio, this method will often hold the `Mutex` for less time
    /// than chaining a computation to [`lock`](#method.lock).  The reason is
    /// that Tokio polls all tasks promptly upon notification.  However, Tokio
    /// does not guarantee that it will poll all futures promptly when their
    /// owning task gets notified.  So it's best to hold `Mutex`es within their
    /// own tasks, lest their continuations get blocked by slow stacked
    /// combinators.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate futures;
    /// # extern crate futures_locks;
    /// # extern crate tokio_ as tokio;
    /// # use futures_locks::*;
    /// # use tokio::runtime::Runtime;
    /// # fn main() {
    /// let mtx = Mutex::<u32>::new(0);
    /// let mut rt = Runtime::new().unwrap();
    /// rt.block_on(async {
    ///     mtx.with(|mut guard| {
    ///         *guard += 5;
    ///         futures::future::ok::<(), ()>(())
    ///     }).unwrap().await
    /// });
    /// assert_eq!(mtx.try_unwrap().unwrap(), 5);
    /// # }
    /// ```
    #[cfg(any(feature = "tokio", all(feature = "nightly-docs", rustdoc)))]
    #[cfg_attr(feature = "nightly-docs", doc(cfg(feature = "tokio")))]
    pub fn with<F, B, R, E>(&self, f: F)
        -> Result<impl Future<Output = Result<R, E>>, SpawnError>
        where F: FnOnce(MutexGuard<T>) -> B + Send + 'static,
              B: Future<Output = Result<R, E>> + Send + 'static,
              R: Send + 'static,
              E: Send + 'static,
              T: Send
    {
        let (tx, rx) = oneshot::channel::<Result<R, E>>();
        tokio_executor::DefaultExecutor::current().spawn(Box::pin(self.lock()
            .then(move |data| {
                f(data).then(move |result| {
                    // Swallow errors; there's nothing to do if the
                    // receiver got cancelled
                    let _ = tx.send(result);
                    future::ready(())
                })
            })
        ))
            // We control the sender so we're sure it won't be dropped before
            // sending so we can unwrap safely
            .map(|_| rx.map(Result::unwrap))
    }

    /// Like [`with`](#method.with) but for Futures that aren't `Send`.
    /// Spawns a new task on a single-threaded Runtime to complete the Future.
    ///
    /// # Examples
    ///
    /// ```
    /// # extern crate futures;
    /// # extern crate futures_locks;
    /// # extern crate tokio_ as tokio;
    /// # use futures_locks::*;
    /// # use std::rc::Rc;
    /// # use tokio::runtime::current_thread;
    /// # fn main() {
    /// // Note: Rc is not `Send`
    /// let mtx = Mutex::<Rc<u32>>::new(Rc::new(0));
    /// let mut rt = current_thread::Runtime::new().unwrap();
    /// let r = rt.block_on(async {
    ///     mtx.with_local(|mut guard| {
    ///         *Rc::get_mut(&mut *guard).unwrap() += 5;
    ///         futures::future::ok::<(), ()>(())
    ///     }).await
    /// });
    /// assert_eq!(*mtx.try_unwrap().unwrap(), 5);
    /// # }
    /// ```
    #[cfg(any(feature = "tokio", all(feature = "nightly-docs", rustdoc)))]
    #[cfg_attr(feature = "nightly-docs", doc(cfg(feature = "tokio")))]
    pub fn with_local<F, B, R, E>(&self, f: F)
        -> impl Future<Output = Result<R, E>>
        where F: FnOnce(MutexGuard<T>) -> B + 'static,
              B: Future<Output = Result<R, E>> + 'static + Unpin,
              R: 'static,
              E: 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, E>>();
        current_thread::spawn(self.lock()
            .then(move |data| {
                f(data).then(move |result| {
                    // Swallow errors; there's nothing to do if the
                    // receiver got cancelled
                    let _ = tx.send(result);
                    future::ready(())
                })
            })
        );
        // We control the sender so we're sure it won't be dropped before
        // sending so we can unwrap safely
        rx.map(Result::unwrap)
    }
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

// LCOV_EXCL_START
#[cfg(test)]
mod t {
    use super::*;

    /// Pet Kcov
    #[test]
    fn debug() {
        let m = Mutex::<u32>::new(0);
        format!("{:?}", &m);
    }

    #[test]
    fn test_default() {
        let m = Mutex::default();
        let value: u32 = m.try_unwrap().unwrap();
        let expected = u32::default();

        assert_eq!(expected, value);
    }
}
// LCOV_EXCL_STOP
