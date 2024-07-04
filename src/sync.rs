use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll, Waker},
};

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

struct InnerAsyncMutex<T> {
    t: T,
    lock: bool,
    wakers: VecDeque<Waker>,
}

pub struct AsyncLock<'a, T>(&'a AsyncMutex<T>);

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

pub struct AsyncMutexGuard<'a, T>(AsyncLock<'a, T>);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{io::Alarm, rt::Rt};
    use std::{cell::RefCell, rc::Rc};

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
        assert!(crate::test_utils::is_sorted(&v.borrow()));
    }
}
