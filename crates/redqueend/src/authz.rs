//! Authorization of privileged requests through polkit.
//!
//! Every method that changes hardware names the [`Action`] it needs, and the
//! daemon asks polkit whether the *calling bus connection* may do it. The
//! subject is the caller's unique bus name, so polkit resolves the real
//! process itself (no pid or uid supplied by the client is ever trusted).
//! Any failure to get an answer means "no".

use std::collections::HashMap;

use tracing::warn;
use zbus::message::Header;
use zbus_polkit::policykit1::{AuthorityProxy, CheckAuthorizationFlags, Subject};

/// A privileged operation class. Each maps to one polkit action in
/// `packaging/polkit/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Switching the thermal profile.
    SetProfile,
    /// Manual fan control and fan curves.
    ControlFans,
    /// Battery charge limit, calibration, USB charging.
    Battery,
    /// Firmware-backed settings (overdrive, boot sound, ...).
    FirmwareSettings,
    /// Enabling driver options.
    ManageDriver,
}

impl Action {
    /// The polkit action id.
    pub fn id(self) -> &'static str {
        match self {
            Self::SetProfile => "io.github.asutoshad.RedQueen.set-profile",
            Self::ControlFans => "io.github.asutoshad.RedQueen.control-fans",
            Self::Battery => "io.github.asutoshad.RedQueen.battery",
            Self::FirmwareSettings => "io.github.asutoshad.RedQueen.firmware-settings",
            Self::ManageDriver => "io.github.asutoshad.RedQueen.manage-driver",
        }
    }
}

/// Why a request wasn't authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// polkit said no, or the password prompt was dismissed.
    Denied(String),
    /// No answer could be obtained. Treated as a refusal.
    Unavailable(String),
}

/// Decides whether a caller may perform an [`Action`].
pub enum Authorizer {
    /// Ask polkit.
    Polkit(AuthorityProxy<'static>),
    /// Refuse everything (used where hardware control is disabled).
    DenyAll(String),
    /// Allow everything. **Tests only**: the daemon never constructs this.
    AllowAll,
}

impl Authorizer {
    /// Connects to polkit on the system bus.
    pub async fn polkit() -> zbus::Result<Self> {
        let conn = zbus::Connection::system().await?;
        Ok(Self::Polkit(AuthorityProxy::new(&conn).await?))
    }

    /// Checks whether the sender of `header` may perform `action`,
    /// allowing polkit to prompt the user.
    pub async fn check(&self, header: &Header<'_>, action: Action) -> Result<(), AuthError> {
        match self {
            Self::AllowAll => Ok(()),
            Self::DenyAll(why) => Err(AuthError::Denied(why.clone())),
            Self::Polkit(authority) => {
                let subject = Subject::new_for_message_header(header)
                    .map_err(|e| AuthError::Unavailable(e.to_string()))?;
                let result = authority
                    .check_authorization(
                        &subject,
                        action.id(),
                        &HashMap::new(),
                        CheckAuthorizationFlags::AllowUserInteraction.into(),
                        "",
                    )
                    .await
                    .map_err(|e| {
                        warn!(error = %e, action = action.id(), "polkit check failed");
                        AuthError::Unavailable(format!("polkit: {e}"))
                    })?;
                if result.is_authorized {
                    Ok(())
                } else if result.details.contains_key("polkit.dismissed") {
                    Err(AuthError::Denied(
                        "the authentication prompt was dismissed".into(),
                    ))
                } else {
                    Err(AuthError::Denied("not authorized".into()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_are_namespaced_and_distinct() {
        let all = [
            Action::SetProfile,
            Action::ControlFans,
            Action::Battery,
            Action::FirmwareSettings,
            Action::ManageDriver,
        ];
        let mut ids: Vec<&str> = all.iter().map(|a| a.id()).collect();
        assert!(
            ids.iter()
                .all(|i| i.starts_with("io.github.asutoshad.RedQueen."))
        );
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
    }
}
