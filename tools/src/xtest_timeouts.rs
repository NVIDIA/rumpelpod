// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Non-default xtest timeouts, stored as multiples of the default.
//!
//! The file is the source of truth at list time so the runner can
//! schedule long tests first.  Values are multiples (not seconds) so
//! `--timeout` / `XTEST_TIMEOUT` scales every entry.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Repo-relative path to the timeouts file.
pub const XTEST_TIMEOUTS_REL: &str = "tools/xtest-timeouts.json5";

/// Smallest allowed multiplier.  1x is the default and must be omitted.
pub const MIN_TIMEOUT_MULTIPLIER: u64 = 2;

/// Test name -> multiplier of the default timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutOverrides {
    pub multipliers: BTreeMap<String, u64>,
}

impl TimeoutOverrides {
    pub fn parse(contents: &str) -> Result<Self> {
        let raw: BTreeMap<String, u64> =
            json5::from_str(contents).context("parsing xtest timeouts file")?;
        let mut multipliers = BTreeMap::new();
        let mut invalid = Vec::new();
        for (name, multiplier) in raw {
            if name.is_empty() {
                return Err(anyhow::anyhow!(
                    "xtest timeouts file contains an empty test name"
                ));
            }
            if multiplier < MIN_TIMEOUT_MULTIPLIER {
                invalid.push(format!("{name}: {multiplier}"));
                continue;
            }
            multipliers.insert(name, multiplier);
        }
        if !invalid.is_empty() {
            let joined = invalid.join(", ");
            return Err(anyhow::anyhow!(
                "xtest timeout multipliers must be integers >= {MIN_TIMEOUT_MULTIPLIER} \
                 (omit tests that use the default): {joined}"
            ));
        }
        Ok(Self { multipliers })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&contents)
    }

    /// Every listed name must exist in the discovered suite, including
    /// tests filtered out of this run and `#[ignore]` tests.
    pub fn validate(&self, known_tests: &HashSet<String>) -> Result<()> {
        let unknown: BTreeSet<&str> = self
            .multipliers
            .keys()
            .filter(|name| !known_tests.contains(name.as_str()))
            .map(String::as_str)
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let joined = unknown.into_iter().collect::<Vec<_>>().join(", ");
        Err(anyhow::anyhow!(
            "xtest timeouts file lists tests that do not exist: {joined}"
        ))
    }

    pub fn timeout_for(&self, name: &str, default: Duration) -> Result<Duration> {
        let Some(&multiplier) = self.multipliers.get(name) else {
            return Ok(default);
        };
        let default_secs = default.as_secs();
        let secs = default_secs.checked_mul(multiplier).with_context(|| {
            format!("timeout overflow: {default_secs}s * {multiplier} for {name}")
        })?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parse_rejects_default_multiplier() {
        let err = TimeoutOverrides::parse(r#"{ "foo::bar": 1 }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo::bar: 1"), "{err}");
    }

    #[test]
    fn parse_rejects_zero() {
        let err = TimeoutOverrides::parse(r#"{ "foo::bar": 0 }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("foo::bar: 0"), "{err}");
    }

    #[test]
    fn parse_accepts_json5_comments_and_trailing_comma() {
        let parsed = TimeoutOverrides::parse(indoc! {r#"
            {
              // long SSH work
              "ssh::slow": 5,
            }
        "#})
        .expect("parse");
        assert_eq!(parsed.multipliers.get("ssh::slow"), Some(&5));
    }

    #[test]
    fn validate_requires_every_listed_name() {
        let parsed = TimeoutOverrides::parse(r#"{ "exists": 2, "missing": 3 }"#).unwrap();
        let known = HashSet::from(["exists".to_string(), "other".to_string()]);
        let err = parsed.validate(&known).unwrap_err().to_string();
        assert!(err.contains("missing"), "{err}");
        assert!(!err.contains("exists"), "{err}");
    }

    #[test]
    fn timeout_for_scales_with_default() {
        let parsed = TimeoutOverrides::parse(r#"{ "slow": 5 }"#).unwrap();
        assert_eq!(
            parsed.timeout_for("slow", Duration::from_secs(60)).unwrap(),
            Duration::from_secs(300)
        );
        assert_eq!(
            parsed.timeout_for("fast", Duration::from_secs(60)).unwrap(),
            Duration::from_secs(60)
        );
    }
}
