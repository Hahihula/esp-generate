//! Binary-provided option-tree fragments — the `builtin:` include scheme.
//!
//! `!Include builtin:<name>` resolves against content **esp-generate provides**
//! rather than a file in the template. It is the option-tree analog of a fact:
//! the SDK defines the hook (an ordinary `!Include` whose path the loader
//! recognises), the binary supplies the current data, so the fragment tracks
//! the installed binary. A template that writes `!Include builtin:chip` gets
//! every chip this binary knows — new silicon appears without a template edit.
//!
//! Note there is no SDK support for this and none is needed: `Template::load`
//! takes a `Fn(&str) -> Option<String>` loader, so `builtin:` is a convention
//! of *this* loader. The SDK stays IO-free and knows nothing about builtins.
//!
//! ## What is contract, and what floats
//!
//! A builtin injects structure that templates reference, so its **referenceable
//! surface** is contract: the option `name`s, each `(name, selection_group)`
//! pairing, and the `sets` keys it emits. A template's `{{ wokwi_board }}` or
//! `compatible: { chip: [...] }` depends on those. Everything else is *data* and
//! floats freely — which chips are in the list, their `display_name`, the `sets`
//! *values*. Additions are the point; a rename or removal is breaking.

use strum::IntoEnumIterator;

use crate::Chip;

/// The prefix that marks an include as binary-provided rather than a file path.
pub const SCHEME: &str = "builtin:";

/// Resolve a `builtin:` include, or `None` if `path` names no builtin.
///
/// Returns YAML in the same shape a template file would contain, so the SDK's
/// `!Include` expansion cannot tell the difference.
pub fn resolve(path: &str) -> Option<String> {
    match path.strip_prefix(SCHEME)? {
        "chip" => Some(chip_category()),
        _ => None,
    }
}

/// The `chip` selection group, generated from the [`Chip`] enum.
///
/// This is why the category is a builtin rather than template YAML: the enum
/// and the option list cannot drift, because there is only one of them. The
/// old hand-authored `chip.yaml` needed a startup validator to assert the two
/// agreed; generating removes the failure mode instead of checking for it.
fn chip_category() -> String {
    let mut yaml = String::from(
        "!Category\n\
         name: chip\n\
         display_name: Chip selection\n\
         help: Target Espressif chip. Switching chips deselects and hides incompatible options.\n\
         options:\n",
    );

    for chip in Chip::iter() {
        yaml.push_str(&format!(
            "  - !Option\n    \
               name: {chip}\n    \
               display_name: {}\n    \
               selection_group: chip\n",
            chip.display_name()
        ));

        if let Some(board) = chip.wokwi_board() {
            yaml.push_str(&format!("    sets:\n      wokwi_board: {board}\n"));
        }
    }

    yaml
}

#[cfg(test)]
mod test {
    use crate::template::{GeneratorOptionItem, Template};

    use super::*;

    /// Parse the fragment the way `Template::load` would, so the test fails on
    /// malformed YAML rather than on some downstream symptom.
    fn chip_options() -> Vec<GeneratorOptionItem> {
        let yaml = format!("required: []\noptions:\n  - !Include {SCHEME}chip\n");
        let template =
            Template::load(&yaml, resolve).expect("the generated chip category must parse");

        let [GeneratorOptionItem::Category(category)] = template.options.as_slice() else {
            panic!("expected exactly one category, got {:?}", template.options);
        };
        assert_eq!(category.name, "chip");
        category.options.clone()
    }

    #[test]
    fn every_chip_the_binary_knows_is_offered() {
        let options = chip_options();
        assert_eq!(options.len(), Chip::iter().count());

        for (item, chip) in options.iter().zip(Chip::iter()) {
            let GeneratorOptionItem::Option(option) = item else {
                panic!("the chip category holds only options");
            };
            assert_eq!(option.name, chip.to_string());
            assert_eq!(option.display_name, chip.display_name());
            assert_eq!(
                option.selection_group, "chip",
                "every chip option is in the `chip` group"
            );
        }
    }

    #[test]
    fn wokwi_boards_ride_along_with_the_chips_that_have_them() {
        let options = chip_options();

        for (item, chip) in options.iter().zip(Chip::iter()) {
            let GeneratorOptionItem::Option(option) = item else {
                unreachable!()
            };
            let board = option.sets.get("wokwi_board").and_then(|v| v.as_scalar());

            match chip.wokwi_board() {
                Some(expected) => assert_eq!(board, Some(expected), "{chip}"),
                None => assert_eq!(board, None, "{chip} has no Wokwi board"),
            }
        }
    }

    #[test]
    fn an_unknown_builtin_is_not_resolved() {
        // So a typo surfaces as a missing include rather than empty content.
        assert!(resolve("builtin:nope").is_none());
        // And an ordinary path is left for the file loader.
        assert!(resolve(".template/module.yaml").is_none());
    }
}
