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

/// Contract versions are **strict semver**, ordering included.
///
/// This matters for the [`min_generator_version`] floor: semver orders
/// `2.0.0-rc.1 < 2.0.0`, so a prerelease build of the generator does *not*
/// satisfy a floor of `2.0.0`. A hand-rolled `major.minor.patch` type that
/// discards the suffix would compare them equal and let an rc — which may not
/// yet implement the feature the floor is gating — silently pass.
///
/// Host *tool* versions (rustc, espflash, probe-rs) are a different problem:
/// they are reported in loose formats and their prereleases should be treated
/// as good enough. That leniency lives in the binary's `check` module and is
/// deliberately not shared with this type.
pub use semver::Version;

/// The `spec_version` this SDK implements. A breaking contract change bumps
/// this; additive growth does not (it raises a template's
/// [`min_generator_version`] floor instead).
pub const SPEC_VERSION: u32 = 1;

/// The first esp-generate release that exposes the `spec_version` 1 fact API.
/// Every baseline v1 feature is tagged with this.
pub const V1_BASELINE: Version = release(2, 0, 0);

/// A final (non-prerelease) release version, constructible in `const` context —
/// `semver::Version::new` is not `const`.
pub const fn release(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
        pre: semver::Prerelease::EMPTY,
        build: semver::BuildMetadata::EMPTY,
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
/// Not `Copy`: `semver::Version` owns its prerelease/build strings.
#[derive(Clone, Debug)]
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
    feature(name).map(|f| f.since.clone())
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

/// Whether a generator at `generator` satisfies a template's
/// `min_generator_version` floor.
///
/// Split out from a bare `>=` because the prerelease rule is the subtle part:
/// semver puts `2.1.0-rc.1` *below* `2.1.0`, so an rc of the very release that
/// introduces a feature does not clear that feature's floor. That is the
/// intended reading — the rc may predate the feature landing.
pub fn satisfies_floor(generator: &Version, floor: &Version) -> bool {
    generator >= floor
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn contract_versions_are_strict_semver() {
        // Full three-component semver only: the contract is machine-authored
        // (a template declares it, `check` computes it), so there is no reason
        // to accept the abbreviations host tools get away with.
        assert_eq!(Version::parse("2.0.0").unwrap(), release(2, 0, 0));
        assert!(Version::parse("2.1").is_err());
        assert!(Version::parse("3").is_err());
        assert!(Version::parse("2.0.0.0").is_err());
        assert!(Version::parse("x").is_err());

        assert!(release(2, 0, 0) < release(2, 0, 1));
        assert!(release(2, 1, 0) > release(2, 0, 9));
        assert_eq!(release(2, 0, 0).to_string(), "2.0.0");
        assert_eq!(release(2, 0, 0), V1_BASELINE);
    }

    #[test]
    fn a_prerelease_does_not_satisfy_a_final_release_floor() {
        // The whole reason contract versions use semver. Stripping the suffix
        // would make these compare equal, letting an rc of 2.0.0 — which may
        // not yet implement the feature being gated — pass a 2.0.0 floor.
        let rc = Version::parse("2.0.0-rc.1").unwrap();
        assert!(rc < V1_BASELINE, "rc must sort below the final release");
        assert!(!satisfies_floor(&rc, &V1_BASELINE));

        // The final release, and anything after it, does satisfy the floor.
        assert!(satisfies_floor(&release(2, 0, 0), &V1_BASELINE));
        assert!(satisfies_floor(&release(2, 0, 1), &V1_BASELINE));
        assert!(satisfies_floor(
            &Version::parse("2.1.0-rc.1").unwrap(),
            &V1_BASELINE
        ));

        // Prereleases order among themselves.
        assert!(Version::parse("2.0.0-rc.1").unwrap() < Version::parse("2.0.0-rc.2").unwrap());
        assert!(Version::parse("2.0.0-alpha").unwrap() < Version::parse("2.0.0-beta").unwrap());

        // Build metadata: semver §10 excludes it from precedence, but the
        // crate *derives* `Ord`, so it does participate — `2.0.0+a` sorts
        // above `2.0.0`. Harmless for a floor, because empty build metadata
        // always sorts first: carrying a `+build` suffix can only ever help a
        // version clear a floor, never block it. Pinned so the deviation is a
        // deliberate, recorded choice rather than a latent surprise.
        let with_build = Version::parse("2.0.0+a").unwrap();
        assert!(satisfies_floor(&with_build, &V1_BASELINE));
        assert!(with_build > V1_BASELINE);
        assert_ne!(with_build, V1_BASELINE);
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
        let later = release(2, 3, 0);
        let floor = [feature_since("option").unwrap(), later.clone()]
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
