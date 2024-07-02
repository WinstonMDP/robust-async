use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, sleep},
    time::{Duration, Instant},
};

/// Runtime
pub struct Rt {
    spawner: Spawner,
    executor: Receiver<Arc<dyn TaskTrait>>,
}

impl Rt {
    #[must_use]
    pub fn new() -> Self {
        let (spawner, executor) = channel();
        Self {
            spawner: Spawner(spawner),
            executor,
        }
    }

    pub fn run(self) {
        drop(self.spawner);
        while let Ok(task) = self.executor.recv() {
            task.poll();
        }
    }

    #[must_use]
    pub fn spawner(&self) -> &Spawner {
        &self.spawner
    }
}

impl Default for Rt {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct Spawner(Sender<Arc<dyn TaskTrait>>);

impl Spawner {
    /// # Panics
    ///
    /// When an executor corresponding to the spawner has been dropped.
    pub fn spawn<T>(&self, future: T) -> Join<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send,
    {
        let task = Arc::new(Task::new(self.clone(), future));
        let join = private_clone(&task.join);
        // It is achievable panic, but it was chosen because of ergonomics.
        self.0.send(task).unwrap();
        join
    }
}

// All mutexes are redundant, since the rt is single-threaded,
// but are kept for assurance. So a Task invariant is,
// that it can access inner data without locking.
struct Task<T: Future> {
    future: Mutex<Pin<Box<T>>>,
    spawner: Spawner,
    join: Join<T::Output>,
}

impl<T: Future> Task<T> {
    fn new(spawner: Spawner, future: T) -> Self {
        Self {
            future: Mutex::new(Box::pin(future)),
            spawner,
            join: Join(
                Mutex::new(InnerJoin {
                    waker: None,
                    output: Some(Poll::Pending),
                    cancelled: false,
                })
                .into(),
            ),
        }
    }
}

trait TaskTrait: Send + Sync {
    fn poll(self: Arc<Self>);
}

impl<T> TaskTrait for Task<T>
where
    T: Future + Send + 'static,
    T::Output: Send,
{
    fn poll(self: Arc<Self>) {
        let mut join = self.join.0.try_lock().unwrap();
        // It would be better if the Context could store the cancelled flag to signal futures.
        if join.cancelled {
            return;
        }
        if if let Some(poll) = &mut join.output {
            poll.is_pending()
        } else {
            false
        } {
            if let Poll::Ready(output) = self
                .future
                .try_lock()
                .unwrap()
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(self.clone())))
            {
                if let Some(waker) = &mut join.waker {
                    waker.wake_by_ref();
                }
                join.output = Some(Poll::Ready(output));
            }
        }
    }
}

impl<T> Wake for Task<T>
where
    T: Future + Send + 'static,
    T::Output: Send,
{
    fn wake(self: Arc<Self>) {
        self.spawner.0.send(self.clone()).unwrap();
    }
}

pub struct Join<T>(Arc<Mutex<InnerJoin<T>>>);

struct InnerJoin<T> {
    waker: Option<Waker>,
    output: Option<Poll<T>>,
    cancelled: bool,
}

impl<T> Join<T> {
    // Mut because of the Task-Mutex invariant.
    #[allow(clippy::missing_panics_doc)]
    pub fn cancel(&mut self) {
        self.0.try_lock().unwrap().cancelled = true;
    }
}

impl<T> Future for Join<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut join = self.0.try_lock().unwrap();
        if join.cancelled {
            return Poll::Pending;
        }
        if let Some(poll) = &mut join.output {
            if poll.is_ready() {
                return join.output.take().unwrap();
            }
        }
        if join.waker.is_none() {
            join.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

// It is private, because joins after clone could run simultaneously,
// but it violates the Task-Mutex invariant.
// Manual impl because of unnecessary derive bounds
fn private_clone<T>(join: &Join<T>) -> Join<T> {
    Join(join.0.clone())
}

pub struct AsyncMutex<T>(Arc<UnsafeCell<InnerAsyncMutex<T>>>);

impl<T> AsyncMutex<T> {
    pub fn new(t: T) -> Self {
        Self(
            UnsafeCell::new(InnerAsyncMutex {
                t,
                lock: false,
                wakers: VecDeque::new(),
            })
            .into(),
        )
    }

    #[must_use]
    pub fn lock(&self) -> AsyncLock<T> {
        AsyncLock(self.0.clone())
    }
}

pub struct AsyncLock<T>(Arc<UnsafeCell<InnerAsyncMutex<T>>>);

impl<T> Future for AsyncLock<T> {
    type Output = AsyncMutexGuard<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !(unsafe { &*self.0.get() }).lock {
            (unsafe { &mut *self.0.get() }).lock = true;
            return Poll::Ready(AsyncMutexGuard(self.0.clone()));
        }
        unsafe { &mut *self.0.get() }
            .wakers
            .push_back(cx.waker().clone());
        Poll::Pending
    }
}

struct InnerAsyncMutex<T> {
    t: T,
    lock: bool,
    wakers: VecDeque<Waker>,
}

unsafe impl<T> Send for AsyncMutex<T> {}
unsafe impl<T> Send for AsyncLock<T> {}
unsafe impl<T> Send for AsyncMutexGuard<T> {}
unsafe impl<T> Sync for AsyncMutex<T> {}

pub struct AsyncMutexGuard<T>(Arc<UnsafeCell<InnerAsyncMutex<T>>>);

impl<T> Deref for AsyncMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &unsafe { &*self.0.get() }.t
    }
}

impl<T> DerefMut for AsyncMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut unsafe { &mut *self.0.get() }.t
    }
}

impl<T> Drop for AsyncMutexGuard<T> {
    fn drop(&mut self) {
        if let Some(waker) = unsafe { &mut *self.0.get() }.wakers.pop_front() {
            waker.wake();
        }
        unsafe { &mut *self.0.get() }.lock = false;
    }
}

pub struct Alarm {
    instant: Instant,
}

impl Alarm {
    #[must_use]
    pub fn timer(secs: u64) -> Alarm {
        Alarm {
            instant: Instant::now() + Duration::new(secs, 0),
        }
    }
}

impl Future for Alarm {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if Instant::now() >= self.instant {
            println!("Done");
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        let instant = self.instant;
        thread::spawn(move || {
            sleep(instant - Instant::now());
            waker.wake();
        });
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ord;

    fn is_sorted<T: Clone + Ord>(a: &[T]) -> bool {
        let mut c = a.to_vec();
        c.sort_unstable();
        a == c
    }

    #[test]
    fn spawn_t_1() {
        let rt = Rt::new();
        let b = Box::leak(Box::new(Mutex::new(false)));
        rt.spawner().spawn(async { *b.try_lock().unwrap() = true });
        rt.run();
        assert!(*b.try_lock().unwrap());
    }

    #[test]
    fn spawn_t_2() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Box::leak(Box::new(Mutex::new(false)));
        let rt = Rt::new();
        rt.spawner()
            .spawn(async { *b.try_lock().unwrap() = bool_future().await });
        rt.run();
    }

    #[test]
    fn spawn_t_3() {
        let v = Box::leak(Box::new(Mutex::new(Vec::new())));
        let rt = Rt::new();
        rt.spawner().spawn(async {
            Alarm::timer(3).await;
            v.try_lock().unwrap().push(1);
        });
        rt.spawner().spawn(async {
            Alarm::timer(1).await;
            v.try_lock().unwrap().push(0);
        });
        rt.run();
        assert!(is_sorted(&v.try_lock().unwrap()));
    }

    #[test]
    fn spawn_t_4() {
        let v = Arc::new(Mutex::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let c_c_v = c_v.clone();
            spawner.spawn(async move {
                Alarm::timer(3).await;
                c_c_v.try_lock().unwrap().push(1);
            });
            async move { c_v.try_lock().unwrap().push(0) }.await;
        });
        rt.run();
        assert!(is_sorted(&v.try_lock().unwrap()));
    }

    #[test]
    fn join_t_1() {
        let v = Arc::new(Mutex::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let c_c_v = c_v.clone();
            let join = spawner.spawn(async move {
                Alarm::timer(3).await;
                c_c_v.try_lock().unwrap().push(0);
            });
            join.await;
            c_v.try_lock().unwrap().push(1);
        });
        rt.run();
        assert!(is_sorted(&v.try_lock().unwrap()));
    }

    #[test]
    fn join_t_2() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Box::leak(Box::new(Mutex::new(false)));
        let rt = Rt::new();
        let join = rt.spawner().spawn(bool_future());
        rt.spawner().spawn(async {
            *b.try_lock().unwrap() = join.await;
        });
        rt.run();
        assert!(*b.try_lock().unwrap());
    }

    #[test]
    fn join_t_3() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Arc::new(Mutex::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let join = spawner.spawn(bool_future());
            *c_b.try_lock().unwrap() = join.await;
        });
        rt.run();
        assert!(*b.try_lock().unwrap());
    }

    #[test]
    fn join_t_4() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            Alarm::timer(3).await;
            true
        }
        let b = Arc::new(Mutex::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let join = spawner.spawn(bool_future());
            *c_b.try_lock().unwrap() = join.await;
        });
        rt.run();
        assert!(*b.try_lock().unwrap());
    }

    #[test]
    fn cancel_t_1() {
        let b = Arc::new(Mutex::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let mut join = spawner.spawn(async move {
                Alarm::timer(3).await;
                *c_b.try_lock().unwrap() = true;
            });
            join.cancel();
        });
        rt.run();
        assert!(!*b.try_lock().unwrap());
    }

    #[test]
    fn cancel_t_2() {
        let b = Arc::new(Mutex::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let mut join = spawner.spawn(async move {
                Alarm::timer(3).await;
                *c_b.try_lock().unwrap() = true;
            });
            join.cancel();
            join.await;
        });
        rt.run();
        assert!(!*b.try_lock().unwrap());
    }

    #[test]
    fn async_mutex_t_1() {
        let v = Arc::new(Mutex::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let async_mutex = Arc::new(AsyncMutex::new(()));
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let async_mutex = async_mutex.clone();
            let _guard = async_mutex.lock().await;
            let c_c_v = c_v.clone();
            spawner.spawn(async move {
                async_mutex.lock().await;
                c_c_v.try_lock().unwrap().push(1);
            });
            Alarm::timer(3).await;
            c_v.try_lock().unwrap().push(0);
        });
        rt.run();
        assert!(is_sorted(&v.try_lock().unwrap()));
    }

    #[test]
    fn alarm_t_1() {
        let rt = Rt::new();
        rt.spawner().spawn(Alarm::timer(3));
        rt.run();
    }
}
