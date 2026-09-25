//! Keeping the machine awake while an agent is working.
//!
//! Two decisions worth keeping:
//!
//! - **System sleep only, never the display.** A forty-minute agent run should
//!   not hold the screen on. macOS separates the two assertions and we take
//!   only `PreventUserIdleSystemSleep`.
//! - **The release is held off by [`LINGER`].** An agent between two tool calls
//!   is silent for longer than the "is it still working" threshold all the
//!   time. Dropping the assertion on every such gap would thrash it several
//!   times a minute, so the guard waits out a quiet period before letting go.
//!   Taking it is immediate; only letting go is delayed.

use std::time::{Duration, Instant};

/// How long the assertion is held after the last moment any agent was working.
/// Longer than the working threshold by enough that ordinary think-time gaps
/// never release it.
pub const LINGER: Duration = Duration::from_secs(90);

/// Holds a "do not idle-sleep" assertion for as long as some agent is working.
///
/// Drive it from a sweep: call [`AwakeGuard::set`] every pass with whether any
/// agent is currently working, and it takes or releases the platform assertion
/// as needed. Dropping the guard releases immediately — which is what makes
/// quitting Bench leave no assertion behind.
pub struct AwakeGuard {
    platform: Option<platform::Assertion>,
    /// When some agent was last seen working. `None` once the assertion has
    /// been released, so a quiet Bench does no work at all.
    last_working: Option<Instant>,
    linger: Duration,
}

impl AwakeGuard {
    pub fn new() -> Self {
        Self::with_linger(LINGER)
    }

    pub fn with_linger(linger: Duration) -> Self {
        Self {
            platform: None,
            last_working: None,
            linger,
        }
    }

    /// Whether the assertion is currently held.
    pub fn is_held(&self) -> bool {
        self.platform.is_some()
    }

    /// One sweep's worth of bookkeeping.
    ///
    /// `any_working` is whether any agent is working right now; `now` is the
    /// sweep's instant, taken by the caller so the whole pass shares one clock.
    pub fn set(&mut self, any_working: bool, now: Instant) {
        if any_working {
            self.last_working = Some(now);
            if self.platform.is_none() {
                // A failure here is not worth failing a sweep over: the machine
                // sleeping is a degradation, not a corruption. Log and carry
                // on, and the next sweep tries again.
                match platform::Assertion::take() {
                    Ok(assertion) => {
                        log::debug!("holding the stay-awake assertion: an agent is working");
                        self.platform = Some(assertion);
                    }
                    Err(error) => {
                        log::warn!("could not hold the stay-awake assertion: {error:#}");
                    }
                }
            }
            return;
        }

        let Some(last_working) = self.last_working else {
            return;
        };
        if now.duration_since(last_working) < self.linger {
            return;
        }
        if self.platform.take().is_some() {
            log::debug!("releasing the stay-awake assertion: no agent has worked recently");
        }
        self.last_working = None;
    }

    /// Lets the assertion go now, whatever the linger would have said. Used
    /// when the user turns the whole thing off.
    pub fn release(&mut self) {
        self.platform = None;
        self.last_working = None;
    }
}

impl Default for AwakeGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, bail};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    // IOKit's power-management assertions. `IOPMAssertionCreateWithName` returns
    // `kIOReturnSuccess` (0) and fills in an id that `IOPMAssertionRelease`
    // takes back.
    type IOPMAssertionID = u32;
    type IOReturn = i32;

    const IO_RETURN_SUCCESS: IOReturn = 0;
    const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
    }

    /// Idle *system* sleep only. The display-sleep assertion is deliberately
    /// not taken; see the module docs.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";
    /// Shown in `pmset -g assertions`, so make it say who and why.
    const ASSERTION_NAME: &str = "Bench: an agent is working";

    pub struct Assertion(IOPMAssertionID);

    impl Assertion {
        pub fn take() -> Result<Self> {
            let assertion_type = CFString::new(ASSERTION_TYPE);
            let assertion_name = CFString::new(ASSERTION_NAME);
            let mut id: IOPMAssertionID = 0;
            // SAFETY: both strings outlive the call, and `id` is a valid
            // out-pointer. IOKit copies what it keeps.
            let result = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type.as_concrete_TypeRef(),
                    K_IOPM_ASSERTION_LEVEL_ON,
                    assertion_name.as_concrete_TypeRef(),
                    &mut id,
                )
            };
            if result != IO_RETURN_SUCCESS {
                bail!("IOPMAssertionCreateWithName failed with {result}");
            }
            Ok(Self(id))
        }
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful create and is released
            // exactly once, here.
            let result = unsafe { IOPMAssertionRelease(self.0) };
            if result != IO_RETURN_SUCCESS {
                log::warn!("IOPMAssertionRelease failed with {result}");
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use anyhow::Result;

    /// Every other platform: the guard's bookkeeping still runs, but there is
    /// nothing to assert. Linux would want a systemd-inhibit lock and Windows
    /// `SetThreadExecutionState`; neither is wired up yet.
    pub struct Assertion;

    impl Assertion {
        pub fn take() -> Result<Self> {
            Ok(Self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the linger: an agent that goes quiet between two tool
    /// calls must not drop the assertion.
    #[test]
    fn a_quiet_gap_shorter_than_the_linger_keeps_the_assertion() {
        let mut guard = AwakeGuard::with_linger(Duration::from_secs(90));
        let start = Instant::now();

        guard.set(true, start);
        assert!(guard.is_held());

        guard.set(false, start + Duration::from_secs(30));
        assert!(
            guard.is_held(),
            "a thirty second gap is an agent thinking, not an agent finishing"
        );

        guard.set(true, start + Duration::from_secs(31));
        assert!(guard.is_held());
    }

    #[test]
    fn a_quiet_period_past_the_linger_releases_it() {
        let mut guard = AwakeGuard::with_linger(Duration::from_secs(90));
        let start = Instant::now();

        guard.set(true, start);
        assert!(guard.is_held());

        guard.set(false, start + Duration::from_secs(91));
        assert!(!guard.is_held(), "the work is over; let the machine sleep");
    }

    #[test]
    fn nothing_is_held_for_a_machine_with_no_agents() {
        let mut guard = AwakeGuard::new();
        guard.set(false, Instant::now());
        assert!(!guard.is_held());
    }
}
