use esp_metadata_generated::{MemoryRegion, PinInfo};
use serde::{Deserialize, Serialize};

pub mod cargo;

pub use esp_template_sdk::{config, process, template};

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

    /// The chip-derived half of [`process::Facts`]: everything that depends on
    /// which chip is selected and on nothing else.
    pub fn facts(self) -> process::Facts {
        let metadata = self.metadata();
        let mut facts = process::Facts {
            symbols: metadata
                .all_symbols()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            is_xtensa: metadata.is_xtensa(),
            is_riscv: !metadata.is_xtensa(),
            ..Default::default()
        };
        facts.set_value("chip", self.to_string());
        // Int-typed so `#IF dram2_uninit_size > 0` works; `#REPLACE` still
        // splices the decimal form.
        facts.set_value("dram2_uninit_size", self.dram2_region().size());
        facts.set_value("rust_target", metadata.target());
        facts
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

    /// [`Chip::facts`] is what makes `requires_capabilities` enforceable: the
    /// SDK's capability gate is only as good as the symbol set handed to it,
    /// and an empty or chip-independent set would silently pass everything.
    #[test]
    fn chip_facts_carry_a_chip_specific_symbol_set() {
        let c6 = Chip::Esp32c6.facts();
        let h2 = Chip::Esp32h2.facts();

        assert!(c6.symbols.contains("soc_has_wifi"));
        assert!(
            !h2.symbols.contains("soc_has_wifi"),
            "ESP32-H2 has no Wi-Fi; a gate on `soc_has_wifi` must be able to tell it apart from a C6"
        );
    }

    /// The ISA flags and substitution values travel with the symbols, so no
    /// caller can install half the chip's facts and leave `is_xtensa` stale
    /// from a previously selected chip.
    #[test]
    fn chip_facts_are_complete_for_every_chip() {
        for chip in Chip::iter() {
            let facts = chip.facts();

            assert!(!facts.symbols.is_empty(), "{chip} declares no symbols");
            assert_ne!(
                facts.is_xtensa, facts.is_riscv,
                "{chip} must be exactly one of Xtensa or RISC-V"
            );
            assert_eq!(
                facts.values.get("chip"),
                Some(&process::FactValue::Str(chip.to_string())),
                "{chip} must name itself"
            );
            for key in ["dram2_uninit_size", "rust_target"] {
                assert!(facts.values.contains_key(key), "{chip} is missing `{key}`");
            }
        }
    }
}
