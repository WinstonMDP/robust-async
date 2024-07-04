use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    thread::{self, sleep},
    time::{Duration, Instant},
};

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
    use super::Alarm;
    use crate::rt::Rt;

    #[test]
    fn alarm_t_1() {
        let rt = Rt::new();
        rt.spawner().spawn(Alarm::timer(3));
        rt.run();
    }
}
