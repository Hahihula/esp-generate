use std::collections::{BTreeSet, HashMap, HashSet};

use esp_metadata_generated::{MemoryRegion, PinInfo};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

pub mod builtin;
pub mod cargo;
pub mod source;

pub use esp_template_sdk::{config, contract, process, template};
pub use source::TemplateSource;

/// Build-script-generated `TEMPLATE_FILES` array mapping each file under
/// `template/` to its baked-in contents. Kept `pub` so xtask (and any other
/// consumer that needs to resolve `!Include` paths against the bundled
/// template tree) can share the same source-of-truth as the binary.
pub mod template_files;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    strum::EnumIter,
    strum::Display,
    strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum Chip {
    Esp32,
    Esp32c2,
    Esp32c3,
    Esp32c5,
    Esp32c6,
    Esp32c61,
    Esp32h2,
    Esp32s2,
    Esp32s3,
}
impl Chip {
    pub fn metadata(self) -> esp_metadata_generated::Chip {
        match self {
            Chip::Esp32 => esp_metadata_generated::Chip::Esp32,
            Chip::Esp32c2 => esp_metadata_generated::Chip::Esp32c2,
            Chip::Esp32c3 => esp_metadata_generated::Chip::Esp32c3,
            Chip::Esp32c5 => esp_metadata_generated::Chip::Esp32c5,
            Chip::Esp32c6 => esp_metadata_generated::Chip::Esp32c6,
            Chip::Esp32c61 => esp_metadata_generated::Chip::Esp32c61,
            Chip::Esp32h2 => esp_metadata_generated::Chip::Esp32h2,
            Chip::Esp32s2 => esp_metadata_generated::Chip::Esp32s2,
            Chip::Esp32s3 => esp_metadata_generated::Chip::Esp32s3,
        }
    }

    pub fn dram2_region(self) -> &'static MemoryRegion {
        self.metadata()
            .memory_layout()
            .region("dram2_uninit")
            .expect("All chips should have a dram2_uninit region")
    }

    pub fn pins(self) -> &'static [PinInfo] {
        self.metadata().pins()
    }

    /// The chip's name as a person writes it.
    ///
    /// Not derivable from the variant: `Esp32c61` is "ESP32-C61", and the
    /// hyphenation doesn't follow from the kebab-case option name either.
    pub fn display_name(self) -> &'static str {
        match self {
            Chip::Esp32 => "ESP32",
            Chip::Esp32c2 => "ESP32-C2",
            Chip::Esp32c3 => "ESP32-C3",
            Chip::Esp32c5 => "ESP32-C5",
            Chip::Esp32c6 => "ESP32-C6",
            Chip::Esp32c61 => "ESP32-C61",
            Chip::Esp32h2 => "ESP32-H2",
            Chip::Esp32s2 => "ESP32-S2",
            Chip::Esp32s3 => "ESP32-S3",
        }
    }

    /// The Wokwi board this chip simulates as, if Wokwi supports it.
    ///
    /// Wokwi support is not a chip *capability* — it's a property of an
    /// external simulator — so it can't come from `esp-metadata`. It rides
    /// along with the chip because it is keyed by chip and nothing else.
    pub fn wokwi_board(self) -> Option<&'static str> {
        match self {
            Chip::Esp32 => Some("board-esp32-devkit-c-v4"),
            Chip::Esp32c3 => Some("board-esp32-c3-devkitm-1"),
            Chip::Esp32c6 => Some("board-esp32-c6-devkitc-1"),
            Chip::Esp32h2 => Some("board-esp32-h2-devkitm-1"),
            Chip::Esp32s2 => Some("board-esp32-s2-devkitm-1"),
            Chip::Esp32s3 => Some("board-esp32-s3-devkitc-1"),
            Chip::Esp32c2 | Chip::Esp32c5 | Chip::Esp32c61 => None,
        }
    }

    /// The chip-derived half of [`process::Facts`]: everything that depends on
    /// which chip is selected and on nothing else.
    ///
    /// These facts have to be live during *configuration*, not just during
    /// generation: [`config::ActiveConfiguration`] gates `requires_capabilities`
    /// on them, and treats absent facts as unconstrained. Generation layers the
    /// selection- and project-scoped values on top of the same base, so the
    /// capability set the option tree was filtered by is by construction the
    /// one `chip.…` later sees.
    pub fn facts(self) -> process::Facts {
        process::Facts {
            chip: Some(self.chip_fields()),
            ..Default::default()
        }
    }

    /// The `chip` struct a template sees: `chip.name`, `chip.rust_target`,
    /// `chip.dram2_uninit_size`, and one field per `esp-metadata` symbol.
    ///
    /// The symbol fields are the **union over every chip**, not just this
    /// chip's. somni reports an unknown struct field as an error, which is the
    /// diagnostic we want for a typo — but it means a capability this chip
    /// merely lacks must still be *present* and `false`, or a portable
    /// `#%if chip.soc_has_wifi` would fail on an ESP32-H2 instead of taking its
    /// else branch.
    fn chip_fields(self) -> IndexMap<String, process::FactValue> {
        // A symbol is either a bare flag (`riscv`) or a `name="value"` pair
        // (`bt_controller="npl"`). Classify by name across *all* chips so a
        // field's type doesn't change with the selection: a valued symbol is
        // always a string, empty when this chip doesn't declare it.
        let mut valued: HashSet<&'static str> = HashSet::new();
        let mut every_symbol: BTreeSet<&'static str> = BTreeSet::new();
        for chip in Chip::iter() {
            for symbol in chip.metadata().all_symbols() {
                match symbol.split_once('=') {
                    Some((name, _)) => {
                        valued.insert(name);
                        every_symbol.insert(name);
                    }
                    None => {
                        every_symbol.insert(symbol);
                    }
                }
            }
        }

        // What *this* chip declares, indexed the same way.
        let metadata = self.metadata();
        let mut mine: HashMap<&str, Option<&str>> = HashMap::new();
        for symbol in metadata.all_symbols() {
            match symbol.split_once('=') {
                Some((name, value)) => {
                    mine.insert(name, Some(value.trim_matches('"')));
                }
                None => {
                    mine.insert(symbol, None);
                }
            }
        }

        let mut fields = IndexMap::new();
        fields.insert("name".to_string(), self.to_string().into());
        fields.insert("rust_target".to_string(), metadata.target().into());
        fields.insert(
            "dram2_uninit_size".to_string(),
            self.dram2_region().size().into(),
        );

        for symbol in every_symbol {
            let value: process::FactValue = if valued.contains(symbol) {
                mine.get(symbol).and_then(|v| *v).unwrap_or("").into()
            } else {
                mine.contains_key(symbol).into()
            };
            fields.insert(symbol.to_string(), value);
        }

        fields
    }
}

/// This turns a list of strings into a sentence, and appends it to the base string.
///
/// # Example
///
/// ```rust
/// # use esp_generate::append_list_as_sentence;
/// let list = &["foo", "bar", "baz"];
/// let sentence = append_list_as_sentence("Here is a sentence.", "My elements are", list);
/// assert_eq!(sentence, "Here is a sentence. My elements are `foo`, `bar` and `baz`.");
///
/// let list = &["foo", "bar", "baz"];
/// let sentence = append_list_as_sentence("The following list is problematic:", "", list);
/// assert_eq!(sentence, "The following list is problematic: `foo`, `bar` and `baz`.");
/// ```
pub fn append_list_as_sentence<S: AsRef<str>>(base: &str, word: &str, els: &[S]) -> String {
    if !els.is_empty() {
        let mut requires = String::new();

        if !base.is_empty() {
            requires.push_str(base);
            requires.push(' ');
        }

        for (i, r) in els.iter().enumerate() {
            if i == 0 {
                if !word.is_empty() {
                    requires.push_str(word);
                    requires.push(' ');
                }
            } else if i == els.len() - 1 {
                requires.push_str(" and ");
            } else {
                requires.push_str(", ");
            };

            requires.push('`');
            requires.push_str(r.as_ref());
            requires.push('`');
        }
        requires.push('.');

        requires
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod test {
    use strum::IntoEnumIterator;

    use super::*;

    fn fields(chip: Chip) -> IndexMap<String, process::FactValue> {
        chip.facts().chip.expect("a chip always has chip facts")
    }

    /// The capability gate is only as good as the field values handed to it, so
    /// a chip-independent set would silently pass everything.
    #[test]
    fn chip_fields_are_chip_specific() {
        let c6 = fields(Chip::Esp32c6);
        let h2 = fields(Chip::Esp32h2);

        assert_eq!(
            c6.get("soc_has_wifi"),
            Some(&process::FactValue::Bool(true))
        );
        assert_eq!(
            h2.get("soc_has_wifi"),
            Some(&process::FactValue::Bool(false)),
            "ESP32-H2 has no Wi-Fi"
        );
    }

    /// The whole point of the union: a capability the chip *lacks* must still
    /// be a field, or a portable `#%if chip.soc_has_wifi` would be an
    /// unknown-field error on an ESP32-H2 instead of taking its else branch.
    /// Only a genuine typo may be absent.
    #[test]
    fn every_chip_carries_every_chips_symbols() {
        let all: BTreeSet<String> = Chip::iter().flat_map(|c| fields(c).into_keys()).collect();

        for chip in Chip::iter() {
            let mine = fields(chip);
            for symbol in &all {
                assert!(
                    mine.contains_key(symbol),
                    "{chip} is missing the field `{symbol}`"
                );
            }
            assert!(
                !mine.contains_key("soc_has_wfi"),
                "a misspelling must stay absent, so somni can report it"
            );
        }
    }

    #[test]
    fn chip_fields_cover_the_isa_and_the_scalars() {
        for chip in Chip::iter() {
            let f = fields(chip);

            assert_eq!(
                f.get("name"),
                Some(&process::FactValue::Str(chip.to_string())),
                "{chip} must name itself"
            );
            for key in ["rust_target", "dram2_uninit_size"] {
                assert!(f.contains_key(key), "{chip} is missing `{key}`");
            }

            // `xtensa` / `riscv` are ordinary metadata symbols, which is why
            // the purpose-picked `is_xtensa` / `is_riscv` facts could go.
            let xtensa = f.get("xtensa") == Some(&process::FactValue::Bool(true));
            let riscv = f.get("riscv") == Some(&process::FactValue::Bool(true));
            assert_ne!(
                xtensa, riscv,
                "{chip} must be exactly one of Xtensa or RISC-V"
            );
            assert_eq!(xtensa, chip.metadata().is_xtensa());
        }
    }

    /// Some symbols are `name="value"` pairs. Their type must not change with
    /// the selected chip, or a comparison that works on one chip becomes a type
    /// error on another.
    #[test]
    fn valued_symbols_stay_strings_on_every_chip() {
        for chip in Chip::iter() {
            let f = fields(chip);
            assert!(
                matches!(f.get("bt_controller"), Some(process::FactValue::Str(_))),
                "{chip} must expose `bt_controller` as a string"
            );
            // The value is unquoted, not the raw `bt_controller="npl"` symbol.
            if let Some(process::FactValue::Str(v)) = f.get("bt_controller") {
                assert!(!v.contains('"'), "{chip} left quotes in `{v}`");
            }
        }
    }
}
