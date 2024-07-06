use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    thread::{self, sleep},
    time::{Duration, Instant},
};

pub struct Alarm {
    instant: Instant,
    waker: Option<Waker>,
}

impl Alarm {
    #[must_use]
    pub fn timer(secs: u64) -> Alarm {
        Alarm {
            instant: Instant::now() + Duration::new(secs, 0),
            waker: None,
        }
    }
}

impl Future for Alarm {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if Instant::now() >= self.instant {
            println!("Done");
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        if self.waker.is_some() {
            self.waker = Some(waker);
        } else {
            let instant = self.instant;
            thread::spawn(move || {
                sleep(instant - Instant::now());
                waker.wake();
            });
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::Alarm;
    use crate::rt::Rt;
    use std::{
        future::Future,
        pin::Pin,
        sync::Arc,
        task::{Context, Wake, Waker},
    };

    #[test]
    fn alarm_t_1() {
        let rt = Rt::new();
        rt.spawner().spawn(Alarm::timer(3));
        rt.run();
    }

    #[test]
    fn alarm_t_2() {
        struct Empty {}

        impl Wake for Empty {
            fn wake(self: std::sync::Arc<Self>) {}
        }

        let rt = Rt::new();
        let mut alarm = Alarm::timer(3);
        let _future =
            Pin::new(&mut alarm).poll(&mut Context::from_waker(&Waker::from(Arc::new(Empty {}))));
        rt.spawner().spawn(alarm);
        rt.run();
    }
}
