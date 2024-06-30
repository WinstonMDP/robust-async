use futures::task::{waker, ArcWake};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        mpsc::{channel, Sender},
        Arc, Mutex,
    },
    task::{Context, Poll},
    thread::{self, sleep},
    time::Instant,
};

pub fn block_on(future: impl Future<Output = ()> + Send + 'static) {
    let (scheduler, schedule) = channel();
    Arc::new(Task::new(scheduler, future)).schedule();
    while let Ok(task) = schedule.recv() {
        task.poll();
    }
}

struct TaskFuture {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    poll: Poll<()>,
}

struct Task {
    task_future: Mutex<TaskFuture>,
    scheduler: Sender<Arc<Self>>,
}

impl Task {
    fn new(
        scheduler: Sender<Arc<Task>>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            task_future: Mutex::new(TaskFuture {
                future: Box::pin(future),
                poll: Poll::Pending,
            }),
            scheduler,
        }
    }

    fn schedule(self: &Arc<Self>) {
        self.scheduler.send(self.clone()).unwrap();
    }

    fn poll(self: &Arc<Self>) {
        let mut task_future = self.task_future.try_lock().unwrap();
        if task_future.poll.is_pending() {
            task_future.poll = task_future
                .future
                .as_mut()
                .poll(&mut Context::from_waker(&waker(self.clone())));
        }
    }
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.schedule();
    }
}

pub struct Join {
    a: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    b: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl Join {
    pub fn new(
        a: impl Future<Output = ()> + Send + 'static,
        b: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            a: Some(Box::pin(a)),
            b: Some(Box::pin(b)),
        }
    }
}

impl Future for Join {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut a) = self.a {
            if a.as_mut().poll(cx).is_ready() {
                self.a.take();
            }
        }
        if let Some(ref mut b) = self.b {
            if b.as_mut().poll(cx).is_ready() {
                self.b.take();
            }
        }
        if self.a.is_none() && self.b.is_none() {
            return Poll::Ready(());
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
    fn block_on_unit_future() {
        block_on(async {
            println!("hey");
        });
    }

    #[test]
    fn block_on_bool_future() {
        #[allow(clippy::unused_async)]
        async fn bool_future() -> bool {
            true
        }
        block_on(async { println!("{}", bool_future().await) });
    }

    #[test]
    fn join() {
        block_on(async {
            Join::new(
                Alarm {
                    instant: Instant::now() + Duration::new(5, 0),
                },
                Alarm {
                    instant: Instant::now() + Duration::new(1, 0),
                },
            )
            .await;
        });
    }

    #[test]
    fn alarm() {
        block_on(Alarm {
            instant: Instant::now() + Duration::new(3, 0),
        });
    }
}
