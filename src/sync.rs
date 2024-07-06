use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, Waker},
};

pub struct AsyncMutex<T> {
    t: UnsafeCell<T>,
    lock: Mutex<bool>,
    wakers: Mutex<VecDeque<Waker>>,
}

impl<T> AsyncMutex<T> {
    pub fn new(t: T) -> Self {
        Self {
            t: t.into(),
            lock: false.into(),
            wakers: VecDeque::new().into(),
        }
    }
}

unsafe impl<T: Send> Sync for AsyncMutex<T> {}

impl<'a, T> Future for &'a AsyncMutex<T> {
    type Output = AsyncMutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut lock = self.lock.lock().unwrap();
        if !*lock {
            *lock = true;
            return Poll::Ready(AsyncMutexGuard(&self));
        }
        self.wakers
            .try_lock()
            .unwrap()
            .push_back(cx.waker().clone());
        Poll::Pending
    }
}

pub struct AsyncMutexGuard<'a, T>(&'a AsyncMutex<T>);

impl<T> Deref for AsyncMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.t.get() }
    }
}

impl<T> DerefMut for AsyncMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.t.get() }
    }
}

impl<T> Drop for AsyncMutexGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(waker) = self.0.wakers.try_lock().unwrap().pop_front() {
            waker.wake();
        }
        *self.0.lock.try_lock().unwrap() = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{io::Alarm, rt::Rt, test_utils::is_sorted};
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
            let _guard = async_mutex.as_ref().await;
            let async_mutex = async_mutex.clone();
            let c_c_v = c_v.clone();
            spawner.spawn(async move {
                async_mutex.as_ref().await;
                c_c_v.borrow_mut().push(1);
            });
            Alarm::timer(3).await;
            c_v.borrow_mut().push(0);
        });
        rt.run();
        assert!(is_sorted(&v.borrow()));
    }
}
