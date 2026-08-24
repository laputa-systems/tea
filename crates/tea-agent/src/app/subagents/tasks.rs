//! Smol-backed structured task ownership for optional child lanes.
//!
//! The supervisor retains every [`SmolTaskHandle`] it receives. This runtime
//! deliberately never detaches a task: a dropped handle requests cancellation
//! through the wrapper and drops the concrete Smol task rather than letting an
//! unowned child outlive the supervisor.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tea_core::runtime::{SubagentTaskError, TaskHandle, TaskRuntime};
use tea_core::scheduler::CancellationToken;

/// Concrete Smol executor port installed by the terminal host.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SmolTaskRuntime;

/// One terminal-owned child task. The only concrete Smol task remains inside
/// this handle until exactly one joiner consumes it.
pub(crate) struct SmolTaskHandle {
    cancellation: CancellationToken,
    state: smol::lock::Mutex<SmolTaskState>,
}

struct SmolTaskState {
    task: Option<smol::Task<()>>,
    settled: bool,
}

impl std::fmt::Debug for SmolTaskHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let settled = self.state.try_lock().map(|state| state.settled);
        formatter
            .debug_struct("SmolTaskHandle")
            .field("cancelled", &self.cancellation.is_cancelled())
            .field("settled", &settled)
            .finish_non_exhaustive()
    }
}

impl SmolTaskRuntime {
    /// Construct the terminal's structured child-task executor port.
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl TaskRuntime for SmolTaskRuntime {
    fn spawn(
        &self,
        _name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<Arc<dyn TaskHandle>, SubagentTaskError> {
        let cancellation = CancellationToken::new();
        let cancellation_wait = cancellation.clone();
        // Dropping the losing `or` branch drops the caller's child-operation
        // future. The outer Smol task remains owned by the returned handle,
        // so no cancellation or normal completion path detaches child work.
        let task = smol::spawn(async move {
            smol::future::or(task, async move { cancellation_wait.cancelled().await }).await;
        });
        Ok(Arc::new(SmolTaskHandle {
            cancellation,
            state: smol::lock::Mutex::new(SmolTaskState {
                task: Some(task),
                settled: false,
            }),
        }))
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            smol::Timer::after(duration).await;
        })
    }
}

impl TaskHandle for SmolTaskHandle {
    fn cancel(&self) {
        // `CancellationToken` is explicitly idempotent. The wrapper below
        // races this signal with the child future, so cancellation remains
        // available even after another caller has begun joining the task.
        self.cancellation.cancel();
    }

    fn join<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // The guard intentionally lives through `task.await`: an
            // abandoned join releases only the guard, never the concrete
            // task. A later join then resumes terminal observation from the
            // same owned task.
            let mut state = self.state.lock().await;
            if state.settled {
                return;
            }

            {
                let task = state
                    .task
                    .as_mut()
                    .expect("an unsettled task handle retains its Smol task");
                task.await;
            }
            drop(state.task.take());
            state.settled = true;
        })
    }
}

impl Drop for SmolTaskHandle {
    fn drop(&mut self) {
        // If the coordinator releases its final handle without joining, take
        // the concrete task through Smol's explicit cancellation path. Merely
        // dropping a `smol::Task` marks it detached after cancellation, which
        // would leave terminal cleanup unobserved. `Task::cancel` instead
        // waits until the wrapper has dropped its child future.
        self.cancellation.cancel();
        let task = self.state.get_mut().task.take();
        if let Some(task) = task {
            let _ = smol::block_on(task.cancel());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    #[test]
    fn join_observes_one_task_once_and_wakes_all_joiners() {
        let runtime = SmolTaskRuntime::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let task_runs = Arc::clone(&runs);
        let handle = runtime
            .spawn(
                "one",
                Box::pin(async move {
                    task_runs.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .expect("Smol accepts a task");

        let first = handle.join();
        let second = handle.join();
        smol::block_on(async {
            smol::future::zip(first, second).await;
        });
        smol::block_on(handle.join());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancellation_is_idempotent_and_settles_an_active_task() {
        let runtime = SmolTaskRuntime::new();
        let (started_sender, started_receiver) = smol::channel::bounded(1);
        let handle = runtime
            .spawn(
                "pending",
                Box::pin(async move {
                    started_sender.send(()).await.expect("receiver is retained");
                    std::future::pending::<()>().await;
                }),
            )
            .expect("Smol accepts a task");

        smol::block_on(async {
            started_receiver.recv().await.expect("task starts");
        });
        handle.cancel();
        handle.cancel();
        smol::block_on(handle.join());
    }

    #[test]
    fn dropped_in_flight_join_leaves_the_task_for_a_later_join() {
        let runtime = SmolTaskRuntime::new();
        let (started_sender, started_receiver) = smol::channel::bounded(1);
        let (release_sender, release_receiver) = smol::channel::bounded(1);
        let handle = runtime
            .spawn(
                "join-retry",
                Box::pin(async move {
                    started_sender.send(()).await.expect("receiver is retained");
                    release_receiver.recv().await.expect("sender is retained");
                }),
            )
            .expect("Smol accepts a task");
        smol::block_on(async {
            started_receiver.recv().await.expect("task starts");
        });

        let first_join = handle.join();
        assert_eq!(smol::block_on(smol::future::poll_once(first_join)), None);
        smol::block_on(async {
            release_sender.send(()).await.expect("task remains owned");
        });
        smol::block_on(handle.join());
    }

    #[test]
    fn dropping_the_final_handle_cancels_its_owned_task() {
        struct PendingDropSignal {
            started: Option<smol::channel::Sender<()>>,
            dropped: Arc<AtomicBool>,
        }

        impl Future for PendingDropSignal {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
                if let Some(started) = self.started.take() {
                    started.try_send(()).expect("receiver is retained");
                }
                Poll::Pending
            }
        }

        impl Drop for PendingDropSignal {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let runtime = SmolTaskRuntime::new();
        let (started_sender, started_receiver) = smol::channel::bounded(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let handle = runtime
            .spawn(
                "drop-owned-task",
                Box::pin(PendingDropSignal {
                    started: Some(started_sender),
                    dropped: task_dropped,
                }),
            )
            .expect("Smol accepts a task");
        smol::block_on(async {
            started_receiver.recv().await.expect("task starts");
        });

        drop(handle);
        assert!(
            dropped.load(Ordering::SeqCst),
            "dropping supervisor ownership drops the child future rather than detaching it"
        );
    }
}
