# cargo-gamma-process — Sampled memory control on macOS

> Status: **Proposed**.
> Companion to the [crate design](DESIGN.md).

## Summary

Linux and Windows bound a test subtree's memory with a kernel boundary (a
cgroup leaf, a job object). macOS has no unprivileged equivalent, so today
`--memory enforce` silently degrades to `off` there, and a mutant that turns
bounded allocation into unbounded allocation runs until the wall-clock timeout.
On a developer machine that is long enough to exhaust physical and compressed
memory and take every other application down with it.

This document proposes closing that gap with **sampled enforcement**: on
macOS, cargo-gamma samples the physical footprint of every process in the
launch's process group at a short fixed interval, and kills the group when the
sum passes the ceiling the run already computes. Everything above the
mechanism is reused unchanged: the baseline-derived ceiling, the
`MemoryPolicy` arithmetic, the `outofmem` outcome, its note and its
suppression eligibility.

The bound is soft: a runaway can overshoot the ceiling by roughly its growth
rate times the sampling interval, and a descendant that leaves the process
group is not counted. Both limits are stated to the user rather than hidden.

## Motivating incident

The incident below happened in a downstream workspace with a different
mutation tool (cargo-mutants 27.1.0), before cargo-gamma was in use there.
It is recorded here because cargo-gamma on macOS has exactly the same
exposure: its timeout is the only guard.

### The code and the mutants

A tree type walks from a node to the root. Reduced from the original:

```rust
fn ancestor_chain(&self, id: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut cur = id.to_owned();
    while let Some(p) = self.parent_of(&cur) {
        chain.push(p.clone());
        cur = p;
    }
    chain
}
```

The loop terminates only because `parent_of` eventually returns `None` at the
root. Three mutants removed that guarantee:

| Mutant | Effect |
|---|---|
| `parent_of` → `Some(String::new())` | every node, including `""`, has a parent |
| `parent_of` → `Some("xyzzy".into())` | every node, including `"xyzzy"`, has a parent |
| `==` → `!=` in the parent lookup | the lookup finds some non-matching node as parent, forever |

Each turns the loop into an unbounded `push`. The vector doubles its capacity
as it grows, so the process touches new memory as fast as it can copy. Seven
tests exercised the walk and ran in parallel in one test binary; together they
grew at roughly 5–9 GB/s.

### What happened to the machine

The run used `--timeout 180`. At 5 GB/s, 180 seconds is about 900 GB; the
machine had 64 GB. The timeout is the wrong unit for this failure: memory runs
out within seconds, minutes before the timeout fires.

macOS does not fail the allocation. It compresses memory, then swaps, then, when
compressor space runs out, jetsam kills processes by priority. Background
applications go first and the foreground terminal last. That ordering matches
what the user saw: every application closed except the terminal.

Five incidents across two days involved the same three mutants:

| When (PDT) | Test-process footprint | Consequence |
|---|---|---|
| 10-05 17:48 | ~121 GB | jetsam, mass application kill |
| 10-05 17:59 | ~109 GB | jetsam, mass application kill |
| 10-06 09:12 | unknown | kernel watchdog panic, "100% of compressed pages limit", reboot |
| 10-06 09:39 | ~90 GB | jetsam, `vm-compressor-space-shortage`, ~12.6k kills |
| 10-06 09:49 | ~96 GB | jetsam, `vm-compressor-space-shortage`, ~10.1k kills |

The footprint figures come from the jetsam reports in
`/Library/Logs/DiagnosticReports`. The kill counts include system daemons that
launchd relaunched and jetsam killed again. The figures exceed physical memory
because they count compressed pages, which is why resident size alone would
have misread the situation.

At 09:48:26 the tool's timeout terminated the `cargo` process it had started.
The jetsam snapshot at 09:48:59 still lists the grandchild test binary alive at
about 94 GB. Terminating the direct child did not terminate the process that
held the memory.

### What the incident establishes

1. **A time budget cannot bound memory.** Growth of several GB/s exhausts a
   workstation in seconds; any timeout long enough for a healthy suite is
   orders of magnitude too long.
2. **The boundary has to be the process tree, not the direct child.** The
   allocating process was a grandchild.
3. **The metric has to include compressed memory.** Resident size shrinks as
   macOS compresses, so a resident-size guard reads lower exactly when the
   machine is in most danger.
4. **The risk is generic, not mutator-specific.** Nothing about these mutants
   is special: any mutant that breaks a loop's termination while the loop
   allocates does this. A guard tied to one mutator family does not cover it.

The downstream project has since wrapped these tests in a 500 ms in-test
deadline, which cuts the peak to about 5.4 GB. That is good test hygiene, but it
protects only the tests that were wrapped. It is bounded by time rather than
memory, and the abandoned thread keeps allocating until the test binary exits.
The guard belongs in the tool that generates the mutants.

## What macOS offers

Each candidate was checked on macOS 15.7.2, arm64, 64 GB.

| Mechanism | Result |
|---|---|
| `setrlimit(RLIMIT_AS / RLIMIT_DATA / RLIMIT_RSS)` | Rejected with `EINVAL` when lowered from infinity, as a soft-only or a soft-and-hard limit. A 2 GB allocation then succeeds. In the same process, `RLIMIT_NOFILE` and `RLIMIT_STACK` lower normally. These limits are also per process, not per tree. |
| cgroups, job objects | Do not exist. |
| Kernel per-process memory limits (memorystatus) | Private interfaces, not a supported API for an unprivileged tool. |
| Running tests in a VM or container | Changes the platform under test and costs far more than the protection is worth. |
| Sampling from the supervisor | Works unprivileged: `proc_listpids(PROC_PGRP_ONLY, pgid)` lists a group's members, and `proc_pid_rusage(pid, RUSAGE_INFO_V4)` reads each member's `ri_phys_footprint`. |

`ri_phys_footprint` is the metric jetsam and Activity Monitor use. It includes
compressed memory, so it does not fall while the machine compresses.

## Design

### Mechanism

For each launch on macOS whose memory request is measuring, a sampler owned by
the launch's boundary does the following every `SAMPLE_INTERVAL`:

1. Lists the members of the launch's process group.
2. Sums their `ri_phys_footprint`.
3. Records the sum as the running peak.
4. If enforcing and the sum exceeds the ceiling, kills the group and records
   the reading that crossed it.

The kill reuses the existing process-group sweep, including the macOS
zombie-leader handling. The verdict reuses `outofmem` and its note: the binary,
the peak reached, and the ceiling passed.

The baseline uses the same sampler to measure the peak that calibrates the
ceiling, so the default `--memory enforce` becomes available on macOS without
any new configuration.

### Sampling interval

`SAMPLE_INTERVAL` is a fixed 10 ms constant, not a user setting. A prototype
measured the overshoot against a runaway growing faster than the incident's:

| Interval | 1 process, ~12 GB/s | 4 processes, ~40 GB/s |
|---|---|---|
| **10 ms** | **+47 MB**, stopped at 0.13 s | **+96 MB**, stopped at 0.04 s |
| 50 ms | +360 MB | +801 MB |
| 100 ms | +887 MB | +2,950 MB |

The overshoot is about the growth rate times the interval. At 10 ms it is well
inside the default 128 MiB headroom. That compares with more than 90 GB in the
incident. One sample costs one listing call plus one usage call per group
member, which is negligible next to running a test binary.

### Stated limits

- **Soft bound.** The overshoot above is inherent. The `outofmem` note reports
  the reading that crossed the ceiling, which can exceed the ceiling.
- **Group escape.** A descendant that calls `setsid` or `setpgid` leaves the
  group and is neither counted nor killed. This is the same best-effort
  containment macOS already has for termination. It does not newly weaken
  anything.
- **Unreadable members.** An unprivileged reader gets `EPERM` for a process
  owned by another user. For example, a test may launch a setuid tool such as
  `top`, `sudo` or `crontab`. Such a member is skipped like a vanished one, so
  the bound covers what this user can read. Stopping the launch instead would
  make every such suite unjudged on macOS. Before this design, those suites ran
  normally. Any other reading failure, including a failure to list the group,
  means the guard is lost. That stops the launch, and it is reported as unjudged
  rather than as a measurement.
- **Short-lived peaks.** A process that peaks and exits between two samples is
  not seen. This is acceptable for runaway detection, because a runaway is by
  definition still alive and still growing. It matters only for baseline
  calibration, where an underestimated peak would tighten the ceiling.
  The headroom and multiplier absorb it. When measuring without a ceiling, as
  the baseline does, each sample also sums the members'
  `ri_lifetime_max_phys_footprint`, with one final reading taken after the
  leader exits and before it is reaped. On macOS 15.7.2, an exited, unreaped
  process still reports its lifetime maximum, while its current footprint reads
  zero. This sum can exceed the true simultaneous peak, which errs toward a
  looser ceiling. Enforcement compares only the current footprint, never
  historical maxima.

### Support and demand

On macOS, `support()` reports that memory control is available in its sampled
form, instead of reporting unsupported. Consequences:

- Inherited `--memory enforce` now enforces on macOS rather than degrading to
  `off`. The run states once that enforcement is sampled, so the user is not led
  to believe that a kernel boundary exists.
- A stated `--memory enforce` or `--memory-limit` is honored on macOS instead of
  failing the run.
- `--memory off` remains the user's explicit choice of no control.

The existing contract says that support is claimed only where it exists. It is
kept by naming the kind of support: kernel-enforced, or sampled.

### Where the code goes

| Crate | Change | Reason |
|---|---|---|
| `cargo-gamma-unsafe` | Safe wrappers for `proc_listpids` and `proc_pid_rusage` | FFI belongs here; every other crate forbids unsafe code |
| `cargo-gamma-process` | The macOS sampler behind the existing accounting and ceiling interfaces | This crate already owns memory accounting for Linux and Windows |
| `cargo-gamma-lib` | Rewording of the memory module's rationale; the run-start note naming sampled enforcement | Policy is unchanged; only the description of the platform changes |

This design changes one existing rationale. The memory module says that polling
is not used because it describes one process and races short-lived
descendants. That remains true where a kernel boundary exists. On macOS, no such
boundary exists. Polling the whole process group is the strongest available
guard, and the race affects only processes that are not runaways.

### Testing

Following the repository's test-review rules, unit tests do not spawn
processes:

- The sampler is driven by an injected reading source. Tests cover the
  following: a sum crossing the ceiling kills exactly once; the peak tracks the
  maximum across samples; members that disappear between listing and reading
  are skipped, not treated as errors; and a measuring but not enforcing policy
  never kills.
- A single macOS-only integration test, ignored by default and failing when
  requested by name on a host that cannot run it, launches a real runaway
  allocator under a small explicit ceiling. It asserts that the outcome is
  `outofmem` and that the recorded peak is within a stated bound of the
  ceiling.

## Relation to reference-value mutants

The leak-specific limiting proposed in PR #243 was a response to this same gap,
seen from one mutator: a `Box::leak` replacement on an unmetered host. With
sampled enforcement, macOS is no longer unmetered. A leaking mutant is held to
the same ceiling as every other mutant on every supported platform. A
per-mutator counter would not have stopped the incident above, while this
design does.

## Out of scope

- Sealed containment on macOS, which would need a boundary a descendant cannot
  leave. No unprivileged mechanism exists.
- Host-wide memory pressure policy, such as pausing workers when the machine is
  low on memory regardless of ceilings.
- Changing the ceiling arithmetic, the `outofmem` outcome, or its suppression
  defaults.
