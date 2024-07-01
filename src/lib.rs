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
    spawner: Sender<Arc<Task>>,
    executor: Receiver<Arc<Task>>,
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
    pub fn spawner(&self) -> &Sender<Arc<Task>> {
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
pub fn spawn(
    spawner: &Sender<Arc<Task>>,
    future: impl Future<Output = ()> + Send + 'static,
) -> Join {
    let task = Arc::new(Task::new(spawner.clone(), future));
    let join = task.join.clone();
    // It is achievable panic, but it was chosen because of ergonomics.
    spawner.send(task).unwrap();
    join
}

#[derive(Clone)]
pub struct Join {
    waker: Arc<Mutex<Option<Waker>>>,
    completed: Arc<Mutex<bool>>,
}

pub struct Task {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    spawner: Sender<Arc<Self>>,
    join: Join,
}

impl Task {
    fn new(spawner: Sender<Arc<Task>>, future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            future: Mutex::new(Box::pin(future)),
            spawner,
            join: Join {
                waker: Mutex::new(None).into(),
                completed: Mutex::new(false).into(),
            },
        }
    }

    fn poll(self: &Arc<Self>) {
        if !*self.join.completed.try_lock().unwrap() {
            if let Poll::Ready(()) = self
                .future
                .try_lock()
                .unwrap()
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(self.clone())))
            {
                if let Some(waker) = &mut *self.join.waker.try_lock().unwrap() {
                    waker.wake_by_ref();
                }
                *self.join.completed.try_lock().unwrap() = true;
            }
        }
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.spawner.send(self.clone()).unwrap();
    }
}

impl Future for Join {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if *self.completed.try_lock().unwrap() {
            return Poll::Ready(());
        }
        let mut join_waker = self.waker.try_lock().unwrap();
        if (*join_waker).is_none() {
            *join_waker = Some(cx.waker().clone());
        }
        Poll::Pending
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
