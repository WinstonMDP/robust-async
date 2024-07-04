use std::{
    future::Future,
    pin::Pin,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
};

trait TaskTrait {
    fn poll(&self);
    fn set_id(&mut self, id: Id);
    fn is_completed(&self) -> bool;
}

trait SendTaskTrait: TaskTrait + Send + Sync {}

/// Runtime
pub struct Rt {
    executor: Receiver<Signal>,
    id_receiver: Receiver<Id>,
    id_sender: Sender<Id>,
    send_task_receiver: Receiver<Box<dyn SendTaskTrait>>,
    send_task_sender: Sender<Box<dyn SendTaskTrait>>,
    send_tasks: Vec<Option<Box<dyn SendTaskTrait>>>,
    signal_sender: Sender<Signal>,
    task_receiver: Receiver<Box<dyn TaskTrait>>,
    task_sender: Sender<Box<dyn TaskTrait>>,
    tasks: Vec<Option<Box<dyn TaskTrait>>>,
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
        let mut send_free_options = Vec::new();
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
                        i: send_free_options.pop().unwrap_or(self.send_tasks.len()),
                    };
                    task.set_id(id.clone());
                    if id.i == self.send_tasks.len() {
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
                    send_free_options.push(id.i);
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

#[derive(Clone)]
pub struct Spawner {
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
    task_sender: Sender<Box<dyn TaskTrait>>,
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
        let join = task.join.clone();
        // It is achievable panic, but it was chosen because of ergonomics.
        self.task_sender.send(task).unwrap();
        self.signal_sender.send(Signal::Task).unwrap();
        join
    }
}

#[derive(Clone)]
pub struct SendSpawner {
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
    task_sender: Sender<Box<dyn SendTaskTrait>>,
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
        let join = task.join.clone();
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
    join: Join<T::Output>,
    signal_sender: Sender<Signal>,
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
    Id,
    SendTask,
    Task,
}

#[derive(Clone)]
struct Id {
    i: usize,
    is_send: bool,
}

struct IdWaker {
    id: Id,
    id_sender: Sender<Id>,
    signal_sender: Sender<Signal>,
}

impl Wake for IdWaker {
    fn wake(self: Arc<Self>) {
        self.id_sender.send(self.id.clone()).unwrap();
        self.signal_sender.send(Signal::Id).unwrap();
    }
}

pub struct Join<T>(Arc<Mutex<InnerJoin<T>>>);

impl<T> Join<T> {
    // Mut because of the Task-Mutex invariant.
    // It would be better if the Context could store the cancelled flag to signal futures.
    #[allow(clippy::missing_panics_doc)]
    pub fn cancel(&mut self) {
        self.0.try_lock().unwrap().output = Poll::Ready(None);
    }

    // It is private, because joins after clone could run simultaneously,
    // but it violates the Task-Mutex invariant.
    // Manual impl because of unnecessary derive bounds
    fn clone(&self) -> Self {
        Join(self.0.clone())
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

struct InnerJoin<T> {
    output: Poll<Option<T>>,
    waker: Option<Waker>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{io::Alarm, test_utils::is_sorted};
    use std::{cell::RefCell, rc::Rc, thread};

    #[test]
    fn spawn_t_1() {
        let rt = Rt::new();
        let b = Box::leak(Box::new(RefCell::new(false)));
        rt.spawner().spawn(async { *b.borrow_mut() = true });
        rt.run();
        assert!(*b.borrow());
    }

    #[test]
    fn spawn_t_2() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Box::leak(Box::new(RefCell::new(false)));
        let rt = Rt::new();
        rt.spawner()
            .spawn(async { *b.borrow_mut() = bool_future().await });
        rt.run();
    }

    #[test]
    fn spawn_t_3() {
        let v = Box::leak(Box::new(RefCell::new(Vec::new())));
        let rt = Rt::new();
        rt.spawner().spawn(async {
            Alarm::timer(3).await;
            v.borrow_mut().push(1);
        });
        rt.spawner().spawn(async {
            Alarm::timer(1).await;
            v.borrow_mut().push(0);
        });
        rt.run();
        assert!(is_sorted(&v.borrow()));
    }

    #[test]
    fn spawn_t_4() {
        let v = Rc::new(RefCell::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let c_c_v = c_v.clone();
            spawner.spawn(async move {
                Alarm::timer(3).await;
                c_c_v.borrow_mut().push(1);
            });
            async move { c_v.borrow_mut().push(0) }.await;
        });
        rt.run();
        assert!(is_sorted(&v.borrow()));
    }

    #[test]
    fn spawn_t_5() {
        let rt = Rt::new();
        let spawner = rt.send_spawner().clone();
        rt.spawner().spawn(async move {
            let join = thread::spawn(move || spawner.spawn(Alarm::timer(3)))
                .join()
                .unwrap();
            join.await;
        });
        rt.run();
    }

    #[test]
    fn join_t_1() {
        let v = Rc::new(RefCell::new(Vec::new()));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_v = v.clone();
        rt.spawner().spawn(async move {
            let c_c_v = c_v.clone();
            let join = spawner.spawn(async move {
                Alarm::timer(3).await;
                c_c_v.borrow_mut().push(0);
            });
            join.await;
            c_v.borrow_mut().push(1);
        });
        rt.run();
        assert!(is_sorted(&v.borrow()));
    }

    #[test]
    fn join_t_2() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Box::leak(Box::new(RefCell::new(false)));
        let rt = Rt::new();
        let join = rt.spawner().spawn(bool_future());
        rt.spawner().spawn(async {
            *b.borrow_mut() = join.await.unwrap();
        });
        rt.run();
        assert!(*b.borrow_mut());
    }

    #[test]
    fn join_t_3() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let b = Rc::new(RefCell::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let join = spawner.spawn(bool_future());
            *c_b.borrow_mut() = join.await.unwrap();
        });
        rt.run();
        assert!(*b.borrow());
    }

    #[test]
    fn join_t_4() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            Alarm::timer(3).await;
            true
        }
        let b = Rc::new(RefCell::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let join = spawner.spawn(bool_future());
            *c_b.borrow_mut() = join.await.unwrap();
        });
        rt.run();
        assert!(*b.borrow_mut());
    }

    #[test]
    fn cancel_t_1() {
        let b = Rc::new(RefCell::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let mut join = spawner.spawn(async move {
                Alarm::timer(3).await;
                *c_b.borrow_mut() = true;
            });
            join.cancel();
        });
        rt.run();
        assert!(!*b.borrow());
    }

    #[test]
    fn cancel_t_2() {
        let b = Rc::new(RefCell::new(false));
        let rt = Rt::new();
        let spawner = rt.spawner().clone();
        let c_b = b.clone();
        rt.spawner().spawn(async move {
            let mut join = spawner.spawn(async move {
                Alarm::timer(3).await;
                *c_b.borrow_mut() = true;
            });
            join.cancel();
            join.await;
        });
        rt.run();
        assert!(!*b.borrow());
    }
}
