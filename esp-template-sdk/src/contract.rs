//! Contract feature registry — the SDK version that introduced each fact, and
//! the minimum SDK version a template needs, computed from what it uses.
//!
//! ## One version, not two
//!
//! The contract is versioned by this crate's own semver version carries both meanings:
//!
//! * the **major** *is* the spec version — a breaking contract change bumps it
//! * the **minor** is the additive-feature level — a template that uses a newer
//!   fact needs an SDK at least that new.
//!
//! A template declares the SDK version it was written against, and
//! [`is_compatible`] is the whole gate. Tagging features with the crate's
//! *current* version also means a feature is usable the moment it lands: the
//! SDK that implements it satisfies its own `since` by construction, with no
//! release ceremony in between.
//!
//! ## Scope: the fact API, not the language
//!
//! This registry covers only the facts the SDK registers for template
//! expressions — the predicates and values in [`FEATURES`]. The template
//! *language* (directive keywords, interpolation, expression syntax) belongs to
//! the templating engine and is versioned by that dependency's semver in
//! `Cargo.toml`. Two surfaces, two owners, one version number each.

use std::sync::LazyLock;

/// Contract versions are **strict semver**, ordering included.
///
/// This matters for [`is_compatible`]: semver orders `2.0.0-rc.1 < 2.0.0`, so a
/// prerelease build does *not* satisfy a requirement of `2.0.0`. A hand-rolled
/// `major.minor.patch` type that discards the suffix would compare them equal
/// and let an rc — which may not yet implement the feature being gated —
/// silently pass.
///
/// Host *tool* versions (rustc, espflash, probe-rs) are a different problem:
/// they are reported in loose formats and their prereleases should be treated
/// as good enough. That leniency lives in the binary's `check` module and is
/// deliberately not shared with this type.
pub use semver::Version;

/// The contract version this SDK implements: its own crate version.
///
/// There is no separate spec-version integer — the semver major already means
/// "breaking contract change", so a template that works with SDK `1.x` works
/// with every `1.y >= 1.x` and with no `2.z`.
pub static SDK_VERSION: LazyLock<Version> = LazyLock::new(|| {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version must be strict semver")
});

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
    /// A somni predicate callable from a template condition (e.g. `chip_has`).
    Predicate,
    /// A binary-provided value a template can interpolate or test against
    /// (e.g. `project_name`, `reserved_gpio_code`). These are supplied through
    /// `Facts::values`, so a template-scoped key of the same name is already
    /// beaten by the binary's first-writer-wins rule.
    Value,
    /// A namespace of related fields, addressed as `name.field` (`chip`).
    /// Registered directly rather than through `Facts::values`, so it needs
    /// explicit protection from a template-scoped value of the same name.
    Struct,
}

/// The fact API, grouped by the SDK release that introduced each batch.
///
/// **Adding a feature:** append it to the last group if that group's version is
/// still unreleased, otherwise start a new group at the current crate version.
/// A group must never be newer than [`SDK_VERSION`] — the SDK has to satisfy
/// its own registry, which `no_feature_claims_to_predate_the_sdk_that_ships_it`
/// enforces.
const REGISTRY: &[(Version, &[(&str, FeatureKind)])] = &[(
    // The whole fact API landed in the first SDK release.
    release(0, 1, 0),
    &[
        // Predicates (registered in `process::build_env`).
        ("option", FeatureKind::Predicate),
        ("group_selected", FeatureKind::Predicate),
        ("has_reserved_pins", FeatureKind::Predicate),
        // Binary-provided values (set by the binary's `Facts` builder).
        //
        // `chip` is a struct: `chip.name`, `chip.rust_target`,
        // `chip.dram2_uninit_size` and one field per `esp-metadata` symbol.
        // Its *fields* are not versioned individually — the symbol set is
        // metadata-driven, and gaining a symbol is additive by construction.
        ("chip", FeatureKind::Struct),
        ("project_name", FeatureKind::Value),
        ("generate_version", FeatureKind::Value),
        ("generate_parameters", FeatureKind::Value),
        ("rust_toolchain", FeatureKind::Value),
        ("reserved_gpio_code", FeatureKind::Value),
    ],
)];

/// One contract feature, resolved out of [`REGISTRY`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The name a template references.
    pub name: &'static str,
    pub kind: FeatureKind,
    /// The SDK release that introduced this feature.
    pub since: Version,
}

/// Every contract feature, oldest group first.
pub fn features() -> impl Iterator<Item = Feature> {
    REGISTRY.iter().flat_map(|(since, batch)| {
        batch.iter().map(move |(name, kind)| Feature {
            name,
            kind: *kind,
            since: since.clone(),
        })
    })
}

/// Look up a contract feature by the name a template references.
pub fn feature(name: &str) -> Option<Feature> {
    features().find(|f| f.name == name)
}

/// The introduced-at version of a known feature, or `None` if the name isn't a
/// contract feature. (Validation — "unknown name = hard error" — is a separate
/// concern owned by `check`; this only answers "when was it introduced?".)
pub fn feature_since(name: &str) -> Option<Version> {
    feature(name).map(|f| f.since)
}

/// Whether `name` is registered *outside* `Facts::values`, so a value carrying
/// that name must not be registered over it.
///
/// Predicates and structs qualify. Plain [`Value`](FeatureKind::Value) features
/// do not — they are *supplied* through `Facts::values`, and skipping them here
/// would drop the binary's own facts on the floor.
///
/// Derived from [`REGISTRY`] rather than kept as its own list next to the
/// registrations: the two would otherwise have to be edited together, and
/// nothing would catch it when they weren't.
pub fn is_reserved_name(name: &str) -> bool {
    matches!(
        feature(name),
        Some(f) if matches!(f.kind, FeatureKind::Predicate | FeatureKind::Struct)
    )
}

/// The lowest SDK version that provides every feature in `used`, or `None` if
/// `used` references no contract feature at all — such a template asks nothing
/// of the SDK, so any version will run it.
pub fn min_sdk_version<'a>(used: impl IntoIterator<Item = &'a str>) -> Option<Version> {
    used.into_iter().filter_map(feature_since).max()
}

/// Whether an SDK at `sdk` can run a template written against `required`.
///
/// Two conditions, which together replace the old `spec_version` + floor pair:
///
/// * **same compatibility range** — a major bump (or, below 1.0, a minor bump)
///   is a breaking contract change, so it must not silently apply;
/// * **`sdk >= required`** — the SDK has to actually provide the features.
///
/// Note this is deliberately *not* `semver::VersionReq`'s caret matching, which
/// excludes prereleases outside the comparator's own version: `2.1.0-rc.1` does
/// satisfy a requirement of `2.0.0` here, because an rc of a later release does
/// contain everything the earlier one had. esp-generate ships rcs, and locking
/// their users out of every template would be the wrong trade. A prerelease of
/// the *required* version itself still fails, since `2.0.0-rc.1 < 2.0.0`.
pub fn is_compatible(sdk: &Version, required: &Version) -> bool {
    compatibility_range(sdk) == compatibility_range(required) && sdk >= required
}

/// The compatibility range a version belongs to, as Cargo's caret operator
/// defines it: `1.4.0` and `1.7.2` share one, `0.1.x` and `0.2.x` do not
/// (below 1.0 the minor carries breaking changes), and `0.0.x` releases are
/// each their own.
fn compatibility_range(v: &Version) -> (u64, u64, u64) {
    match (v.major, v.minor) {
        (0, 0) => (0, 0, v.patch),
        (0, minor) => (0, minor, 0),
        (major, _) => (major, 0, 0),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_contract_version_is_the_crate_version() {
        // The whole point of dropping `SPEC_VERSION`: there is one number, and
        // Cargo owns it. If this ever needs a second source of truth, the
        // single-version model has broken down.
        assert_eq!(SDK_VERSION.to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn no_feature_claims_to_predate_the_sdk_that_ships_it() {
        // A feature is usable the moment it lands, so its group can never be
        // ahead of the crate version — that would make the SDK fail to satisfy
        // its own registry.
        for f in features() {
            assert!(
                is_compatible(&SDK_VERSION, &f.since),
                "feature `{}` since {} is not satisfied by this SDK ({})",
                f.name,
                f.since,
                *SDK_VERSION
            );
        }
    }

    #[test]
    fn registry_groups_are_ordered_and_distinct() {
        // The grouping only pays off if it stays a changelog: one entry per
        // release, oldest first.
        let versions: Vec<&Version> = REGISTRY.iter().map(|(v, _)| v).collect();
        for pair in versions.windows(2) {
            assert!(pair[0] < pair[1], "groups must be strictly increasing");
        }
    }

    #[test]
    fn reserved_names_are_the_ones_not_supplied_through_values() {
        // `process` gates template-scoped values on this. A predicate or struct
        // that fell out of the registry would silently become shadowable; a
        // plain value wrongly listed here would stop being registered at all,
        // which is how `project_name` briefly went missing during the `chip`
        // namespacing.
        for f in features() {
            let bypasses_values = matches!(f.kind, FeatureKind::Predicate | FeatureKind::Struct);
            assert_eq!(
                is_reserved_name(f.name),
                bypasses_values,
                "`{}` ({:?}) is reserved iff it bypasses `Facts::values`",
                f.name,
                f.kind
            );
        }
        assert!(is_reserved_name("chip"), "the chip struct must be reserved");
        assert!(
            !is_reserved_name("project_name"),
            "a plain value is supplied *through* `values` and must stay registerable"
        );
        assert!(!is_reserved_name("definitely_not_a_fact"));
    }

    #[test]
    fn the_chip_namespace_replaced_the_capability_predicate() {
        // Chip capabilities are `chip.<symbol>` fields now, so somni reports a
        // typo itself. These names must not come back as separate facts.
        for gone in ["chip_has", "is_xtensa", "is_riscv", "rust_target"] {
            assert!(
                feature(gone).is_none(),
                "`{gone}` was folded into the `chip` struct"
            );
        }
    }

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
    }

    #[test]
    fn a_breaking_bump_is_not_compatible_in_either_direction() {
        // The major *is* the spec version, which is why the separate integer
        // was redundant.
        assert!(!is_compatible(&release(2, 0, 0), &release(1, 9, 9)));
        assert!(!is_compatible(&release(1, 9, 9), &release(2, 0, 0)));

        // Below 1.0 the minor carries breaking changes, so `0.1` and `0.2` are
        // as incompatible as `1.x` and `2.x`. This is the regime the SDK is in
        // today, so getting it wrong would be a live bug, not a future one.
        assert!(!is_compatible(&release(0, 2, 0), &release(0, 1, 0)));
        assert!(!is_compatible(&release(0, 1, 0), &release(0, 2, 0)));
        assert!(is_compatible(&release(0, 1, 5), &release(0, 1, 2)));

        // `0.0.x` releases are each their own range.
        assert!(!is_compatible(&release(0, 0, 2), &release(0, 0, 1)));
        assert!(is_compatible(&release(0, 0, 1), &release(0, 0, 1)));
    }

    #[test]
    fn a_newer_sdk_runs_an_older_template_but_not_the_reverse() {
        // "A template declaring x.y is compatible with x.y+n" — the review's
        // rule, which is the whole gate now.
        assert!(is_compatible(&release(1, 4, 0), &release(1, 2, 0)));
        assert!(is_compatible(&release(1, 2, 0), &release(1, 2, 0)));
        assert!(!is_compatible(&release(1, 2, 0), &release(1, 4, 0)));
    }

    #[test]
    fn a_prerelease_of_the_required_version_is_not_new_enough() {
        // The reason contract versions use semver rather than a stripped
        // `major.minor.patch`: an rc of 1.2.0 may not yet implement the feature
        // that a 1.2.0 requirement is gating.
        let rc = Version::parse("1.2.0-rc.1").unwrap();
        assert!(
            rc < release(1, 2, 0),
            "rc must sort below its final release"
        );
        assert!(!is_compatible(&rc, &release(1, 2, 0)));

        // An rc of a *later* release is fine — it contains everything 1.2.0
        // had. This is where we deliberately diverge from `VersionReq`, which
        // would reject it and lock rc users out of every template.
        assert!(is_compatible(
            &Version::parse("1.3.0-rc.1").unwrap(),
            &release(1, 2, 0)
        ));

        // Prereleases order among themselves.
        assert!(Version::parse("1.2.0-rc.1").unwrap() < Version::parse("1.2.0-rc.2").unwrap());
        assert!(Version::parse("1.2.0-alpha").unwrap() < Version::parse("1.2.0-beta").unwrap());

        // Build metadata: semver §10 excludes it from precedence, but the
        // crate *derives* `Ord`, so it does participate — `1.2.0+a` sorts above
        // `1.2.0`. Harmless here, because empty build metadata always sorts
        // first: a `+build` suffix can only ever help a version qualify, never
        // block it. Pinned so the deviation is recorded rather than latent.
        let with_build = Version::parse("1.2.0+a").unwrap();
        assert!(is_compatible(&with_build, &release(1, 2, 0)));
        assert!(with_build > release(1, 2, 0));
        assert_ne!(with_build, release(1, 2, 0));
    }

    #[test]
    fn every_feature_name_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for f in features() {
            assert!(seen.insert(f.name), "duplicate feature name: {}", f.name);
        }
    }

    #[test]
    fn implemented_predicates_are_registered() {
        for name in ["option", "group_selected", "has_reserved_pins"] {
            assert!(
                matches!(feature(name), Some(f) if f.kind == FeatureKind::Predicate),
                "predicate `{name}` is not registered as a contract feature"
            );
        }
        assert!(
            matches!(feature("chip"), Some(f) if f.kind == FeatureKind::Struct),
            "`chip` is a namespace, not a scalar value"
        );
    }

    #[test]
    fn the_language_is_not_this_registrys_business() {
        // Directive keywords are owned by the templating engine and versioned
        // by its own dependency semver, so they must not reappear here.
        for name in ["IF", "ELIF", "ELSE", "ENDIF", "REPLACE", "for", "include"] {
            assert!(
                feature(name).is_none(),
                "`{name}` is language surface — version it via the engine dependency"
            );
        }
    }

    #[test]
    fn a_template_using_no_contract_features_requires_nothing() {
        // Not `Some(0.0.0)`: that sentinel used to flow into `is_compatible`,
        // whose range check then declared a 1.2.0 SDK incompatible with a
        // template that asks for nothing at all.
        assert_eq!(min_sdk_version([]), None);
        assert!(!is_compatible(&release(1, 2, 0), &release(0, 0, 0)));
        // Unknown names are a `check` hard error, handled elsewhere.
        assert_eq!(min_sdk_version(["totally_made_up"]), None);
    }

    #[test]
    fn the_requirement_rises_to_the_newest_feature_used() {
        let baseline = min_sdk_version(["option", "chip_has"]).unwrap();
        assert_eq!(baseline, feature_since("option").unwrap());

        // Synthetic newer feature: the requirement must climb to it without
        // coupling the test to a real future feature.
        let later = release(0, 9, 0);
        let computed = [feature_since("option").unwrap(), later.clone()]
            .into_iter()
            .max()
            .unwrap();
        assert_eq!(computed, later);
        assert!(computed > baseline);
    }
}
