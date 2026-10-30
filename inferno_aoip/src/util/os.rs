use thread_priority::Error;

#[cfg(all(target_family = "unix", not(target_vendor = "apple")))]
pub fn set_current_thread_realtime(priority_hint: u8) -> Result<(), Error> {
  use thread_priority::unix::set_thread_priority_and_policy;
  use thread_priority::{
    thread_native_id, RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue,
    ThreadSchedulePolicy,
  };

  set_thread_priority_and_policy(
    thread_native_id(),
    ThreadPriority::Crossplatform(
      ThreadPriorityValue::try_from(priority_hint.min(ThreadPriorityValue::MAX)).unwrap(),
    ),
    ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
  )
}

// On Apple platforms SCHED_FIFO needs root, so for a normal process it fails and the thread
// keeps normal priority: sleeps overrun by milliseconds and the TX flow drops out. A Mach
// time-constraint policy needs no privileges and is what Core Audio uses for its own threads.
#[cfg(target_vendor = "apple")]
pub fn set_current_thread_realtime(priority_hint: u8) -> Result<(), Error> {
  let mut policy = time_constraint_policy(priority_hint, mach_timebase()?);
  // safety: policy outlives the call, and the count matches its type
  let result = unsafe {
    libc::thread_policy_set(
      libc::pthread_mach_thread_np(libc::pthread_self()),
      libc::THREAD_TIME_CONSTRAINT_POLICY as _,
      &mut policy as *mut _ as libc::thread_policy_t,
      libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
    )
  };
  if result == 0 {
    Ok(())
  } else {
    log::warn!("unable to set real-time thread policy: thread_policy_set returned {result}");
    Err(Error::OS(result))
  }
}

#[cfg(target_vendor = "apple")]
fn mach_timebase() -> Result<mach2::mach_time::mach_timebase_info, Error> {
  let mut timebase = mach2::mach_time::mach_timebase_info::default();
  // safety: valid out-pointer
  let result = unsafe { mach2::mach_time::mach_timebase_info(&mut timebase) };
  if result != 0 {
    log::warn!("unable to set real-time thread policy: mach_timebase_info returned {result}");
    Err(Error::OS(result))
  } else if timebase.numer == 0 || timebase.denom == 0 {
    log::warn!("unable to set real-time thread policy: mach_timebase_info gave a zero timebase");
    Err(Error::OS(mach2::kern_return::KERN_FAILURE))
  } else {
    Ok(timebase)
  }
}

// Mach real-time threads have no priority levels to map priority_hint onto. Instead a higher
// hint gets a tighter constraint, from 2 ms at hint 0 to about 1 ms at 99. The scheduler orders
// ready real-time threads by deadline, so when TX (81) and RX (80) become ready together TX runs
// first. That is weaker than SCHED_FIFO, where a higher priority always preempts.
// Half the constraint is computation, leaving room for a heavy iteration (many flows, high
// sample rates). Period 0: the flow threads wake at packet times and poll timeouts, not on a
// fixed period.
#[cfg(target_vendor = "apple")]
fn time_constraint_policy(
  priority_hint: u8,
  timebase: mach2::mach_time::mach_timebase_info,
) -> libc::thread_time_constraint_policy {
  let ns_to_abs = |ns: u64| (ns * timebase.denom as u64 / timebase.numer as u64) as u32;
  let constraint_ns =
    2_000_000 - 10_000 * priority_hint.min(thread_priority::ThreadPriorityValue::MAX) as u64;
  libc::thread_time_constraint_policy {
    period: 0,
    computation: ns_to_abs(constraint_ns / 2),
    constraint: ns_to_abs(constraint_ns),
    preemptible: 1,
  }
}

#[cfg(not(target_family = "unix"))]
pub fn set_current_thread_realtime(_priority_hint: u8) -> Result<(), Error> {
  thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
}

#[cfg(all(test, target_vendor = "apple"))]
mod tests {
  use super::*;

  fn current_policy() -> (libc::thread_time_constraint_policy, libc::boolean_t) {
    let mut policy =
      libc::thread_time_constraint_policy { period: 0, computation: 0, constraint: 0, preemptible: 0 };
    let mut count = libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    let mut get_default: libc::boolean_t = 0;
    // safety: valid out-pointers, and the count matches the policy's type
    let result = unsafe {
      libc::thread_policy_get(
        libc::pthread_mach_thread_np(libc::pthread_self()),
        libc::THREAD_TIME_CONSTRAINT_POLICY as _,
        &mut policy as *mut _ as libc::thread_policy_t,
        &mut count,
        &mut get_default,
      )
    };
    assert_eq!(result, 0);
    (policy, get_default)
  }

  #[test]
  fn applies_time_constraint_policy() {
    std::thread::spawn(|| {
      set_current_thread_realtime(81).unwrap();
      let (policy, get_default) = current_policy();
      assert_eq!(get_default, 0, "thread still has the default policy");
      let expected = time_constraint_policy(81, mach_timebase().unwrap());
      assert_eq!(
        (policy.period, policy.computation, policy.constraint),
        (expected.period, expected.computation, expected.constraint)
      );
    })
    .join()
    .unwrap();
  }

  #[test]
  fn higher_hint_gets_earlier_deadline() {
    let timebase = mach2::mach_time::mach_timebase_info { numer: 125, denom: 3 };
    let tx = time_constraint_policy(81, timebase);
    let rx = time_constraint_policy(80, timebase);
    assert!(tx.constraint < rx.constraint);
    assert!(tx.computation <= tx.constraint);
    assert_eq!(
      time_constraint_policy(255, timebase).constraint,
      time_constraint_policy(99, timebase).constraint
    );
  }
}
