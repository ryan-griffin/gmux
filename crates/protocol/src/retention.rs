//! Daemon-owned scrollback retention. Limits apply independently to every pane.
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_BYTES: usize = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitMode {
    Default,
    Unlimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Limit {
    Count(#[serde(deserialize_with = "finite_count")] usize),
    Mode(LimitMode),
}
fn finite_count<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    let value = usize::deserialize(deserializer)?;
    if value == usize::MAX {
        return Err(serde::de::Error::custom("Limit is too large"));
    }
    Ok(value)
}
impl std::str::FromStr for Limit {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Mode(LimitMode::Default)),
            "unlimited" => Ok(Self::Mode(LimitMode::Unlimited)),
            _ if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => value
                .parse::<usize>()
                .ok()
                .filter(|&value| value < usize::MAX)
                .map(Self::Count)
                .ok_or_else(|| "Limit is too large".into()),
            _ => Err("Use an unsigned integer, default, or unlimited".into()),
        }
    }
}
impl Limit {
    fn resolve(self, default: Option<usize>) -> anyhow::Result<Option<usize>> {
        // The native API uses SIZE_MAX as its unlimited sentinel internally.
        match self {
            Self::Count(value) => {
                anyhow::ensure!(value < usize::MAX, "Limit is too large");
                Ok(Some(value))
            }
            Self::Mode(LimitMode::Default) => Ok(default),
            Self::Mode(LimitMode::Unlimited) => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollbackLimits {
    pub max_bytes: Limit,
    pub max_lines: Limit,
}
impl Default for ScrollbackLimits {
    fn default() -> Self {
        Self {
            max_bytes: Limit::Count(DEFAULT_MAX_BYTES),
            max_lines: Limit::Mode(LimitMode::Unlimited),
        }
    }
}
impl ScrollbackLimits {
    /// Validate both limits and resolve defaults to optional byte and line counts.
    pub fn resolve_values(self) -> anyhow::Result<(Option<usize>, Option<usize>)> {
        Ok((
            self.max_bytes.resolve(Some(DEFAULT_MAX_BYTES))?,
            self.max_lines.resolve(None)?,
        ))
    }
    pub fn resolve(self) -> anyhow::Result<Self> {
        let (max_bytes, max_lines) = self.resolve_values()?;
        let limit =
            |value: Option<usize>| value.map_or(Limit::Mode(LimitMode::Unlimited), Limit::Count);
        Ok(Self {
            max_bytes: limit(max_bytes),
            max_lines: limit(max_lines),
        })
    }
    pub fn update(
        self,
        max_bytes: Option<Limit>,
        max_lines: Option<Limit>,
    ) -> anyhow::Result<Self> {
        Self {
            max_bytes: max_bytes.unwrap_or(self.max_bytes),
            max_lines: max_lines.unwrap_or(self.max_lines),
        }
        .resolve()
    }
}

/// Omission leaves a limit unchanged. Explicit null is a malformed update.
pub(crate) fn update_field<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Limit>, D::Error> {
    Limit::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ExecutionRequest, Request};
    use serde_json::json;

    #[test]
    fn complete_requests_distinguish_omission_reset_unlimited_and_zero() {
        let current = ScrollbackLimits {
            max_bytes: Limit::Count(128),
            max_lines: Limit::Count(42),
        };
        let decode = |value| {
            Request::decode(value)
                .unwrap()
                .into_execution(|_| unreachable!())
                .unwrap()
                .0
        };
        for (fields, expected) in [
            (json!({}), current),
            (json!({"max_bytes": 128, "max_lines": 42}), current),
            (
                json!({"max_bytes": "default"}),
                ScrollbackLimits {
                    max_bytes: Limit::Count(DEFAULT_MAX_BYTES),
                    ..current
                },
            ),
            (
                json!({"max_bytes": "unlimited", "max_lines": 0}),
                ScrollbackLimits {
                    max_bytes: Limit::Mode(LimitMode::Unlimited),
                    max_lines: Limit::Count(0),
                },
            ),
            (
                json!({"max_bytes": 0, "max_lines": "default"}),
                ScrollbackLimits {
                    max_bytes: Limit::Count(0),
                    max_lines: Limit::Mode(LimitMode::Unlimited),
                },
            ),
        ] {
            let mut wire = fields;
            wire["method"] = json!("scrollback");
            let request = decode(wire);
            let mutates = request.mutates_state();
            let ExecutionRequest::Scrollback {
                max_bytes,
                max_lines,
            } = request
            else {
                panic!()
            };
            assert_eq!(mutates, max_bytes.is_some() || max_lines.is_some());
            let updated = current.update(max_bytes, max_lines).unwrap();
            assert_eq!(updated, expected);
        }
        for value in [
            json!(null),
            json!(-1),
            json!(1.5),
            json!(true),
            json!("10"),
            json!("bad"),
            json!({}),
            json!([]),
            json!(usize::MAX),
        ] {
            assert!(
                Request::decode(json!({"method":"scrollback", "max_bytes":0, "max_lines":value}))
                    .is_err()
            );
        }
        assert!(
            Request::decode(json!({"method":"scrollback", "max_bytes":0, "unexpected":true}))
                .is_err()
        );
        assert!(
            current
                .update(Some(Limit::Count(0)), Some(Limit::Count(usize::MAX)))
                .is_err()
        );
        assert!(Request::decode(json!({"method":"_execution", "request":{"method":"scrollback", "max_bytes":0}, "unexpected":true})).is_err());
    }
}
