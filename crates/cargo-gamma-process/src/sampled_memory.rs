// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS process-group sampling, owned and stopped by the launch's boundary.

use core::panic::AssertUnwindSafe;
use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use cargo_gamma_unsafe::footprint::{self, Footprint};
use cargo_gamma_unsafe::{PlatformError, Situation};

use crate::{MemoryRequest, MemoryUsage, SAMPLE_INTERVAL};

#[derive(Debug)]
pub(super) struct Sampler {
    stop: mpsc::Sender<bool>,
    // The handle is only consumed through an exclusive borrow. Unwinding leaves Drop responsible
    // for joining before the group is reaped; it cannot expose partially updated shared state.
    worker: Option<AssertUnwindSafe<JoinHandle<io::Result<()>>>>,
    state: Arc<Mutex<Sample>>,
}

impl Sampler {
    pub(super) fn start(
        group: i32,
        request: MemoryRequest,
        kill: impl Fn() -> io::Result<()> + Send + 'static,
    ) -> Result<Self, PlatformError> {
        let (stop, stopped) = mpsc::channel();
        let state = Arc::new(Mutex::new(Sample::new(request.limit)));
        let published = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("gamma-memory".into())
            .spawn(move || {
                let mut sample = Sample::new(request.limit);
                loop {
                    let result = sample.read(|| footprint::members(group), footprint::read, &kill);
                    if result.is_err() && !sample.usage.exhausted {
                        // A failed reading must not leave a launch running under a silently lost
                        // guard. A breach has already attempted its own sweep, even if it failed.
                        sample.swept = kill().is_ok();
                    }
                    *published
                        .lock()
                        .expect("only the sampler writes state and no code inside its lock can panic") = sample;
                    result?;
                    if sample.usage.exhausted {
                        return Ok(());
                    }
                    match stopped.recv_timeout(SAMPLE_INTERVAL) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(final_reading) => {
                            if final_reading && request.limit.is_none() {
                                // The owner has not reaped the leader yet, so its lifetime maximum
                                // can recover a baseline peak that fell between timer samples.
                                let result = sample.read(|| footprint::members(group), footprint::read, &kill);
                                *published
                                    .lock()
                                    .expect("only the sampler writes state and no code inside its lock can panic") = sample;
                                result?;
                            }
                            return Ok(());
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            })
            .map_err(|cause| PlatformError::because_static(Situation::Refused, "the macOS memory sampler could not be started", cause))?;
        Ok(Self {
            stop,
            worker: Some(AssertUnwindSafe(worker)),
            state,
        })
    }

    pub(super) fn usage(&self) -> MemoryUsage {
        self.state
            .lock()
            .expect("only the sampler writes state and no code inside its lock can panic")
            .usage
    }

    pub(super) fn swept(&self) -> bool {
        self.state
            .lock()
            .expect("only the sampler writes state and no code inside its lock can panic")
            .swept
    }

    pub(super) fn check(&mut self) -> io::Result<()> {
        if self.worker.as_ref().is_some_and(|worker| worker.is_finished()) {
            self.finish(false)?;
        }
        Ok(())
    }

    /// Joins before the owner releases any numeric process identity.
    pub(super) fn finish(&mut self, final_reading: bool) -> io::Result<()> {
        if let Some(worker) = self.worker.take() {
            let _stopped = self.stop.send(final_reading);
            worker
                .0
                .join()
                .expect("the sampler thread contains no panicking application callbacks")?;
        }
        Ok(())
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let _stopped = self.finish(false);
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    limit: Option<u64>,
    usage: MemoryUsage,
    swept: bool,
}

impl Sample {
    fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            usage: MemoryUsage::default(),
            swept: false,
        }
    }

    fn read(
        &mut self,
        members: impl FnOnce() -> io::Result<Vec<i32>>,
        mut footprint: impl FnMut(i32) -> io::Result<Option<Footprint>>,
        kill: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        if self.usage.exhausted {
            return Ok(());
        }
        let mut current = 0_u64;
        let mut lifetime_max = 0_u64;
        for pid in members()? {
            if let Some(reading) = footprint(pid)? {
                current = current.saturating_add(reading.current);
                lifetime_max = lifetime_max.saturating_add(reading.lifetime_max);
            }
        }
        // Baseline calibration conservatively includes each listed member's lifetime maximum.
        // Enforcement uses only current aggregate footprint, never a sum of historical peaks.
        let peak = if self.limit.is_none() { current.max(lifetime_max) } else { current };
        self.usage.peak = Some(self.usage.peak.unwrap_or(0).max(peak));
        if self.limit.is_some_and(|limit| current > limit) {
            self.usage.exhausted = true;
            kill()?;
            self.swept = true;
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn reading(current: u64, lifetime_max: u64) -> Footprint {
        Footprint { current, lifetime_max }
    }

    fn sample(state: &mut Sample, readings: &[(i32, Option<Footprint>)], kills: &mut usize) {
        let mut expected = readings.iter();
        state
            .read(
                || Ok(readings.iter().map(|(pid, _)| *pid).collect()),
                |pid| {
                    let (expected_pid, usage) = expected.next().unwrap();
                    assert_eq!(pid, *expected_pid);
                    Ok(*usage)
                },
                || {
                    *kills += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(expected.next(), None);
    }

    #[test]
    fn crossing_the_aggregate_ceiling_sweeps_exactly_once_and_keeps_the_crossing_reading() {
        let mut state = Sample::new(Some(100));
        let mut kills = 0;
        sample(
            &mut state,
            &[(11, Some(reading(60, 200))), (12, Some(reading(40, 100)))],
            &mut kills,
        );
        assert_eq!(kills, 0);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(100),
                exhausted: false
            }
        );
        sample(
            &mut state,
            &[(11, Some(reading(60, 200))), (12, Some(reading(41, 100)))],
            &mut kills,
        );
        assert_eq!(kills, 1);
        assert!(state.swept);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(101),
                exhausted: true
            }
        );
        state
            .read(
                || panic!("a breached sampler must stop reading"),
                |_| panic!("unexpected read"),
                || panic!("second kill"),
            )
            .unwrap();
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(101),
                exhausted: true
            }
        );
    }

    #[test]
    fn peak_is_the_maximum_across_samples_and_measurement_never_kills() {
        let mut state = Sample::new(None);
        let mut kills = 0;
        for current in [40, 90, 20] {
            sample(&mut state, &[(11, Some(reading(current, current)))], &mut kills);
        }
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(90),
                exhausted: false
            }
        );
        assert_eq!(kills, 0);
    }

    #[test]
    fn vanished_members_are_skipped_without_discarding_live_members() {
        let mut state = Sample::new(Some(100));
        let mut kills = 0;
        sample(
            &mut state,
            &[(11, Some(reading(30, 30))), (12, None), (13, Some(reading(40, 40)))],
            &mut kills,
        );
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(70),
                exhausted: false
            }
        );
        assert_eq!(kills, 0);
    }

    #[test]
    fn an_inaccessible_member_is_skipped_while_readable_members_can_still_breach() {
        let mut state = Sample::new(Some(100));
        let mut kills = 0;
        // The footprint wrapper returns None for EPERM, just as it does for a vanished member.
        sample(
            &mut state,
            &[(11, Some(reading(30, 30))), (12, None), (13, Some(reading(40, 40)))],
            &mut kills,
        );
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(70),
                exhausted: false
            }
        );
        assert_eq!(kills, 0);
        assert!(!state.swept);

        sample(
            &mut state,
            &[(11, Some(reading(60, 60))), (12, None), (13, Some(reading(41, 41)))],
            &mut kills,
        );
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(101),
                exhausted: true
            }
        );
        assert_eq!(kills, 1);
        assert!(state.swept);
    }

    #[test]
    fn baseline_recovers_unreaped_members_lifetime_peaks() {
        let mut state = Sample::new(None);
        let mut kills = 0;
        sample(&mut state, &[(11, Some(reading(0, 70))), (12, Some(reading(20, 30)))], &mut kills);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(100),
                exhausted: false
            }
        );
        assert_eq!(kills, 0);
    }

    #[test]
    fn a_reading_failure_is_not_a_zero_measurement() {
        let mut state = Sample::new(None);
        let error = state
            .read(
                || Ok(vec![11]),
                |_| Err(io::Error::from_raw_os_error(libc::EIO)),
                || panic!("unexpected kill"),
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(state.usage, MemoryUsage::default());
    }

    #[test]
    fn a_failed_breach_sweep_does_not_claim_completed_cleanup_or_repeat_the_kill() {
        let mut state = Sample::new(Some(100));
        let mut kills = 0;
        let error = state
            .read(
                || Ok(vec![11]),
                |_| Ok(Some(reading(101, 101))),
                || {
                    kills += 1;
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(101),
                exhausted: true
            }
        );
        assert!(!state.swept);
        state
            .read(
                || panic!("breach already recorded"),
                |_| panic!("unexpected read"),
                || panic!("second kill"),
            )
            .unwrap();
        assert_eq!(kills, 1);
    }
}
