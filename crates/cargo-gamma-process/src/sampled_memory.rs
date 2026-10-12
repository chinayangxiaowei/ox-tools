// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS process-group sampling, owned and stopped by the launch's boundary.

use core::panic::AssertUnwindSafe;
use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

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
        Self::start_with(request, move || footprint::members(group), footprint::read, kill)
    }

    fn start_with(
        request: MemoryRequest,
        mut members: impl FnMut() -> io::Result<Vec<i32>> + Send + 'static,
        mut footprint: impl FnMut(i32) -> io::Result<Option<Footprint>> + Send + 'static,
        kill: impl Fn() -> io::Result<()> + Send + 'static,
    ) -> Result<Self, PlatformError> {
        let (stop, stopped) = mpsc::channel();
        let state = Arc::new(Mutex::new(Sample::new(request.limit, request.calibrating)));
        let published = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("gamma-memory".into())
            .spawn(move || {
                let mut sample = Sample::new(request.limit, request.calibrating);
                loop {
                    let deadline = Instant::now() + SAMPLE_INTERVAL;
                    let result = sample.read(&mut members, &mut footprint, &kill);
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
                    // Charge reading time against the interval so extra group members do not
                    // add a full wait to the breach-overshoot bound.
                    match stopped.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(final_reading) => {
                            if final_reading && request.calibrating {
                                // The owner has not reaped the leader yet, so its lifetime maximum
                                // can recover a baseline peak that fell between timer samples.
                                let result = sample.read(&mut members, &mut footprint, &kill);
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
    calibrating: bool,
    usage: MemoryUsage,
    swept: bool,
}

impl Sample {
    fn new(limit: Option<u64>, calibrating: bool) -> Self {
        Self {
            limit,
            calibrating,
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
        let peak = if self.calibrating { current.max(lifetime_max) } else { current };
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

    fn gated_members(
        groups: Vec<Vec<i32>>,
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    ) -> impl FnMut() -> io::Result<Vec<i32>> + Send {
        let mut groups = groups.into_iter();
        let mut resume = Some(resume);
        move || {
            let group = groups.next().unwrap();
            if let Some(resume) = resume.take() {
                entered.send(()).unwrap();
                resume.recv().unwrap();
            }
            Ok(group)
        }
    }

    fn resume_on_stop(sampler: &mut Sampler, resume: mpsc::Sender<()>, final_reading: bool) -> JoinHandle<()> {
        let (stop, stopped) = mpsc::channel();
        let forward = core::mem::replace(&mut sampler.stop, stop);
        thread::spawn(move || {
            let requested = stopped.recv().unwrap();
            assert_eq!(requested, final_reading);
            // Queue the real stop request before releasing the first reading. No timer can
            // introduce an extra sample, regardless of how these threads are scheduled.
            forward.send(requested).unwrap();
            resume.send(()).unwrap();
        })
    }

    #[test]
    fn sampler_thread_exits_on_the_first_breach_and_sweeps_once() {
        let (killed, kills) = mpsc::channel();
        let mut groups = core::iter::once(vec![11]);
        let mut readings = core::iter::once(reading(101, 200));
        let mut sampler = Sampler::start_with(
            MemoryRequest {
                calibrating: false,
                meter: true,
                limit: Some(100),
            },
            move || Ok(groups.next().unwrap()),
            move |pid| {
                assert_eq!(pid, 11);
                Ok(Some(readings.next().unwrap()))
            },
            move || {
                killed.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();

        // Join without sending a stop request: the breach must end the worker by itself.
        sampler.worker.take().unwrap().0.join().unwrap().unwrap();
        assert_eq!(
            sampler.usage(),
            MemoryUsage {
                peak: Some(101),
                exhausted: true,
            }
        );
        assert!(sampler.swept());
        assert_eq!(kills.into_iter().collect::<Vec<_>>(), [()]);
    }

    #[test]
    fn sampler_thread_finish_recovers_lifetime_maxima_with_one_final_reading() {
        for limit in [None, Some(50)] {
            let (entered, started) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            let (read, reads) = mpsc::channel();
            let (killed, kills) = mpsc::channel();
            let mut readings = [reading(40, 40), reading(0, 100)].into_iter();
            let mut sampler = Sampler::start_with(
                MemoryRequest {
                    calibrating: true,
                    meter: true,
                    limit,
                },
                gated_members(vec![vec![11], vec![11]], entered, resumed),
                move |pid| {
                    read.send(pid).unwrap();
                    Ok(Some(readings.next().unwrap()))
                },
                move || {
                    killed.send(()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
            started.recv().unwrap();
            let relay = resume_on_stop(&mut sampler, resume, true);

            sampler.finish(true).unwrap();
            relay.join().unwrap();
            assert_eq!(reads.into_iter().collect::<Vec<_>>(), [11, 11]);
            assert_eq!(
                sampler.usage(),
                MemoryUsage {
                    peak: Some(100),
                    exhausted: false,
                }
            );
            assert!(!sampler.swept());
            assert_eq!(kills.into_iter().collect::<Vec<_>>(), []);
        }
    }

    #[test]
    fn sampler_thread_mutant_uses_current_only_and_takes_no_final_reading() {
        let (entered, started) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let (read, reads) = mpsc::channel();
        let (killed, kills) = mpsc::channel();
        let mut sampler = Sampler::start_with(
            MemoryRequest {
                meter: true,
                calibrating: false,
                limit: Some(100),
            },
            gated_members(vec![vec![11]], entered, resumed),
            move |pid| {
                read.send(pid).unwrap();
                Ok(Some(reading(40, 200)))
            },
            move || {
                killed.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        started.recv().unwrap();
        let relay = resume_on_stop(&mut sampler, resume, true);
        sampler.finish(true).unwrap();
        relay.join().unwrap();
        assert_eq!(reads.into_iter().collect::<Vec<_>>(), [11]);
        assert_eq!(
            sampler.usage(),
            MemoryUsage {
                peak: Some(40),
                exhausted: false
            }
        );
        assert!(!sampler.swept());
        assert_eq!(kills.into_iter().collect::<Vec<_>>(), []);
    }

    #[test]
    fn sampler_thread_reading_error_sweeps_and_is_returned_by_finish() {
        let (entered, started) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let (killed, kills) = mpsc::channel();
        let mut sampler = Sampler::start_with(
            MemoryRequest {
                calibrating: false,
                meter: true,
                limit: Some(100),
            },
            gated_members(vec![vec![11]], entered, resumed),
            |pid| {
                assert_eq!(pid, 11);
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
            move || {
                killed.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        started.recv().unwrap();
        sampler.check().unwrap();
        resume.send(()).unwrap();

        let error = sampler.finish(false).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(sampler.usage(), MemoryUsage::default());
        assert!(sampler.swept());
        assert_eq!(kills.into_iter().collect::<Vec<_>>(), [()]);
        sampler.check().unwrap();
    }

    #[test]
    fn sampler_thread_drop_joins_a_running_worker() {
        let (entered, started) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let (killed, kills) = mpsc::channel();
        let mut sampler = Sampler::start_with(
            MemoryRequest {
                calibrating: false,
                meter: true,
                limit: None,
            },
            gated_members(vec![vec![11]], entered, resumed),
            |pid| {
                assert_eq!(pid, 11);
                Ok(Some(reading(40, 40)))
            },
            move || {
                killed.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        started.recv().unwrap();
        let relay = resume_on_stop(&mut sampler, resume, false);

        drop(sampler);
        // The reader owns the last sender, so disconnection proves Drop waited for its release.
        assert_eq!(started.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        assert_eq!(kills.into_iter().collect::<Vec<_>>(), []);
        relay.join().unwrap();
    }

    #[test]
    fn crossing_the_aggregate_ceiling_sweeps_exactly_once_and_keeps_the_crossing_reading() {
        let mut state = Sample::new(Some(100), false);
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
        let mut state = Sample::new(None, false);
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
        let mut state = Sample::new(Some(100), false);
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
        let mut state = Sample::new(Some(100), false);
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
        for limit in [None, Some(50)] {
            let mut state = Sample::new(limit, true);
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
    }

    #[test]
    fn a_calibrating_launch_with_a_ceiling_enforces_current_and_records_lifetime_peaks() {
        let mut state = Sample::new(Some(50), true);
        let mut kills = 0;
        sample(&mut state, &[(11, Some(reading(20, 100)))], &mut kills);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(100),
                exhausted: false
            }
        );
        assert_eq!(kills, 0);
        sample(&mut state, &[(11, Some(reading(51, 200)))], &mut kills);
        assert_eq!(
            state.usage,
            MemoryUsage {
                peak: Some(200),
                exhausted: true
            }
        );
        assert!(state.swept);
        assert_eq!(kills, 1);
    }

    #[test]
    fn a_reading_failure_is_not_a_zero_measurement() {
        let mut state = Sample::new(None, false);
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
        let mut state = Sample::new(Some(100), false);
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
