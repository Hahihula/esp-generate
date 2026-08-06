//! The template manifest — `metadata.toml`.
//!
//! Read by the **binary**, before the SDK runs. `template.yaml` (the option
//! tree) is the SDK's file; this one is ours, and the split is the parse
//! boundary: the manifest decides *which* files a generated project gets and
//! what they are called, the SDK decides what is *in* them.
//!
//! That division is why `includefile` and `include_as` are not template
//! directives. The templating engine renders a string to a string; it has no
//! concept of a file that should not exist or of an output path, so expressing
//! those as directives put file-lifecycle decisions inside the content layer.
//! Here they are data the binary reads without opening a single template file.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use esp_generate::contract;
use serde::Deserialize;

/// Files that are never emitted, wherever a template puts them.
///
/// Everything under [`RESERVED_DIR`] is template *machinery* — `!Include`
/// fragments for the option tree, and the partials that exist only to be
/// `include`d into a file that is emitted. Making that structural rather than a
/// per-file condition matters: "this is not an output file" is a fact about
/// what the file *is*, and writing it as `when = false` invited the reader to
/// look for the selection that turns it on.
pub const RESERVED_DIR: &str = ".template/";

/// The manifest itself, and the option tree, are the binary's and the SDK's
/// inputs — never output.
pub const MANIFEST_PATH: &str = "metadata.toml";
const OPTION_TREE_PATH: &str = "template.yaml";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// The `esp-template-sdk` version this template is written against.
    pub sdk_version: contract::Version,
    /// Display metadata. Parsed so a manifest declaring it is accepted, and so
    /// the fields are already here for the template picker external templates
    /// will need — nothing reads them yet.
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub description: String,
    /// Per-file emission rules. Absent means "emitted, under its own path".
    #[serde(default)]
    emit: Vec<EmitRule>,
}

#[derive(Debug, Deserialize)]
struct EmitRule {
    /// Source path, template-root-relative.
    path: String,
    /// A somni condition; the file is emitted only when it holds.
    when: Option<String>,
    /// The output path, if it differs from `path`. `{name}` is a literal fact
    /// lookup, not an expression.
    #[serde(rename = "as")]
    output: Option<String>,
}

/// What the binary should do with one template file.
#[derive(Debug, PartialEq, Eq)]
pub enum Emit {
    /// Not an output file at all: machinery, or one of the two manifests.
    Never,
    /// Emitted when `condition` holds (always, if `None`), at `output` — the
    /// source path unless the manifest renamed it.
    When {
        condition: Option<String>,
        output: Option<String>,
    },
}

impl Manifest {
    /// Parse a manifest and check the template is one this binary can run.
    ///
    /// The version gate is enforced *here*, before any template file is
    /// touched, so an incompatible template fails with one clear line instead
    /// of an unknown-fact error part-way through generation.
    pub fn parse(source: &str) -> Result<Self> {
        let manifest: Manifest =
            toml::from_str(source).context("`metadata.toml` is not a valid template manifest")?;

        let sdk = &*contract::SDK_VERSION;
        if !contract::is_compatible(sdk, &manifest.sdk_version) {
            bail!(
                "this template needs esp-template-sdk {}, but this esp-generate has {sdk}",
                manifest.sdk_version
            );
        }

        // `emit` stays a `Vec` rather than a map keyed by path, so the rules
        // keep the order the author wrote them — which is what a future
        // `check` will want to report against, and what keeps a diff of this
        // file readable. Duplicate detection therefore borrows rather than
        // consuming: a keyed collection would give this for free, at the cost
        // of that ordering.
        let mut seen = HashSet::new();
        for rule in &manifest.emit {
            if !seen.insert(rule.path.as_str()) {
                bail!(
                    "`metadata.toml` has more than one `emit` rule for `{}`",
                    rule.path
                );
            }
        }

        Ok(manifest)
    }

    /// What to do with the template file at `path`.
    pub fn emit(&self, path: &str) -> Emit {
        if path == MANIFEST_PATH || path == OPTION_TREE_PATH || path.starts_with(RESERVED_DIR) {
            return Emit::Never;
        }

        match self.emit.iter().find(|rule| rule.path == path) {
            Some(rule) => Emit::When {
                condition: rule.when.clone(),
                output: rule.output.clone(),
            },
            // The default is to emit. Adding an ordinary file to a template
            // should not require touching the manifest, and the failure mode
            // when someone forgets is a file that appears — visible — rather
            // than one that silently vanishes.
            None => Emit::When {
                condition: None,
                output: None,
            },
        }
    }

    /// Check every `emit` rule points at a file that exists.
    ///
    /// A rule whose path is stale — the file was renamed or deleted, and the
    /// manifest wasn't updated — is silently inert: the condition never applies
    /// and the file it was meant to guard is emitted unconditionally, or not at
    /// all. That is the failure this catches, and it is not hypothetical: the
    /// editor dotfiles were missed exactly once during this migration because a
    /// `grep` skipped them.
    pub fn validate_paths(&self, exists: impl Fn(&str) -> bool) -> Result<()> {
        let stale: Vec<&str> = self
            .emit
            .iter()
            .map(|rule| rule.path.as_str())
            .filter(|path| !exists(path))
            .collect();

        if !stale.is_empty() {
            bail!(
                "`metadata.toml` has `emit` rules for files that do not exist: {}",
                stale.join(", ")
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const MINIMAL: &str = r#"
sdk_version = "0.1.0"
"#;

    #[test]
    fn an_unruled_file_is_emitted_under_its_own_path() {
        let manifest = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(
            manifest.emit("src/bin/main.rs"),
            Emit::When {
                condition: None,
                output: None
            }
        );
    }

    #[test]
    fn machinery_is_never_emitted() {
        let manifest = Manifest::parse(MINIMAL).unwrap();

        for path in [
            "metadata.toml",
            "template.yaml",
            ".template/chip.yaml",
            ".template/partials/main_async.rs",
        ] {
            assert_eq!(manifest.emit(path), Emit::Never, "{path}");
        }

        // The rule is a directory prefix, not a substring: a real output file
        // whose name merely contains the reserved name is still emitted.
        assert_ne!(manifest.emit("src/dot.template/x.rs"), Emit::Never);
    }

    #[test]
    fn a_rule_supplies_the_condition_and_the_output_path() {
        let manifest = Manifest::parse(
            r#"
sdk_version = "0.1.0"

[[emit]]
path = "wokwi.toml"
when = 'option("wokwi")'

[[emit]]
path = "GUIDANCE.md"
when = 'group_selected("agents")'
as = "{agent_file}"
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.emit("wokwi.toml"),
            Emit::When {
                condition: Some("option(\"wokwi\")".into()),
                output: None
            }
        );
        assert_eq!(
            manifest.emit("GUIDANCE.md"),
            Emit::When {
                condition: Some("group_selected(\"agents\")".into()),
                output: Some("{agent_file}".into())
            }
        );
    }

    #[test]
    fn an_incompatible_template_is_rejected_up_front() {
        // A breaking bump the running SDK cannot satisfy. Caught before any
        // template file is opened, so the user gets one clear line.
        let err = Manifest::parse("sdk_version = \"99.0.0\"\n")
            .expect_err("a future major must be refused");
        let msg = err.to_string();
        assert!(msg.contains("99.0.0"), "{msg}");
        assert!(msg.contains("esp-template-sdk"), "{msg}");
    }

    #[test]
    fn the_running_sdk_version_is_accepted() {
        let current = contract::SDK_VERSION.to_string();
        Manifest::parse(&format!("sdk_version = \"{current}\"\n"))
            .expect("the SDK must satisfy its own version");
    }

    #[test]
    fn a_manifest_without_a_version_is_not_a_template() {
        // The version is the one key every future binary must find.
        let err = Manifest::parse("name = \"nope\"\n").expect_err("missing version must fail");
        assert!(err.to_string().contains("metadata.toml"), "{err}");
    }

    #[test]
    fn a_rule_for_a_missing_file_is_rejected() {
        let manifest = Manifest::parse(
            r#"
sdk_version = "0.1.0"

[[emit]]
path = "was/renamed.rs"
when = 'option("x")'
"#,
        )
        .unwrap();

        let err = manifest
            .validate_paths(|_| false)
            .expect_err("a stale rule is inert, so it must be loud");
        assert!(err.to_string().contains("was/renamed.rs"), "{err}");

        manifest
            .validate_paths(|_| true)
            .expect("a rule whose file exists is fine");
    }

    #[test]
    fn duplicate_rules_are_rejected() {
        let err = Manifest::parse(
            r#"
sdk_version = "0.1.0"

[[emit]]
path = "wokwi.toml"
when = 'option("wokwi")'

[[emit]]
path = "wokwi.toml"
when = 'false'
"#,
        )
        .expect_err("two rules for one path is ambiguous");
        assert!(err.to_string().contains("wokwi.toml"), "{err}");
    }
}
