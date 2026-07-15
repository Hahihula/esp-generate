//! Contract feature registry — the *introduced-at* version of every fact and
//! directive in the contract, and the [`min_generator_version`] floor computed
//! from what a template actually uses.
//!
//! ## The two version gates
//!
//! `spec_version` bumps only on a breaking change ("v1 forever" is the goal).
//! Additive growth does not bump it; instead a template that uses newer
//! contract features declares a `min_generator_version` floor, so an older
//! binary gives a clean "update esp-generate to ≥ X" instead of an
//! `unknown fact` error mid-generate. The floor is computed from the max
//! `since` over the features the template references.

use std::{fmt::Display, str::FromStr};

/// The `spec_version` this SDK implements. A breaking contract change bumps
/// this; additive growth does not (it raises a template's
/// [`min_generator_version`] floor instead).
pub const SPEC_VERSION: u32 = 1;

/// The first esp-generate release that exposes the `spec_version` 1 fact API.
/// Every baseline v1 feature is tagged with this.
pub const V1_BASELINE: Version = Version::new(2, 0, 0);

/// A three-component release version, used for the additive-feature floor.
/// Deliberately not full semver (no pre-release / build metadata) — ordering
/// `major.minor.patch` is all the floor comparison needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl Version {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse a `major[.minor[.patch]]` string, ignoring any `-pre`/`+build`
    /// suffix. Missing minor/patch default to 0.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// Whether `self` is at least `other`. Equivalent to `self >= *other`;
    /// kept for call sites that read better as a question.
    pub fn is_at_least(&self, other: &Version) -> bool {
        self >= other
    }
}

/// Returned when a string isn't a `major[.minor[.patch]]` version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParseVersionError;

impl Display for ParseVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("expected a `major[.minor[.patch]]` version")
    }
}

impl std::error::Error for ParseVersionError {}

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut parts = core.split('.');
        let mut component = |missing_ok: bool| -> Result<u16, ParseVersionError> {
            match parts.next() {
                Some(p) => p.parse().map_err(|_| ParseVersionError),
                None if missing_ok => Ok(0),
                None => Err(ParseVersionError),
            }
        };
        let major = component(false)?;
        let minor = component(true)?;
        let patch = component(true)?;
        // Reject trailing junk like "1.2.3.4".
        if parts.next().is_some() {
            return Err(ParseVersionError);
        }
        Ok(Self::new(major, minor, patch))
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What kind of contract surface a [`Feature`] describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeatureKind {
    /// A somni predicate usable in `#IF` / `#INCLUDEFILE` (e.g. `chip_has`).
    Predicate,
    /// A binary-provided substitution value referenced by `#REPLACE` /
    /// `#INCLUDE_AS` (e.g. `chip`, `reserved_gpio_code`).
    Value,
    /// A file directive (e.g. `REPLACE`).
    Directive,
}

/// One contract feature, tagged with the release that introduced it.
#[derive(Clone, Copy, Debug)]
pub struct Feature {
    /// The name a template references — the somni identifier for a predicate,
    /// or the bare directive keyword (`IF`, `REPLACE`, …) for a directive.
    pub name: &'static str,
    pub kind: FeatureKind,
    /// The esp-generate release that introduced this feature.
    pub since: Version,
}

/// The full contract-feature registry for the current `spec_version`.
/// Only features the SDK actually implements are listed. Grows in lockstep
/// with the fact API: a feature added in a later release is appended here
/// with its own (higher) `since`.
pub const FEATURES: &[Feature] = &[
    // Predicates (see `process::build_context`).
    Feature {
        name: "option",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    Feature {
        name: "group_selected",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    Feature {
        name: "chip_has",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    Feature {
        name: "is_xtensa",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    Feature {
        name: "is_riscv",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    Feature {
        name: "has_reserved_pins",
        kind: FeatureKind::Predicate,
        since: V1_BASELINE,
    },
    // Binary-provided values (see the binary's `Facts` builder).
    Feature {
        name: "project_name",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "generate_version",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "generate_parameters",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "chip",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "rust_target",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "dram2_uninit_size",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "rust_toolchain",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    Feature {
        name: "reserved_gpio_code",
        kind: FeatureKind::Value,
        since: V1_BASELINE,
    },
    // Directives (see `process::process_file`).
    Feature {
        name: "INCLUDEFILE",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "INCLUDE_AS",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "IF",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "ELIF",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "ELSE",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "ENDIF",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
    Feature {
        name: "REPLACE",
        kind: FeatureKind::Directive,
        since: V1_BASELINE,
    },
];

/// Look up a contract feature by the name a template references.
pub fn feature(name: &str) -> Option<&'static Feature> {
    FEATURES.iter().find(|f| f.name == name)
}

/// The introduced-at version of a known feature, or `None` if the name isn't a
/// contract feature. (Validation — "unknown name = hard error" — is a separate
/// concern owned by `check`; this only answers "when was it introduced?".)
pub fn feature_since(name: &str) -> Option<Version> {
    feature(name).map(|f| f.since)
}

/// Compute the `min_generator_version` floor: the max `since` over the known
/// features referenced, never below [`V1_BASELINE`]. Unknown names are ignored
/// here (they surface as hard errors during `check`).
pub fn min_generator_version<'a>(used: impl IntoIterator<Item = &'a str>) -> Version {
    used.into_iter()
        .filter_map(feature_since)
        .max()
        .unwrap_or(V1_BASELINE)
        .max(V1_BASELINE)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn version_parses_and_orders() {
        assert_eq!(Version::parse("2.0.0"), Some(Version::new(2, 0, 0)));
        assert_eq!(Version::parse("2.1"), Some(Version::new(2, 1, 0)));
        assert_eq!(Version::parse("3"), Some(Version::new(3, 0, 0)));
        // Suffixes are ignored.
        assert_eq!(Version::parse("2.0.0-rc.1"), Some(Version::new(2, 0, 0)));
        assert_eq!(Version::parse("2.0.0+build"), Some(Version::new(2, 0, 0)));
        // Junk is rejected.
        assert_eq!(Version::parse("2.0.0.0"), None);
        assert_eq!(Version::parse("x"), None);

        assert!(Version::new(2, 0, 0) < Version::new(2, 0, 1));
        assert!(Version::new(2, 1, 0) > Version::new(2, 0, 9));
        assert_eq!(Version::new(2, 0, 0).to_string(), "2.0.0");
    }

    #[test]
    fn every_feature_name_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for f in FEATURES {
            assert!(seen.insert(f.name), "duplicate feature name: {}", f.name);
        }
    }

    #[test]
    fn implemented_predicates_are_registered() {
        for name in [
            "option",
            "group_selected",
            "chip_has",
            "is_xtensa",
            "is_riscv",
        ] {
            assert!(
                matches!(feature(name), Some(f) if f.kind == FeatureKind::Predicate),
                "predicate `{name}` is not registered as a contract feature"
            );
        }
    }

    #[test]
    fn floor_is_baseline_for_baseline_only_usage() {
        let floor = min_generator_version(["option", "IF", "REPLACE", "chip_has"]);
        assert_eq!(floor, V1_BASELINE);
        // No usage at all still floors at baseline (never below).
        assert_eq!(min_generator_version([]), V1_BASELINE);
    }

    #[test]
    fn floor_rises_to_the_newest_feature_used() {
        // Synthetic newer feature: the floor must climb to it without coupling
        // the test to a real future feature.
        let later = Version::new(2, 3, 0);
        let floor = [feature_since("option").unwrap(), later]
            .into_iter()
            .max()
            .unwrap();
        assert_eq!(floor, later);
        assert!(floor > V1_BASELINE);
    }

    #[test]
    fn unknown_names_do_not_affect_the_floor() {
        // Unknown names are a `check` hard error, handled elsewhere.
        let floor = min_generator_version(["option", "totally_made_up"]);
        assert_eq!(floor, V1_BASELINE);
    }
}
