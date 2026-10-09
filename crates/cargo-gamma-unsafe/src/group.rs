// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Signalling a process group, and asking after one.
//!
//! A process group is how Unix says "this process and everything descended from it that has not
//! deliberately left". `std` has no way to signal one — [`std::process::Child::kill`] reaches the
//! one process it owns — so the group calls are made here and handed out as safe functions.

use core::mem::MaybeUninit;
use std::io;

/// Whether a child has exited, without reaping it.
///
/// `waitid` with `WNOWAIT` observes the exit while leaving the child as a zombie. Its pid and
/// process group therefore stay reserved until the caller has finished signalling that group and
/// explicitly reaps it with its [`std::process::Child`] handle.
///
/// # Errors
///
/// Returns the operating system's reason when the child cannot be observed. In particular, an
/// `ECHILD` means another waiter has already consumed the child, so callers must not assume its
/// numeric process-group id is still theirs.
pub fn exited(pid: u32) -> io::Result<bool> {
    let pid =
        libc::id_t::try_from(pid).map_err(|_out_of_range| io::Error::new(io::ErrorKind::InvalidInput, "child pid does not fit waitid"))?;
    let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();

    // SAFETY: `info` points to writable storage for exactly one `siginfo_t`; `P_PID` and `pid`
    // ask only about the caller's child; and these flags observe an exit without consuming it.
    let waited = unsafe { libc::waitid(libc::P_PID, pid, info.as_mut_ptr(), libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };

    if waited == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: a successful `waitid` initializes the supplied `siginfo_t`. POSIX specifies a zero
    // `si_pid` when `WNOHANG` found no state change, which is the only field read here.
    let info = unsafe { info.assume_init() };
    // SAFETY: `info` was initialized by the successful `waitid` above, so its returned pid can be
    // read through libc's accessor.
    let reported = unsafe { info.si_pid() };

    Ok(reported != 0)
}

/// Whether an observation failed because the caller has no matching child it can wait for.
///
/// `ECHILD` does not establish why no child can be waited for. A caller that previously owned a
/// matching child may interpret it as evidence that another waiter already reaped that child.
#[must_use]
pub fn is_no_child_to_wait_for(cause: &io::Error) -> bool {
    cause.raw_os_error() == Some(libc::ECHILD)
}

/// Kills every process in a group.
///
/// A group that has already exited is treated as successfully gone.
///
/// # Errors
///
/// Rejects identifiers less than or equal to one without making a system call. POSIX leaves
/// `killpg` undefined for those values, and Linux implements group one as the `kill(-1, signal)`
/// broadcast operation. Other errors are the operating system's reason the group could not be
/// signalled.
pub fn kill(group: i32) -> io::Result<()> {
    if group <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process-group identifiers must be greater than one",
        ));
    }

    // SAFETY: there is no memory-safety precondition to discharge, and that is why this is sound:
    // `killpg` is handed two integers and dereferences no caller pointer, so no value of `group` or
    // of the signal can make it read or write memory it should not. The description follows from
    // that — it names a group by id and delivers a signal — and its failure modes stay in-band: a
    // group that no longer exists yields `ESRCH` through the return value, never through anything
    // unsound. Whose group `group` names, and why the ordering around a reap matters, is a
    // correctness obligation on the caller — see `ProcessTree::observe` in
    // `cargo-gamma-process` — not a
    // soundness one, because a wrong id can only signal the wrong group, not corrupt this process.
    if unsafe { libc::killpg(group, libc::SIGKILL) } == 0 {
        return Ok(());
    }

    map_killpg_error(io::Error::last_os_error())
}

/// Kills a group after its leader has been observed exiting without being reaped.
///
/// macOS can return `EPERM` when the group contains only a zombie leader: its `killpg` path
/// excludes zombies and reports no signalable members as a permission error. The same error can
/// mean an inaccessible live member. Only a group listing containing exactly the observed,
/// unreaped leader proves the former case; every other result preserves the permission error.
/// The unreaped leader keeps the numeric group id tied to this subtree while it is inspected.
///
/// # Errors
///
/// Returns errors from [`kill`], including macOS `EPERM` unless the leader is the group's only
/// remaining member.
pub fn kill_after_exit(group: i32) -> io::Result<()> {
    let killed = kill(group);

    #[cfg(target_os = "macos")]
    if killed.as_ref().is_err_and(|cause| cause.raw_os_error() == Some(libc::EPERM)) && only_exited_leader_remains(group) {
        return Ok(());
    }

    killed
}

/// A two-entry snapshot suffices: a second member, or a sole member with another pid, prevents
/// treating `EPERM` as success. A failed or incomplete query also leaves the original error intact.
#[cfg(target_os = "macos")]
fn only_exited_leader_remains(group: i32) -> bool {
    let mut members: [libc::pid_t; 2] = [0; 2];
    let bytes =
        libc::c_int::try_from(core::mem::size_of_val(&members)).expect("two pid_t values always fit in the proc_listpgrppids byte count");

    // SAFETY: `members` is writable storage for two pid_t values, and `bytes` is its exact byte
    // length. The kernel writes no more than this length and returns the number of pids written.
    let listed = unsafe { libc::proc_listpgrppids(group, members.as_mut_ptr().cast(), bytes) };

    listed == 1 && members[0] == group
}

/// Treats an already-absent group as gone while preserving every other operating-system error.
fn map_killpg_error(cause: io::Error) -> io::Result<()> {
    if cause.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(cause)
    }
}

/// Whether a process with this id currently exists.
///
/// Test support for the process-tree tests in `cargo-gamma-process`, which need to watch a process
/// disappear. Signal zero is the POSIX spelling of "check, deliver nothing".
#[must_use]
pub fn exists(pid: i32) -> bool {
    // SAFETY: there is no memory-safety precondition to discharge — `kill` receives two integers
    // and dereferences no caller pointer, so every value of `pid` is a sound input. Given that,
    // signal zero delivers nothing and only performs the existence-and-permission check whose
    // answer is read back through the return value; a wrong pid can only answer about the wrong
    // process, which is a logic error rather than undefined behaviour.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The process group a process belongs to, or `None` if it could not be read.
///
/// Test support for the process-tree tests in `cargo-gamma-process`, which assert that a contained
/// child leads a group of its own rather than sharing this run's.
#[must_use]
pub fn group_of(pid: i32) -> Option<i32> {
    // SAFETY: there is no memory-safety precondition to discharge — `getpgid` receives one integer
    // and writes no caller memory, returning the group through its own value, so every value of
    // `pid` is a sound input. Because the result comes back in-band, failure is a plain `-1`, which
    // is checked below before the value is treated as a group id.
    let group = unsafe { libc::getpgid(pid) };

    // #[gamma::skip(relational.ge_to_gt, literal.int_increment, reason = "POSIX process-group ids are strictly positive and getpgid reports failure as -1, so zero can never distinguish these comparisons")]
    (group >= 0).then_some(group)
}

/// Raises `SIGINT` at this process.
///
/// Test support for the interrupt test in `cargo-gamma-process`, which has to be interrupted for real:
/// the thing under test is the signal handler [`super::interrupt`] installs, and nothing short of
/// an actual signal exercises it.
pub fn raise_interrupt() {
    // SAFETY: there is no memory-safety precondition to discharge — `raise` receives one integer
    // signal number and dereferences no caller pointer, so the call is sound whatever disposition
    // is installed. That the call may not return is a consequence of the disposition, which is the
    // very thing the interrupt test means to exercise: not returning is a control-flow outcome, not
    // unsoundness.
    let _raised = unsafe { libc::raise(libc::SIGINT) };
}

/// Raises `SIGQUIT` at this process.
///
/// Test support for the process-tree tests in `cargo-gamma-process`. Separate from
/// [`raise_interrupt`] rather than a signal number parameter, because these two exist to be
/// raised at the process running the test and that is not a thing to make general: `SIGQUIT` is
/// the terminal's *other* stop keystroke, and what the test asks is whether a run handles it or
/// dies of it and leaves its subtree behind.
///
/// Coverage excludes this helper because the uncaught signal terminates the subprocess before
/// instrumentation can flush its counters.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn raise_quit() {
    // SAFETY: as for `raise_interrupt` — `raise` receives one integer signal number, dereferences
    // no caller pointer, and is sound whatever disposition is installed. That it may not return is
    // the outcome under test, not unsoundness.
    let _raised = unsafe { libc::raise(libc::SIGQUIT) };
}

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::time::Duration;
    #[cfg(target_os = "macos")]
    use std::io::BufRead as _;
    #[cfg(target_os = "macos")]
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;
    #[cfg(target_os = "macos")]
    use std::process::Stdio;
    use std::thread;
    use std::time::Instant;

    use super::*;

    #[test]
    fn checking_existence_does_not_signal_the_process() {
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid fits");

        assert!(exists(pid));

        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            assert!(
                child.try_wait().expect("status").is_none(),
                "the existence check delivered a signal"
            );
            thread::sleep(Duration::from_millis(5));
        }

        child.kill().expect("cleanup");
        let _status = child.wait().expect("reap");
    }

    #[test]
    fn killing_a_non_group_identifier_is_rejected_without_signalling() {
        for group in [1, 0, -1, i32::MIN] {
            let error = kill(group).expect_err("an identifier that is unsafe for killpg must be refused");

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    /// A real group with a live member is killed outright, and a group that has already gone is
    /// treated as a success rather than an error — `ESRCH` is what the kernel reports for both "no
    /// such process" and "no such group", so a caller sweeping a group whose leader already exited
    /// must not be told that failed.
    #[test]
    fn killing_a_group_signals_it_once_and_treats_a_second_kill_as_already_gone() {
        use std::os::unix::process::CommandExt as _;

        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn a group leader");
        let group = i32::try_from(child.id()).expect("pid fits a group id");

        kill(group).expect("a live group can be killed");

        // A killed-but-unreaped leader is still a zombie, which `exists` still reports as present,
        // so it has to be reaped before the second kill can land on an empty group.
        let _status = child.wait().expect("reap");

        assert!(!exists(group), "the group's leader should have been reaped");

        kill(group).expect("a group that has already gone is reported as already gone, not an error");
    }

    /// A failure other than an absent group remains the real error rather than being reported as
    /// the "already gone" success reserved for `ESRCH`.
    #[test]
    fn a_killpg_permission_failure_is_preserved() {
        let error = map_killpg_error(io::Error::from_raw_os_error(libc::EPERM)).expect_err("a permission failure is not an absent group");

        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_group_with_another_member_is_not_a_zombie_only_group() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo ready; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("the shell starts as a process-group leader");
        let group = i32::try_from(child.id()).expect("a child pid fits a process-group identifier");
        let mut ready = String::new();
        let announced = std::io::BufReader::new(child.stdout.take().expect("stdout was piped above"))
            .read_line(&mut ready)
            .expect("the shell announces after its sleep child starts");
        let only_leader = only_exited_leader_remains(group);
        let killed = kill(group);
        let reaped = child.wait();

        assert!(announced > 0 && ready == "ready\n", "the child was not started: {ready:?}");
        assert!(!only_leader, "a live group member was mistaken for the sole exited leader");
        killed.expect("the test's live group can be terminated");
        reaped.expect("the test's group leader can be reaped");
    }

    /// A process id that is not this process's child cannot be observed for its exit, so the kernel
    /// reports `ECHILD` even though no competing waiter reaped it.
    #[test]
    fn observing_a_pid_that_is_not_this_process_own_child_fails() {
        // Pid 1 (init, or the container's own init process) is never this test's child.
        let error = exited(1).expect_err("pid 1 is not this process's child");

        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
        assert!(is_no_child_to_wait_for(&error));
        assert!(!is_no_child_to_wait_for(&io::Error::from_raw_os_error(libc::EPERM)));
    }
}
