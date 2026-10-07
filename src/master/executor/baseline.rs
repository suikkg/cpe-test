//! 起流前采样与双向同步。提前返回或 panic 也必须释放另一条腿。

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Condvar;

pub(super) struct BaselineGate {
    remaining: Mutex<usize>,
    ready: Condvar,
}

pub(super) struct BaselineParticipant<'a> {
    gate: &'a BaselineGate,
    arrived: AtomicBool,
}

impl BaselineGate {
    pub(super) fn new(legs: usize) -> Self {
        Self {
            remaining: Mutex::new(legs),
            ready: Condvar::new(),
        }
    }

    pub(super) fn participant(&self) -> BaselineParticipant<'_> {
        BaselineParticipant {
            gate: self,
            arrived: AtomicBool::new(false),
        }
    }
}

impl BaselineParticipant<'_> {
    fn arrive(&self) {
        if !self.arrived.swap(true, Ordering::SeqCst) {
            let mut remaining = lock_recover(&self.gate.remaining);
            *remaining = remaining.saturating_sub(1);
            self.gate.ready.notify_all();
        }
    }

    pub(super) fn wait(&self) {
        self.arrive();
        let mut remaining = lock_recover(&self.gate.remaining);
        while *remaining > 0 {
            remaining = self
                .gate
                .ready
                .wait(remaining)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

impl Drop for BaselineParticipant<'_> {
    fn drop(&mut self) {
        self.arrive();
    }
}

pub(super) fn background_wait_ms(cfg: &RateCheckCfg) -> u64 {
    if cfg.background_secs == 0 {
        return 0;
    }
    (cfg.background_secs.min(30) * 1_000).max(cfg.sample_interval_ms.clamp(200, 5_000) + 100)
}

impl Ctx {
    pub(super) fn collect_background(&self, enabled: bool, lifecycle: LifecycleLease<'_>) {
        let wait_ms = if enabled {
            background_wait_ms(&self.cfg.iperf.rate_check)
        } else {
            0
        };
        if wait_ms > 0 {
            logln(&format!("    网卡基线采样 {wait_ms}ms..."));
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(wait_ms)
                && !crate::cancel::is_cancelled()
            {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if let Some(participant) = lifecycle.baseline {
            participant.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_wait_covers_the_first_sample_and_can_be_disabled() {
        let mut cfg = RateCheckCfg::default();
        assert_eq!(background_wait_ms(&cfg), 3_000);
        cfg.sample_interval_ms = 5_000;
        assert_eq!(background_wait_ms(&cfg), 5_100);
        cfg.background_secs = 0;
        assert_eq!(background_wait_ms(&cfg), 0);
    }

    #[test]
    fn an_early_return_releases_the_other_leg() {
        let gate = BaselineGate::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let participant = gate.participant();
                participant.wait();
            });
            scope.spawn(|| {
                let _participant = gate.participant();
            });
        });
        assert_eq!(*lock_recover(&gate.remaining), 0);
    }

    #[test]
    fn a_panicking_leg_releases_its_peer() {
        let gate = BaselineGate::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let participant = gate.participant();
                participant.wait();
            });
            scope.spawn(|| {
                assert!(std::panic::catch_unwind(|| {
                    let _participant = gate.participant();
                    panic!("baseline setup failed");
                })
                .is_err());
            });
        });
        assert_eq!(*lock_recover(&gate.remaining), 0);
    }

    #[test]
    fn neither_leg_can_start_before_both_finish_the_background_phase() {
        let gate = BaselineGate::new(2);
        let first = gate.participant();
        let second = gate.participant();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                first.wait();
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            second.wait();
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
    }
}
