use std::{
    future::Future,
    pin::Pin,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, sleep},
    time::Instant,
};

pub struct Rt {
    spawner: Sender<Arc<dyn TaskTrait>>,
    executor: Receiver<Arc<dyn TaskTrait>>,
}

impl Rt {
    #[must_use]
    pub fn new() -> Self {
        let (spawner, executor) = channel();
        Self { spawner, executor }
    }

    pub fn run(self) {
        drop(self.spawner);
        while let Ok(task) = self.executor.recv() {
            task.poll();
        }
    }

    #[must_use]
    pub fn spawner(&self) -> &Sender<Arc<dyn TaskTrait>> {
        &self.spawner
    }
}

impl Default for Rt {
    fn default() -> Self {
        Self::new()
    }
}

/// # Panics
///
/// When an executor corresponding to the spawner has been dropped.
pub fn spawn<T>(spawner: &Sender<Arc<dyn TaskTrait>>, future: T) -> Join<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send,
{
    let task = Arc::new(Task::new(spawner.clone(), future));
    let join = task.join.clone();
    // It is achievable panic, but it was chosen because of ergonomics.
    spawner.send(task).unwrap();
    join
}

pub struct Task<T: Future> {
    future: Mutex<Pin<Box<T>>>,
    spawner: Sender<Arc<dyn TaskTrait>>,
    join: Join<T::Output>,
}

impl<T: Future> Task<T> {
    fn new(spawner: Sender<Arc<dyn TaskTrait>>, future: T) -> Self {
        Self {
            future: Mutex::new(Box::pin(future)),
            spawner,
            join: Join {
                waker: Mutex::new(None).into(),
                poll: Mutex::new(Some(Poll::Pending)).into(),
                cancelled: Mutex::new(false).into(),
            },
        }
    }
}

pub trait TaskTrait: Send + Sync {
    fn poll(self: Arc<Self>);
}

impl<T> TaskTrait for Task<T>
where
    T: Future + Send + 'static,
    T::Output: Send,
{
    fn poll(self: Arc<Self>) {
        // It would be better if the Context could store the cancelled flag to signal futures
        if *self.join.cancelled.try_lock().unwrap() {
            return;
        }
        if if let Some(poll) = &mut *self.join.poll.try_lock().unwrap() {
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
                if let Some(waker) = &mut *self.join.waker.try_lock().unwrap() {
                    waker.wake_by_ref();
                }
                *self.join.poll.try_lock().unwrap() = Some(Poll::Ready(output));
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
        self.spawner.send(self.clone()).unwrap();
    }
}

pub struct Join<T> {
    waker: Arc<Mutex<Option<Waker>>>,
    poll: Arc<Mutex<Option<Poll<T>>>>,
    cancelled: Arc<Mutex<bool>>,
}

impl<T> Join<T> {
    // NOTE: the panic belongs to Mutexes and Arcs, so I'll put it off for later
    pub fn cancel(&self) {
        *self.cancelled.try_lock().unwrap() = true;
    }
}

impl<T> Future for Join<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if *self.cancelled.try_lock().unwrap() {
            return Poll::Pending;
        }
        let mut poll_slot = self.poll.try_lock().unwrap();
        if let Some(poll) = &mut *poll_slot {
            if poll.is_ready() {
                return poll_slot.take().unwrap();
            }
        }
        let mut join_waker = self.waker.try_lock().unwrap();
        if (*join_waker).is_none() {
            *join_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

// Manual impl because of unnecessary derive bounds
impl<T> Clone for Join<T> {
    fn clone(&self) -> Self {
        Join {
            waker: self.waker.clone(),
            poll: self.poll.clone(),
            cancelled: self.cancelled.clone(),
        }
    }
}

pub struct Alarm {
    pub instant: Instant,
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
    use std::time::Duration;

    #[test]
    fn run_unit_future() {
        let rt = Rt::new();
        spawn(rt.spawner(), async { println!("hey") });
        rt.run();
    }

    #[test]
    fn run_bool_future() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let rt = Rt::new();
        spawn(rt.spawner(), async { println!("{}", bool_future().await) });
        rt.run();
    }

    #[test]
    fn concurrent_run() {
        let rt = Rt::new();
        spawn(
            rt.spawner(),
            Alarm {
                instant: Instant::now() + Duration::new(5, 0),
            },
        );
        spawn(
            rt.spawner(),
            Alarm {
                instant: Instant::now() + Duration::new(1, 0),
            },
        );
        rt.run();
    }

    #[test]
    fn nested_spawn() {
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            spawn(&cloned_spawner, async move {
                println!("hey");
            });
        });
        rt.run();
    }

    #[test]
    fn join_run() {
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            let join = spawn(
                &cloned_spawner,
                Alarm {
                    instant: Instant::now() + Duration::new(5, 0),
                },
            );
            join.await;
            println!("hey");
        });
        rt.run();
    }

    #[test]
    fn join_bool_future() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let rt = Rt::new();
        let join = spawn(rt.spawner(), bool_future());
        spawn(rt.spawner(), async {
            println!("{}", join.await);
        });
        rt.run();
    }

    #[test]
    fn join_nested_bool_future() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            let join = spawn(&cloned_spawner, bool_future());
            println!("{}", join.await);
        });
        rt.run();
    }

    #[test]
    fn join_delayed_bool_future() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            Alarm {
                instant: Instant::now() + Duration::new(3, 0),
            }
            .await;
            true
        }
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            let join = spawn(&cloned_spawner, bool_future());
            println!("{}", join.await);
        });
        rt.run();
    }

    #[test]
    fn cancel() {
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            let join = spawn(
                &cloned_spawner,
                Alarm {
                    instant: Instant::now() + Duration::new(5, 0),
                },
            );
            join.cancel();
        });
        rt.run();
    }

    #[test]
    fn cancel_with_await() {
        let rt = Rt::new();
        let cloned_spawner = rt.spawner().clone();
        spawn(rt.spawner(), async move {
            let join = spawn(
                &cloned_spawner,
                Alarm {
                    instant: Instant::now() + Duration::new(5, 0),
                },
            );
            join.cancel();
            join.await;
        });
        rt.run();
    }

    #[test]
    fn alarm() {
        let rt = Rt::new();
        spawn(
            rt.spawner(),
            Alarm {
                instant: Instant::now() + Duration::new(3, 0),
            },
        );
        rt.run();
    }
}
