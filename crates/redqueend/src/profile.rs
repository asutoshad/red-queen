//! Thermal profile control: validate, write, read back, verify.

use std::collections::BTreeSet;
use std::sync::Arc;

use rq_core::ThermalProfileId;
use rq_hardware::profile::{ProfileIo, is_firmware_rejection};
use rq_ipc::{ChoiceState, ProfileChoice, SetProfileResult, ThermalProfilesInfo};

/// Why a profile change didn't happen. Maps onto D-Bus errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetError {
    /// No controllable profile interface.
    Unavailable,
    /// The profile isn't one this hardware offers.
    InvalidArgument(String),
    /// The firmware rejected it earlier in this run.
    Unsupported(String),
    /// The firmware rejects it now.
    Rejected(String),
    /// The write was accepted but the kernel doesn't report the new profile.
    NotConfirmed(String),
    /// An unexpected I/O failure.
    Failed(String),
}

/// A confirmed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Profile before the change, if it could be read.
    pub previous: Option<ThermalProfileId>,
    /// Result handed back to the client.
    pub result: SetProfileResult,
}

/// Owns the profile interface and what has been learned about it.
#[derive(Debug, Default)]
pub struct ProfileController {
    io: Option<Arc<dyn ProfileIo>>,
    rejected: BTreeSet<ThermalProfileId>,
}

impl ProfileController {
    /// A controller over `io` (or none, when the hardware has no profiles).
    pub fn new(io: Option<Arc<dyn ProfileIo>>) -> Self {
        Self {
            io,
            rejected: BTreeSet::new(),
        }
    }

    /// Switches to a different interface. Rejections learned on the old one
    /// are forgotten; on the same interface they are kept.
    pub fn replace_io(&mut self, io: Option<Arc<dyn ProfileIo>>) {
        let same = match (&self.io, &io) {
            (Some(a), Some(b)) => a.describe() == b.describe(),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.rejected.clear();
        }
        self.io = io;
    }

    /// Current state, read live from the kernel.
    pub fn info(&self) -> ThermalProfilesInfo {
        let Some(io) = &self.io else {
            return ThermalProfilesInfo {
                available: false,
                active: None,
                choices: Vec::new(),
            };
        };
        ThermalProfilesInfo {
            available: true,
            active: io.read_active().ok(),
            choices: io
                .choices()
                .into_iter()
                .map(|id| ProfileChoice {
                    state: if self.rejected.contains(&id) {
                        ChoiceState::Unsupported
                    } else {
                        ChoiceState::Available
                    },
                    id,
                })
                .collect(),
        }
    }

    /// Changes the profile and verifies it. Blocking: does firmware I/O.
    pub fn set(&mut self, requested: ThermalProfileId) -> Result<Change, SetError> {
        let io = self.io.clone().ok_or(SetError::Unavailable)?;

        if !io.choices().contains(&requested) {
            return Err(SetError::InvalidArgument(format!(
                "'{}' is not a profile this hardware offers",
                requested.kernel_name()
            )));
        }
        if self.rejected.contains(&requested) {
            return Err(SetError::Unsupported(format!(
                "the firmware rejected '{}' earlier and it is disabled",
                requested.kernel_name()
            )));
        }

        let previous = io.read_active().ok();
        if let Err(e) = io.write_active(&requested) {
            if is_firmware_rejection(&e) {
                self.rejected.insert(requested.clone());
                let still = io
                    .read_active()
                    .map_or_else(|_| "unknown".to_owned(), |a| a.kernel_name().to_owned());
                return Err(SetError::Rejected(format!(
                    "the firmware rejected '{}' ({e}); the active profile is still '{still}'",
                    requested.kernel_name()
                )));
            }
            return Err(SetError::Failed(format!("writing the profile failed: {e}")));
        }

        match io.read_active() {
            Ok(active) if active == requested => Ok(Change {
                previous,
                result: SetProfileResult { requested, active },
            }),
            Ok(active) => Err(SetError::NotConfirmed(format!(
                "the request for '{}' was accepted, but the active profile is '{}'",
                requested.kernel_name(),
                active.kernel_name()
            ))),
            Err(e) => Err(SetError::NotConfirmed(format!(
                "the request for '{}' was accepted, but the active profile could not be read back: {e}",
                requested.kernel_name()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Mutex;

    /// Simulated firmware with configurable misbehaviour.
    #[derive(Debug)]
    struct Fake {
        active: Mutex<ThermalProfileId>,
        reject: Vec<ThermalProfileId>,
        ignore_writes: bool,
        writes: Mutex<Vec<ThermalProfileId>>,
        name: &'static str,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                active: Mutex::new(ThermalProfileId::Balanced),
                reject: vec![],
                ignore_writes: false,
                writes: Mutex::new(vec![]),
                name: "fake",
            }
        }
    }

    impl ProfileIo for Fake {
        fn choices(&self) -> Vec<ThermalProfileId> {
            ThermalProfileId::parse_choices(
                "low-power quiet balanced balanced-performance performance",
            )
        }
        fn read_active(&self) -> io::Result<ThermalProfileId> {
            Ok(self.active.lock().expect("lock").clone())
        }
        fn write_active(&self, p: &ThermalProfileId) -> io::Result<()> {
            self.writes.lock().expect("lock").push(p.clone());
            if self.reject.contains(p) {
                return Err(io::Error::from_raw_os_error(5)); // EIO, as acer-wmi does
            }
            if !self.ignore_writes {
                *self.active.lock().expect("lock") = p.clone();
            }
            Ok(())
        }
        fn describe(&self) -> String {
            self.name.to_owned()
        }
    }

    fn controller(f: Fake) -> (ProfileController, Arc<Fake>) {
        let f = Arc::new(f);
        (ProfileController::new(Some(f.clone())), f)
    }

    #[test]
    fn successful_change_is_verified() {
        let (mut c, _) = controller(Fake::new());
        let change = c.set(ThermalProfileId::Quiet).expect("change");
        assert_eq!(change.previous, Some(ThermalProfileId::Balanced));
        assert_eq!(change.result.active, ThermalProfileId::Quiet);
        assert_eq!(c.info().active, Some(ThermalProfileId::Quiet));
    }

    #[test]
    fn unknown_profile_is_rejected_before_any_write() {
        let (mut c, f) = controller(Fake::new());
        let err = c
            .set(ThermalProfileId::Other("turbo".into()))
            .expect_err("unknown");
        assert!(matches!(err, SetError::InvalidArgument(_)));
        assert!(
            f.writes.lock().expect("lock").is_empty(),
            "nothing was written"
        );
    }

    #[test]
    fn firmware_rejection_disables_the_profile_and_keeps_the_real_state() {
        let (mut c, f) = controller(Fake {
            reject: vec![ThermalProfileId::Performance],
            ..Fake::new()
        });
        let err = c.set(ThermalProfileId::Performance).expect_err("rejected");
        let SetError::Rejected(msg) = err else {
            panic!("expected Rejected, got {err:?}")
        };
        assert!(msg.contains("still 'balanced'"), "{msg}");
        assert_eq!(c.info().active, Some(ThermalProfileId::Balanced));

        let states: Vec<_> = c
            .info()
            .choices
            .iter()
            .map(|x| (x.id.clone(), x.state))
            .collect();
        assert!(states.contains(&(ThermalProfileId::Performance, ChoiceState::Unsupported)));
        assert!(states.contains(&(ThermalProfileId::Quiet, ChoiceState::Available)));

        // A second attempt is refused without touching the firmware again.
        let before = f.writes.lock().expect("lock").len();
        assert!(matches!(
            c.set(ThermalProfileId::Performance),
            Err(SetError::Unsupported(_))
        ));
        assert_eq!(f.writes.lock().expect("lock").len(), before);
        // Other profiles still work.
        assert!(c.set(ThermalProfileId::Quiet).is_ok());
    }

    #[test]
    fn accepted_but_unapplied_is_not_confirmed() {
        let (mut c, _) = controller(Fake {
            ignore_writes: true,
            ..Fake::new()
        });
        let err = c.set(ThermalProfileId::Quiet).expect_err("not confirmed");
        let SetError::NotConfirmed(msg) = err else {
            panic!("expected NotConfirmed")
        };
        assert!(msg.contains("active profile is 'balanced'"), "{msg}");
        assert_eq!(
            c.info().active,
            Some(ThermalProfileId::Balanced),
            "real state is reported"
        );
    }

    #[test]
    fn transient_errors_do_not_disable_a_profile() {
        #[derive(Debug)]
        struct Busy;
        impl ProfileIo for Busy {
            fn choices(&self) -> Vec<ThermalProfileId> {
                vec![ThermalProfileId::Quiet]
            }
            fn read_active(&self) -> io::Result<ThermalProfileId> {
                Ok(ThermalProfileId::Balanced)
            }
            fn write_active(&self, _: &ThermalProfileId) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(16)) // EBUSY
            }
            fn describe(&self) -> String {
                "busy".into()
            }
        }
        let mut c = ProfileController::new(Some(Arc::new(Busy)));
        assert!(matches!(
            c.set(ThermalProfileId::Quiet),
            Err(SetError::Failed(_))
        ));
        assert_eq!(c.info().choices[0].state, ChoiceState::Available);
    }

    #[test]
    fn no_interface_means_unavailable() {
        let mut c = ProfileController::new(None);
        assert!(!c.info().available);
        assert_eq!(c.set(ThermalProfileId::Quiet), Err(SetError::Unavailable));
    }

    #[test]
    fn learned_rejections_survive_the_same_interface_but_not_a_new_one() {
        let (mut c, _) = controller(Fake {
            reject: vec![ThermalProfileId::Performance],
            ..Fake::new()
        });
        let _ = c.set(ThermalProfileId::Performance);

        c.replace_io(Some(Arc::new(Fake::new()))); // same name: same interface
        assert!(matches!(
            c.set(ThermalProfileId::Performance),
            Err(SetError::Unsupported(_))
        ));

        c.replace_io(Some(Arc::new(Fake {
            name: "other",
            ..Fake::new()
        })));
        assert!(
            c.set(ThermalProfileId::Performance).is_ok(),
            "a new interface starts fresh"
        );
    }
}
