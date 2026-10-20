//! Mirror the host audio thread's scheduling policy onto the DSP worker.
//!
//! Fork at plugin init runs from the main thread (`SCHED_OTHER`). REAPER (and
//! similar hosts) raise the audio callback thread; we copy that onto
//! `worker_pid` from the first audio-thread entry so the worker can wake and
//! finish inside the same-callback budget.

#[cfg(target_os = "linux")]
use libc::{self, pid_t, sched_param};

/// Cached `(policy, sched_priority)` last applied to the worker.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedMirror {
    pub policy: libc::c_int,
    pub priority: libc::c_int,
}

/// Read this thread's scheduler params and apply them to `worker_pid`.
///
/// Soft-fails on `EPERM` / `EINVAL`. Re-applies only when `(policy, prio)`
/// changed vs `cache`. If `sched_setscheduler` fails, falls back to
/// `setpriority(PRIO_PROCESS, worker_pid, -10)`.
///
/// Linux only. Does not invent a FIFO priority or set CPU affinity.
#[cfg(target_os = "linux")]
pub fn mirror_to_worker(worker_pid: pid_t, cache: &mut Option<SchedMirror>) {
    if worker_pid <= 0 {
        return;
    }

    let mut param = sched_param {
        sched_priority: 0,
    };
    let mut policy: libc::c_int = 0;
    // SAFETY: `pthread_self` is this thread; `param` / `policy` are stack locals.
    let rc = unsafe {
        libc::pthread_getschedparam(libc::pthread_self(), &raw mut policy, &raw mut param)
    };
    if rc != 0 {
        fallback_nice(worker_pid);
        return;
    }

    let next = SchedMirror {
        policy,
        priority: param.sched_priority,
    };
    if cache.as_ref() == Some(&next) {
        return;
    }

    // SAFETY: `worker_pid` is the child DSP process; `param` matches `policy`.
    let set_rc = unsafe { libc::sched_setscheduler(worker_pid, policy, &raw const param) };
    if set_rc == 0 {
        *cache = Some(next);
        return;
    }
    // Soft-fail EPERM/EINVAL (and any other errno): try niceness only.
    fallback_nice(worker_pid);
    *cache = Some(next);
}

#[cfg(target_os = "linux")]
fn fallback_nice(worker_pid: pid_t) {
    let Ok(who) = libc::id_t::try_from(worker_pid) else {
        return;
    };
    // SAFETY: `PRIO_PROCESS` + child pid; failure is ignored (best-effort).
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, who, -10);
    }
}
