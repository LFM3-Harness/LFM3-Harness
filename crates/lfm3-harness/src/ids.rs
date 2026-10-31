use std::{
    fmt::{self, Display},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::HarnessError;

macro_rules! uuid_identifier {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a new random, opaque identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4().simple().to_string())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = HarnessError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Ok(Self(value.to_ascii_lowercase()));
                }
                Err(HarnessError::InvalidId {
                    kind: $label,
                    value: value.to_owned(),
                })
            }
        }
    };
}

uuid_identifier!(RunId, "run");
uuid_identifier!(StepId, "step");
uuid_identifier!(AttemptId, "attempt");
uuid_identifier!(ArtifactId, "artifact");

#[cfg(test)]
mod tests {
    use super::RunId;

    #[test]
    fn generated_identifiers_round_trip_through_text() {
        let original = RunId::new();
        let parsed = original.as_str().parse::<RunId>().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn identifiers_reject_unbounded_model_text() {
        assert!("not-an-identifier".parse::<RunId>().is_err());
    }
}
