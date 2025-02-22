use std::future::Future;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

pub(crate) struct MaybeDone<R> {
    fut: Option<Pin<Box<dyn Future<Output = R> + Send>>>,
}

impl<R> MaybeDone<R> {
    pub fn empty() -> Self {
        Self { fut: None }
    }

    pub fn set(&mut self, fut: impl Future<Output = R> + Send + 'static) -> &mut Self {
        self.fut = Some(Box::pin(fut));
        self
    }
}

impl<E> MaybeDone<Result<(), E>> {
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), E>> {
        if let Some(fut) = &mut self.fut {
            let res = ready!(Pin::new(fut).poll(cx));
            self.fut = None;
            Poll::Ready(res)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
