//! The same bounded part scheduler is used by upload, remote copy, and virtual
//! capacity tests. Payload construction/SDK calls are supplied by the adapter.
use archive_domain::*;
use archive_sync::CancelContext;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::task::{JoinError, JoinSet};
type Completion<T> = Result<Result<T, OperationError>, JoinError>;
pub(crate) struct PartScheduler<T: Send + 'static> {
    tasks: JoinSet<Result<T, OperationError>>,
    failure: Arc<Mutex<Option<OperationError>>>,
}
impl<T: Send + 'static> PartScheduler<T> {
    pub fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            failure: Arc::new(Mutex::new(None)),
        }
    }
    pub fn len(&self) -> usize {
        self.tasks.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
    pub fn try_join_next(&mut self) -> Option<Completion<T>> {
        self.tasks.try_join_next()
    }
    pub async fn join_next(&mut self) -> Option<Completion<T>> {
        self.tasks.join_next().await
    }
    pub fn abort_all(&mut self) {
        self.tasks.abort_all();
    }
    fn check(&self) -> Result<(), OperationError> {
        match &*self.failure.lock().unwrap() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
    pub async fn schedule(
        &mut self,
        future: impl Future<Output = Result<T, OperationError>> + Send + 'static,
        cancel: &CancelContext,
    ) -> Result<Vec<Completion<T>>, OperationError> {
        cancel.check("part-scheduler")?;
        self.check()?;
        let mut completed = Vec::with_capacity(4);
        while self.tasks.len() >= 4 {
            let result = tokio::select! {biased;_ =cancel.cancelled()=>{cancel.check("part-scheduler")?;unreachable!()},result=self.tasks.join_next()=>result.expect("scheduled parts")};
            match &result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return Err(error.clone()),
                Err(_) => {
                    let mut error = OperationError::new(
                        ErrorKind::Unavailable,
                        "part-scheduler",
                        "part task ended without a response",
                    );
                    error.outcome = OperationState::Unknown;
                    cancel.record_failure();
                    return Err(error);
                }
            }
            completed.push(result);
        }
        cancel.check("part-scheduler")?;
        self.check()?;
        let failed = self.failure.clone();
        let operation = cancel.clone();
        self.tasks.spawn(async move {
            let result = future.await;
            if let Err(error) = &result {
                operation.record_failure();
                let mut slot = failed.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(error.clone());
                }
            }
            result
        });
        Ok(completed)
    }
    pub fn validate_complete(&self, expected: u32, completed: usize) -> Result<(), OperationError> {
        self.check()?;
        if !self.tasks.is_empty() || completed != expected as usize {
            return Err(OperationError::new(
                ErrorKind::Integrity,
                "part-scheduler",
                "not all planned parts completed",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopyPlan, UploadPlan};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    #[derive(Default)]
    struct Ledger {
        alive: AtomicUsize,
        peak: AtomicUsize,
        started: AtomicUsize,
    }
    struct Token(Arc<Ledger>);
    impl Token {
        fn new(ledger: &Arc<Ledger>) -> Self {
            let n = ledger.alive.fetch_add(1, Ordering::SeqCst) + 1;
            ledger.peak.fetch_max(n, Ordering::SeqCst);
            Self(ledger.clone())
        }
    }
    impl Drop for Token {
        fn drop(&mut self) {
            self.0.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[derive(Clone, Copy, Debug)]
    enum Plan {
        Upload(UploadPlan),
        Copy(CopyPlan),
    }
    impl Plan {
        fn parts(self) -> u32 {
            match self {
                Self::Upload(p) => p.parts,
                Self::Copy(p) => p.parts,
            }
        }
        fn part(self, n: u32) -> (u64, u64) {
            match self {
                Self::Upload(p) => p.part(n).unwrap(),
                Self::Copy(p) => p.part(n).unwrap(),
            }
        }
    }
    #[tokio::test(start_paused = true)]
    async fn production_scheduler_virtual_100gb_and_100gib_complete_fail_cancel_and_buffer_budget()
    {
        for bytes in [100_000_000_000, 100 * 1024 * 1024 * 1024] {
            for plan in [
                Plan::Upload(UploadPlan::new(bytes).unwrap()),
                Plan::Copy(CopyPlan::new(bytes).unwrap()),
            ] {
                for fault in [
                    None,
                    Some((1, false)),
                    Some((plan.parts() / 2, false)),
                    Some((plan.parts(), false)),
                    Some((1, true)),
                    Some((plan.parts() / 2, true)),
                    Some((plan.parts(), true)),
                ] {
                    let cancel = CancelContext::new();
                    let ledger = Arc::new(Ledger::default());
                    let assembly = Token::new(&ledger);
                    let mut scheduler = PartScheduler::new();
                    let mut count = 0;
                    let mut total = 0_u64;
                    let mut next = 1;
                    let mut completed = Vec::with_capacity(plan.parts() as usize);
                    // Tokens represent buffer ownership only. No body is allocated,
                    // generated, read, hashed, discarded, or sent in this test.
                    let transfer = async {
                        for number in 1..=plan.parts() {
                            cancel.check("virtual-reader")?;
                            let (offset, length) = plan.part(number);
                            assert_eq!(offset, total);
                            total += length;
                            let token = Token::new(&ledger);
                            let counters = ledger.clone();
                            let operation = cancel.clone();
                            let future = async move {
                                let _owned = token;
                                counters.started.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                if let Some((failed, cancelled)) = fault
                                    && failed == number
                                {
                                    if cancelled {
                                        operation.cancel();
                                    }
                                    return Err(OperationError::new(
                                        if cancelled {
                                            ErrorKind::Cancelled
                                        } else {
                                            ErrorKind::Unavailable
                                        },
                                        "fake-part-sender",
                                        "injected part failure",
                                    ));
                                }
                                Ok(number)
                            };
                            for result in scheduler.schedule(future, &cancel).await? {
                                completed.push(result.map_err(|_| {
                                    OperationError::new(
                                        ErrorKind::Unavailable,
                                        "fake",
                                        "join error",
                                    )
                                })??);
                            }
                            assert!(scheduler.len() <= 4);
                            next = number + 1;
                        }
                        while let Some(result) = scheduler.join_next().await {
                            completed.push(result.map_err(|_| {
                                OperationError::new(ErrorKind::Unavailable, "fake", "join error")
                            })??);
                        }
                        scheduler.validate_complete(plan.parts(), completed.len())?;
                        completed.sort_unstable();
                        for (n, number) in completed.iter().enumerate() {
                            assert_eq!(*number, n as u32 + 1);
                        }
                        assert_eq!(total, bytes);
                        count += 1;
                        Ok::<_, OperationError>(())
                    }
                    .await;
                    let mut aborts = 0;
                    if transfer.is_err() {
                        scheduler.abort_all();
                        while scheduler.join_next().await.is_some() {}
                        assert!(
                            scheduler
                                .validate_complete(plan.parts(), completed.len())
                                .is_err()
                        );
                        assert!(scheduler.is_empty());
                        aborts += 1;
                    }
                    assert_eq!(count, usize::from(fault.is_none()));
                    assert_eq!(aborts, usize::from(fault.is_some()));
                    if let Some((failed, _)) = fault {
                        assert!(
                            next <= failed + 5,
                            "new request after known failure: {next}/{failed}"
                        );
                    }
                    drop(assembly);
                    assert_eq!(ledger.alive.load(Ordering::SeqCst), 0);
                    assert!(ledger.peak.load(Ordering::SeqCst) <= 6);
                    assert_eq!(transfer.is_ok(), fault.is_none());
                    eprintln!(
                        "virtual {:?}: {bytes} bytes, {} parts, fault={fault:?}, issued={}, peak owned buffers={}",
                        plan,
                        plan.parts(),
                        ledger.started.load(Ordering::SeqCst),
                        ledger.peak.load(Ordering::SeqCst)
                    );
                }
            }
        }
    }
}
