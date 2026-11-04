//! Errors from the in-process `control.radio` seam.

use control_radio::Error as RadioLibError;

/// A failure from the in-process radio seam.
///
/// Two shapes, deliberately kept apart: a Hub with no LoRaWAN arm is not a broken command, it
/// is a command that does not apply — the caller renders it differently (a quiet
/// "not on this Hub", not a red error).
#[derive(Debug)]
pub enum RadioError {
    /// This Hub has no rendered radio config: a P2P-only or radio-off Hub, or a dev process
    /// without `/run/proximos/control/config.yaml`. The seam is inert rather than fatal.
    NotConfigured(String),
    /// The dispatch itself failed — ChirpStack down, an unknown DevEUI, the mode gate, a bad
    /// argument. Carries the shipped `control_radio::Error` verbatim.
    Dispatch(RadioLibError),
}

impl RadioError {
    /// The [`crate::relay::CommandOutcome::error_code`] for this failure,
    /// keyed on control.radio's closed `Kind` rather than on its prose.
    ///
    /// The mode refusals (`OffMode`, and `LoRaWANUnavailable` on a P2P Hub)
    /// read the same to an operator as a missing config: this Hub's LoRaWAN
    /// radio cannot be driven from here. The transient band means the
    /// radio service did not answer. Everything else stays unclassified and
    /// renders as the raw detail alone.
    pub fn code(&self) -> Option<&'static str> {
        use crate::relay::error_code;
        use control_radio::error::Kind;

        match self {
            RadioError::NotConfigured(_) => Some(error_code::RADIO_NOT_CONFIGURED),
            RadioError::Dispatch(e) => match e.kind() {
                Kind::OffMode | Kind::LorawanUnavailable => Some(error_code::RADIO_NOT_CONFIGURED),
                Kind::ServiceDown | Kind::Timeout | Kind::Transport | Kind::DaemonDown => {
                    Some(error_code::RADIO_UNAVAILABLE)
                }
                Kind::NotFound => Some(error_code::NODE_UNKNOWN),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RadioError::NotConfigured(why) => {
                write!(f, "radio is not configured on this Hub: {why}")
            }
            RadioError::Dispatch(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RadioError {}

impl From<RadioLibError> for RadioError {
    fn from(e: RadioLibError) -> Self {
        RadioError::Dispatch(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::error_code;
    use control_radio::error::{Component, Kind, Transport};

    fn dispatch(kind: Kind) -> RadioError {
        RadioError::Dispatch(RadioLibError::new(
            kind,
            Transport::Lorawan,
            Component::ChirpstackApi,
            "boom",
        ))
    }

    /// The slug follows control.radio's `Kind`, never its prose, and a kind
    /// with no operator-facing sentence stays unclassified.
    #[test]
    fn radio_errors_map_to_their_command_error_slug() {
        assert_eq!(
            RadioError::NotConfigured("no rendered radio config".into()).code(),
            Some(error_code::RADIO_NOT_CONFIGURED)
        );
        for (kind, slug) in [
            (Kind::OffMode, Some(error_code::RADIO_NOT_CONFIGURED)),
            (
                Kind::LorawanUnavailable,
                Some(error_code::RADIO_NOT_CONFIGURED),
            ),
            (Kind::ServiceDown, Some(error_code::RADIO_UNAVAILABLE)),
            (Kind::Timeout, Some(error_code::RADIO_UNAVAILABLE)),
            (Kind::Transport, Some(error_code::RADIO_UNAVAILABLE)),
            (Kind::DaemonDown, Some(error_code::RADIO_UNAVAILABLE)),
            (Kind::NotFound, Some(error_code::NODE_UNKNOWN)),
            (Kind::InvalidArgument, None),
            (Kind::Unauthenticated, None),
        ] {
            assert_eq!(dispatch(kind).code(), slug, "{kind}");
        }
    }
}
