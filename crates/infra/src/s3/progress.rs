//! Observe HTTP body consumption without a second payload allocation. The SDK
//! TTFB timeout includes uploading the request; disable it for PUT/UploadPart and
//! use progress plus the independent 15-minute operation limit instead.
use super::{IDLE_TIMEOUT, timeouts};
use archive_domain::{Location, OperationError};
use aws_smithy_types::{body::SdkBody, byte_stream::ByteStream};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{sync::watch, time::Instant};

pub(super) fn upload_timeouts() -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::config::Builder::new()
        .timeout_config(timeouts().to_builder().disable_read_timeout().build())
}

pub(super) fn body(bytes: Bytes) -> (ByteStream, watch::Receiver<Instant>) {
    let (progress, receiver) = watch::channel(Instant::now());
    let body = ByteStream::from(bytes).map(move |inner| {
        SdkBody::from_body_1_x(ProgressBody {
            inner,
            pending: Bytes::new(),
            progress: progress.clone(),
        })
    });
    (body, receiver)
}

struct ProgressBody {
    inner: SdkBody,
    pending: Bytes,
    progress: watch::Sender<Instant>,
}
impl Body for ProgressBody {
    type Data = Bytes;
    type Error = <SdkBody as Body>::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            if !this.pending.is_empty() {
                let data = this.pending.split_to(this.pending.len().min(16 * 1024));
                this.progress.send_replace(Instant::now());
                return Poll::Ready(Some(Ok(Frame::data(data))));
            }
            match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => this.pending = data,
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }
    fn is_end_stream(&self) -> bool {
        self.pending.is_empty() && self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        let hint = self.inner.size_hint();
        let mut out = SizeHint::new();
        out.set_lower(hint.lower() + self.pending.len() as u64);
        if let Some(upper) = hint.upper() {
            out.set_upper(upper + self.pending.len() as u64);
        }
        out
    }
}

pub(super) async fn request<T>(
    future: impl Future<Output = Result<T, OperationError>>,
    progress: watch::Receiver<Instant>,
    destination: &Location,
) -> Result<T, OperationError> {
    request_with_idle(future, progress, destination, IDLE_TIMEOUT).await
}

async fn request_with_idle<T>(
    future: impl Future<Output = Result<T, OperationError>>,
    mut progress: watch::Receiver<Instant>,
    destination: &Location,
    idle: std::time::Duration,
) -> Result<T, OperationError> {
    tokio::pin!(future);
    let timer = tokio::time::sleep(idle);
    tokio::pin!(timer);
    let mut closed = false;
    loop {
        tokio::select! { biased;
            result = &mut future => return result,
            changed = progress.changed(), if !closed => {
                closed = changed.is_err();
                timer.as_mut().reset(*progress.borrow_and_update() + idle);
            },
            _ = &mut timer => return Err(super::upload::unknown(destination, "s3-upload", "HTTP body or response made no progress for 120 seconds")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn upload_progress_extends_idle_deadline_without_extending_the_operation_limit() {
        let (sender, receiver) = watch::channel(Instant::now());
        let destination = Location::Local("/fixture".into());
        let flowing = async move {
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                sender.send_replace(Instant::now());
            }
            Ok::<_, OperationError>(())
        };
        request_with_idle(flowing, receiver, &destination, Duration::from_millis(250))
            .await
            .unwrap();
        let (_sender, receiver) = watch::channel(Instant::now());
        let error = request_with_idle(
            std::future::pending::<Result<(), OperationError>>(),
            receiver,
            &destination,
            Duration::from_millis(20),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.outcome, archive_domain::OperationState::Unknown);
    }
    #[tokio::test]
    async fn upload_frames_share_the_original_bytes_and_keep_exact_length() {
        use std::future::poll_fn;
        let data = Bytes::from(vec![0x5a; 1024 * 1024]);
        let pointer = data.as_ptr();
        let (stream, _) = body(data);
        let mut stream = stream.into_inner();
        assert_eq!(stream.size_hint().exact(), Some(1024 * 1024));
        let frame = poll_fn(|cx| Pin::new(&mut stream).poll_frame(cx))
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(frame.as_ptr(), pointer);
        assert_eq!(frame.len(), 16 * 1024);
        assert_eq!(stream.size_hint().exact(), Some(1024 * 1024 - 16 * 1024));
    }
}
