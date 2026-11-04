//! Task supervision and a clean SIGTERM.
//!
//! Two rules, both of them lessons rather than preferences:
//!
//! - **A failed task restarts; it never takes the process down.** The broker loop dies when
//!   mosquitto restarts and the health poll fails while ChirpStack migrates its database on
//!   first boot. Neither is a reason to stop decoding uplinks, and neither is something
//!   `Restart=on-failure` should be answering — that policy exists for a config this daemon
//!   cannot use at all.
//! - **Shutdown is cooperative and observable.** A task that is asleep between health ticks
//!   must wake on SIGTERM rather than hold the stop for up to a minute, so every wait in this
//!   process goes through [`Shutdown::sleep`] instead of `tokio::time::sleep`.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{BACKOFF_MAX, BACKOFF_MIN};

/// A shutdown signal every task shares.
///
/// A `watch` channel and not a `Notify`: `Notify::notify_waiters` only wakes tasks that are
/// *already* waiting, so a task that reached its wait a moment after SIGTERM would sleep
/// through the shutdown it was told about. A watch carries the state, so arriving late is
/// harmless.
#[derive(Clone)]
pub struct Shutdown {
    tx: watch::Sender<bool>,
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Shutdown { tx }
    }

    /// `send_replace`, not `send`: `send` reports an error and **leaves the value unchanged**
    /// when no receiver is currently subscribed, and receivers here are created on demand by
    /// [`Shutdown::wait`]. A SIGTERM that arrived while every task happened to be running
    /// rather than waiting would otherwise be dropped on the floor.
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolve once shutdown has been asked for. Immediate if it already has.
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Sleep, unless shutdown arrives first.
    pub async fn sleep(&self, d: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = self.wait() => {}
        }
    }

    /// Trigger on SIGTERM or SIGINT.
    ///
    /// SIGTERM is what systemd sends on `stop` and on the restart half of a redeploy; SIGINT is
    /// what an engineer running this in a terminal sends. Both mean the same thing, and a
    /// handler that could not be installed is logged rather than fatal — a daemon that refuses
    /// to start because it cannot listen for its own shutdown is worse than one that has to be
    /// killed.
    pub async fn wait_for_signal(&self) {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("proximos-control: cannot listen for SIGTERM: {e}");
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("proximos-control: cannot listen for SIGINT: {e}");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => eprintln!("proximos-control: SIGTERM — shutting down"),
            _ = int.recv() => eprintln!("proximos-control: SIGINT — shutting down"),
        }
        self.trigger();
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::new()
    }
}

/// A supervised task.
pub struct Task {
    pub handle: JoinHandle<()>,
}

/// Run `factory()` in a loop until shutdown, backing off between attempts.
///
/// The factory is called afresh each time rather than the future being retried, because a
/// broker connection cannot be resumed — the restart has to rebuild it.
pub fn spawn<F, Fut>(name: &'static str, shutdown: Shutdown, factory: F) -> Task
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = control_radio::Result<()>> + Send,
{
    let handle = tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN;
        while !shutdown.is_shutting_down() {
            match factory().await {
                // A task that returned without being asked to is unexpected but not fatal;
                // it goes round the same backoff rather than leaving a duty unattended.
                Ok(()) if shutdown.is_shutting_down() => break,
                Ok(()) => eprintln!("proximos-control: the {name} task ended; restarting"),
                Err(e) => eprintln!("proximos-control: the {name} task failed: {e}"),
            }
            if shutdown.is_shutting_down() {
                break;
            }
            shutdown.sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    });
    Task { handle }
}

/// Double, capped. Deliberately not jittered: there is exactly one of this process per Hub and
/// everything it retries against is on loopback, so the thundering-herd problem jitter solves
/// does not exist here — and a predictable ladder is easier to read in a boot log.
pub fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > BACKOFF_MAX {
        BACKOFF_MAX
    } else {
        doubled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime")
    }

    /// The ladder is 1, 2, 4 … 30 and then stays there: fast enough to catch mosquitto coming
    /// up on first boot, slow enough not to spin against a ChirpStack that is still migrating.
    #[test]
    fn the_backoff_doubles_to_a_ceiling_and_stays_there() {
        let mut d = BACKOFF_MIN;
        let mut ladder = vec![d];
        for _ in 0..8 {
            d = next_backoff(d);
            ladder.push(d);
        }
        assert_eq!(
            ladder.iter().map(|d| d.as_secs()).collect::<Vec<_>>(),
            vec![1, 2, 4, 8, 16, 30, 30, 30, 30]
        );
        assert_eq!(next_backoff(BACKOFF_MAX), BACKOFF_MAX);
    }

    /// A watch and not a `Notify`, so a task that starts waiting *after* the signal still sees
    /// it. This is the bug the type note describes, asserted.
    #[test]
    fn shutdown_is_visible_to_a_task_that_arrives_late() {
        rt().block_on(async {
            let s = Shutdown::new();
            s.trigger();
            assert!(s.is_shutting_down());
            // Would hang forever with `Notify::notify_waiters`.
            tokio::time::timeout(Duration::from_secs(1), s.wait())
                .await
                .expect("a late waiter still resolves");
        });
    }

    /// A task asleep between health ticks must wake on SIGTERM, not hold the stop for a
    /// minute.
    #[test]
    fn a_sleeping_task_wakes_on_shutdown_rather_than_serving_out_its_timer() {
        rt().block_on(async {
            let s = Shutdown::new();
            let sleeper = {
                let s = s.clone();
                tokio::spawn(async move { s.sleep(Duration::from_secs(3600)).await })
            };
            tokio::time::sleep(Duration::from_millis(10)).await;
            s.trigger();
            tokio::time::timeout(Duration::from_secs(1), sleeper)
                .await
                .expect("the sleep is cut short")
                .expect("no panic");
        });
    }

    /// A failing task restarts on its own backoff and never takes the process down.
    #[test]
    fn a_failing_task_is_restarted_until_shutdown() {
        rt().block_on(async {
            let s = Shutdown::new();
            let runs = Arc::new(AtomicUsize::new(0));
            let task = {
                let runs = runs.clone();
                spawn("test", s.clone(), move || {
                    let runs = runs.clone();
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        Err(control_radio::Error::new(
                            control_radio::Kind::ServiceDown,
                            None,
                            control_radio::Component::MqttBroker,
                            "connection refused",
                        ))
                    }
                })
            };
            // Virtual time: the runtime fast-forwards through the backoff ladder.
            tokio::time::sleep(Duration::from_secs(20)).await;
            s.trigger();
            tokio::time::timeout(Duration::from_secs(60), task.handle)
                .await
                .expect("the supervisor stops")
                .expect("no panic");

            let n = runs.load(Ordering::SeqCst);
            assert!(n >= 4, "it kept retrying: {n} attempts");
            assert!(n < 20, "and backed off rather than spinning: {n} attempts");
        });
    }

    /// Shutdown wins over the restart: a stop must not be delayed by one more attempt.
    #[test]
    fn a_task_is_not_restarted_after_shutdown() {
        rt().block_on(async {
            let s = Shutdown::new();
            let runs = Arc::new(AtomicUsize::new(0));
            s.trigger();
            let task = {
                let runs = runs.clone();
                spawn("test", s.clone(), move || {
                    let runs = runs.clone();
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
            };
            tokio::time::timeout(Duration::from_secs(5), task.handle)
                .await
                .expect("stops immediately")
                .expect("no panic");
            assert_eq!(
                runs.load(Ordering::SeqCst),
                0,
                "nothing was started after shutdown was asked for"
            );
        });
    }
}
