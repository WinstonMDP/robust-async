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
    task_sender: Sender<Box<dyn TaskTrait>>,
    send_task_sender: Sender<Box<dyn SendTaskTrait>>,
    task_receiver: Receiver<Box<dyn TaskTrait>>,
    send_task_receiver: Receiver<Box<dyn SendTaskTrait>>,
    id_sender: Sender<Id>,
    id_receiver: Receiver<Id>,
    signal_sender: Sender<Signal>,
    executor: Receiver<Signal>,
    tasks: Vec<Option<Box<dyn TaskTrait>>>,
    send_tasks: Vec<Option<Box<dyn SendTaskTrait>>>,
}

impl Rt {
    #[must_use]
    pub fn new() -> Self {
        let (task_sender, task_receiver) = channel();
        let (send_task_sender, send_task_receiver) = channel();
        let (id_sender, id_receiver) = channel();
        let (signal_sender, executor) = channel();
        Self {
            task_sender,
            send_task_sender,
            task_receiver,
            send_task_receiver,
            id_sender,
            id_receiver,
            signal_sender,
            executor,
            tasks: Vec::new(),
            send_tasks: Vec::new(),
        }
    }

    #[allow(clippy::missing_panics_doc)]
    pub fn run(mut self) {
        drop(self.signal_sender);
        let mut free_options = Vec::new();
        while let Ok(signal) = self.executor.recv() {
            let id = match signal {
                Signal::Task => {
                    let mut task = self.task_receiver.try_recv().unwrap();
                    let id = Id {
                        is_send: false,
                        i: free_options.pop().unwrap_or(self.tasks.len()),
                    };
                    task.set_id(id.clone());
                    if id.i == self.tasks.len() {
                        self.tasks.push(Some(task));
                    } else {
                        self.tasks[id.i] = Some(task);
                    }
                    id
                }
                Signal::SendTask => {
                    let mut task = self.send_task_receiver.try_recv().unwrap();
                    let id = Id {
                        is_send: true,
                        i: free_options.pop().unwrap_or(self.tasks.len()),
                    };
                    task.set_id(id.clone());
                    if id.i == self.tasks.len() {
                        self.send_tasks.push(Some(task));
                    } else {
                        self.send_tasks[id.i] = Some(task);
                    }
                    id
                }
                Signal::Id => self.id_receiver.try_recv().unwrap(),
            };
            if id.is_send {
                let task = self.send_tasks[id.i].as_ref().unwrap();
                task.poll();
                if task.is_completed() {
                    self.send_tasks[id.i].take();
                    free_options.push(id.i);
                }
            } else {
                let task = self.tasks[id.i].as_ref().unwrap();
                task.poll();
                if task.is_completed() {
                    self.tasks[id.i].take();
                    free_options.push(id.i);
                }
            }
        }
    }

    #[must_use]
    pub fn spawner(&self) -> Spawner {
        Spawner {
            task_sender: self.task_sender.clone(),
            id_sender: self.id_sender.clone(),
            signal_sender: self.signal_sender.clone(),
        }
    }

    #[must_use]
    pub fn send_spawner(&self) -> SendSpawner {
        SendSpawner {
            task_sender: self.send_task_sender.clone(),
            id_sender: self.id_sender.clone(),
            signal_sender: self.signal_sender.clone(),
        }
    }
}

impl Default for Rt {
    fn default() -> Self {
        Self::new()
    }
}

trait TaskTrait {
    fn poll(&self);
    fn set_id(&mut self, id: Id);
    fn is_completed(&self) -> bool;
}

trait SendTaskTrait: TaskTrait + Send + Sync {}

#[derive(Clone)]
pub struct Spawner {
    task_sender: Sender<Box<dyn TaskTrait>>,
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
}

impl Spawner {
    /// # Panics
    ///
    /// When an executor corresponding to the spawner has been dropped.
    pub fn spawn<T: Future + 'static>(&self, future: T) -> Join<T::Output> {
        let task = Box::new(Task::new(
            self.id_sender.clone(),
            self.signal_sender.clone(),
            future,
        ));
        let join = private_clone(&task.join);
        // It is achievable panic, but it was chosen because of ergonomics.
        self.task_sender.send(task).unwrap();
        self.signal_sender.send(Signal::Task).unwrap();
        join
    }
}

#[derive(Clone)]
pub struct SendSpawner {
    task_sender: Sender<Box<dyn SendTaskTrait>>,
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
}

impl SendSpawner {
    /// # Panics
    ///
    /// When an executor corresponding to the spawner has been dropped.
    pub fn spawn<T>(&self, future: T) -> Join<T::Output>
    where
        T: Future + 'static + Send + Sync,
        T::Output: Send,
    {
        let task = Box::new(Task::new(
            self.id_sender.clone(),
            self.signal_sender.clone(),
            future,
        ));
        let join = private_clone(&task.join);
        // It is achievable panic, but it was chosen because of ergonomics.
        self.task_sender.send(task).unwrap();
        self.signal_sender.send(Signal::SendTask).unwrap();
        join
    }
}

// All mutexes are redundant, since the rt is single-threaded,
// but are kept for assurance. So a Task invariant is,
// that it can access inner data without locking.
struct Task<T: Future> {
    future: Mutex<Pin<Box<T>>>,
    id: Option<Id>,
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
    join: Join<T::Output>,
}

impl<T: Future> Task<T> {
    fn new(id_sender: Sender<Id>, signal_sender: Sender<Signal>, future: T) -> Self {
        Self {
            future: Mutex::new(Box::pin(future)),
            id: None,
            id_sender,
            signal_sender,
            join: Join(
                Mutex::new(InnerJoin {
                    waker: None,
                    output: Poll::Pending,
                })
                .into(),
            ),
        }
    }
}

impl<T: Future> TaskTrait for Task<T> {
    fn poll(&self) {
        let mut join = self.join.0.try_lock().unwrap();
        if join.output.is_pending() {
            if let Poll::Ready(output) =
                self.future
                    .try_lock()
                    .unwrap()
                    .as_mut()
                    .poll(&mut Context::from_waker(&Waker::from(Arc::new(IdWaker {
                        id_sender: self.id_sender.clone(),
                        id: self.id.clone().unwrap(),
                        signal_sender: self.signal_sender.clone(),
                    }))))
            {
                if let Some(waker) = &mut join.waker {
                    waker.wake_by_ref();
                }
                join.output = Poll::Ready(Some(output));
            }
        }
    }

    fn set_id(&mut self, id: Id) {
        self.id = Some(id);
    }

    fn is_completed(&self) -> bool {
        self.join.0.try_lock().unwrap().output.is_ready()
    }
}

impl<T> SendTaskTrait for Task<T>
where
    T: Future + Send + Sync,
    T::Output: Send,
{
}

enum Signal {
    Task,
    SendTask,
    Id,
}

#[derive(Clone)]
struct Id {
    is_send: bool,
    i: usize,
}

struct IdWaker {
    id_sender: Sender<Id>,
    id: Id,
    signal_sender: Sender<Signal>,
}

impl Wake for IdWaker {
    fn wake(self: Arc<Self>) {
        self.id_sender.send(self.id.clone()).unwrap();
        self.signal_sender.send(Signal::Id).unwrap();
    }
}

pub struct Join<T>(Arc<Mutex<InnerJoin<T>>>);

struct InnerJoin<T> {
    waker: Option<Waker>,
    output: Poll<Option<T>>,
}

impl<T> Join<T> {
    // Mut because of the Task-Mutex invariant.
    // It would be better if the Context could store the cancelled flag to signal futures.
    #[allow(clippy::missing_panics_doc)]
    pub fn cancel(&mut self) {
        self.0.try_lock().unwrap().output = Poll::Ready(None);
    }
}

impl<T> Future for Join<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut join = self.0.try_lock().unwrap();
        if let Poll::Ready(poll) = &mut join.output {
            return Poll::Ready(poll.take());
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

pub struct AsyncMutex<T>(UnsafeCell<InnerAsyncMutex<T>>);

impl<T> AsyncMutex<T> {
    pub fn new(t: T) -> Self {
        Self(UnsafeCell::new(InnerAsyncMutex {
            t,
            lock: false,
            wakers: VecDeque::new(),
        }))
    }

    #[must_use]
    pub fn lock(&self) -> AsyncLock<T> {
        AsyncLock(self)
    }
}

pub struct AsyncLock<'a, T>(&'a AsyncMutex<T>);

pub struct AsyncMutexGuard<'a, T>(AsyncLock<'a, T>);

impl<'a, T> Future for AsyncLock<'a, T> {
    type Output = AsyncMutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !unsafe { &*self.0 .0.get() }.lock {
            unsafe { &mut *self.0 .0.get() }.lock = true;
            return Poll::Ready(AsyncMutexGuard(AsyncLock(self.0)));
        }
        unsafe { &mut *self.0 .0.get() }
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

impl<T> Deref for AsyncMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &unsafe { &*self.0 .0 .0.get() }.t
    }
}

impl<T> DerefMut for AsyncMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut unsafe { &mut *self.0 .0 .0.get() }.t
    }
}

impl<T> Drop for AsyncMutexGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(waker) = unsafe { &mut *self.0 .0 .0.get() }.wakers.pop_front() {
            waker.wake();
        }
        unsafe { &mut *self.0 .0 .0.get() }.lock = false;
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
    fn spawn_t_5() {
        let rt = Rt::new();
        let spawner = rt.send_spawner().clone();
        rt.spawner().spawn(async move {
            thread::spawn(move || spawner.spawn(Alarm::timer(3)));
            Alarm::timer(3).await;
        });
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
            *b.try_lock().unwrap() = join.await.unwrap();
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
            *c_b.try_lock().unwrap() = join.await.unwrap();
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
            *c_b.try_lock().unwrap() = join.await.unwrap();
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
        let v = Rc::new(RefCell::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let async_mutex = Rc::new(AsyncMutex::new(()));
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let async_mutex = async_mutex.clone();
            let _guard = async_mutex.lock().await;
            let async_mutex = async_mutex.clone();
            let c_c_v = c_v.clone();
            spawner.spawn(async move {
                async_mutex.lock().await;
                c_c_v.borrow_mut().push(1);
            });
            Alarm::timer(3).await;
            c_v.borrow_mut().push(0);
        });
        rt.run();
        assert!(is_sorted(&v.borrow()));
    }

    #[test]
    fn alarm_t_1() {
        let rt = Rt::new();
        rt.spawner().spawn(Alarm::timer(3));
        rt.run();
    }
}
