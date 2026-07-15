//! The file-directive processor — the pure text-templating engine.
//!
//! `process_file` walks a single template file line-by-line, evaluating the
//! `#IF` / `#ELIF` / `#ELSE` / `#ENDIF` / `#INCLUDEFILE` / `#INCLUDE_AS` /
//! `#REPLACE` directives (using the file's comment prefix — `#`, `//`, or
//! `--`) and returns the rendered bytes. It performs **no IO**: the binary
//! reads the template and writes the result.
//!
//! ## The fact `Context`
//!
//! `#IF` / `#INCLUDEFILE` conditions are [somni] boolean expressions; the facts
//! they can reference are registered on one [`somni_expr::Context`] built from
//! [`Facts`] plus the selected option names:
//!
//! - `option(name)` — is that option selected?
//! - `group_selected(group)` — does that selection group have a pick?
//! - `chip_has(symbol)` — does the selected chip declare that metadata symbol?
//! - `is_xtensa` / `is_riscv` — the selected chip's ISA.
//! - `has_reserved_pins` — does the selected module reserve any GPIOs?
//!
//! Every [`Facts::values`] entry whose name is a somni identifier is *also*
//! registered as a somni variable, so `#IF chip == "esp32c6"` and
//! `#IF dram2_uninit_size > 0` work (spec §4's "binary values" table). The
//! predicates above are reserved: a template-scoped value can never shadow one.
//!
//! `#REPLACE` / `#INCLUDE_AS` remain **literal name lookups** into
//! [`Facts::values`] (never somni expressions), so substitution-only names may
//! contain dashes — those simply aren't reachable from `#IF`.
//!
//! [somni]: https://github.com/bugadani/somni

use std::collections::{HashMap, HashSet};

/// Fact names that are always the binary's, never a template's. A `sets` key or
/// `[vars]` entry colliding with one of these is ignored when building the
/// somni context (spec §4: "binary facts are reserved and win on a name
/// clash"). Function-valued facts are listed too — somni keeps functions and
/// variables in separate namespaces, but shadowing one with a value would still
/// be a confusing surprise.
const RESERVED_FACT_NAMES: &[&str] = &[
    "option",
    "group_selected",
    "chip_has",
    "is_xtensa",
    "is_riscv",
    "has_reserved_pins",
];

/// A substitution value. The [`Display`](std::fmt::Display) form is what
/// `#REPLACE` / `#INCLUDE_AS` splice into the output; the variant is what somni
/// sees, so an [`Int`](FactValue::Int) fact supports arithmetic and ordering in
/// `#IF` while a [`Str`](FactValue::Str) supports string equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactValue {
    Str(String),
    Int(u64),
}

impl std::fmt::Display for FactValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FactValue::Str(s) => f.write_str(s),
            FactValue::Int(i) => write!(f, "{i}"),
        }
    }
}

impl From<String> for FactValue {
    fn from(value: String) -> Self {
        FactValue::Str(value)
    }
}

impl From<&str> for FactValue {
    fn from(value: &str) -> Self {
        FactValue::Str(value.to_string())
    }
}

impl From<u64> for FactValue {
    fn from(value: u64) -> Self {
        FactValue::Int(value)
    }
}

impl From<u32> for FactValue {
    fn from(value: u32) -> Self {
        FactValue::Int(value as u64)
    }
}

impl From<usize> for FactValue {
    fn from(value: usize) -> Self {
        FactValue::Int(value as u64)
    }
}

/// Chip-derived facts passed from the binary to the SDK — the single conduit
/// for everything the directive engine needs to know about the target that
/// isn't a user selection.
#[derive(Debug, Default, Clone)]
pub struct Facts {
    /// Metadata symbols the chip declares. Backs `chip_has(symbol)`.
    pub symbols: HashSet<String>,
    /// Substitution values: spliced literally by `#REPLACE` / `#INCLUDE_AS`,
    /// and exposed to `#IF` as somni variables when the name is an identifier.
    pub values: HashMap<String, FactValue>,
    /// Whether the chip's ISA is Xtensa. Backs `is_xtensa`.
    pub is_xtensa: bool,
    /// Whether the chip's ISA is RISC-V. Backs `is_riscv`.
    pub is_riscv: bool,
    /// Whether the selected module reserves any GPIOs. Backs `has_reserved_pins`.
    pub has_reserved_pins: bool,
}

impl Facts {
    /// Insert a value, keeping the first writer on a key clash. Binary facts
    /// are inserted before template-scoped `sets`, so a template can't shadow
    /// `chip`, `rust_target`, etc.
    pub fn set_value(&mut self, key: impl Into<String>, value: impl Into<FactValue>) {
        self.values
            .entry(key.into())
            .or_insert_with(|| value.into());
    }
}

/// Whether `name` can be written as a somni identifier — i.e. whether
/// registering it as a variable makes it referenceable from `#IF`. Dashed
/// substitution-only names (`coding-agent-guidance-file`) are not.
fn is_somni_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Clone, Copy)]
enum BlockKind {
    // All lines are included
    Root,

    // (current branch to be included, any previous branches included)
    IfElse(bool, bool),
}

impl BlockKind {
    fn include_line(self) -> bool {
        match self {
            BlockKind::Root => true,
            BlockKind::IfElse(current, any) => current && !any,
        }
    }

    fn new_if(current: bool) -> BlockKind {
        BlockKind::IfElse(current, false)
    }

    fn into_else_if(self, condition: bool) -> BlockKind {
        let BlockKind::IfElse(previous, any) = self else {
            panic!("ELIF without IF");
        };
        BlockKind::IfElse(condition, any || previous)
    }

    fn into_else(self) -> BlockKind {
        let BlockKind::IfElse(previous, any) = self else {
            panic!("ELSE without IF");
        };
        BlockKind::IfElse(!any, any || previous)
    }
}

/// Build the somni evaluation context exposing the fact predicates over
/// `selected` / `selected_groups` / `facts`. Kept separate so tests can
/// exercise the same surface.
///
/// The option and selection-group namespaces are **disjoint**: `option(name)`
/// only ever sees option names and `group_selected(group)` only ever sees
/// group names, so an option and a group sharing a name (the bundled template's
/// `coding-agent-guidance` is both a category and a group) can't be confused
/// for one another.
fn build_context<'a>(
    selected: &'a [String],
    selected_groups: &'a [String],
    facts: &'a Facts,
) -> somni_expr::Context<'a> {
    let mut engine = somni_expr::Context::new();

    // Template-scoped values go in first so the reserved registrations below
    // always win; names that aren't somni identifiers stay `#REPLACE`-only.
    for (name, value) in &facts.values {
        if !is_somni_identifier(name) || RESERVED_FACT_NAMES.contains(&name.as_str()) {
            continue;
        }
        match value {
            FactValue::Str(s) => engine.add_variable::<&str>(name, s.as_str()),
            FactValue::Int(i) => engine.add_variable::<u64>(name, *i),
        }
    }

    engine.add_function("option", move |cond: &str| -> bool {
        selected.iter().any(|c| c == cond)
    });

    engine.add_function("group_selected", move |group: &str| -> bool {
        selected_groups.iter().any(|g| g == group)
    });

    engine.add_function("chip_has", move |symbol: &str| -> bool {
        facts.symbols.contains(symbol)
    });

    engine.add_variable::<bool>("is_xtensa", facts.is_xtensa);
    engine.add_variable::<bool>("is_riscv", facts.is_riscv);
    engine.add_variable::<bool>("has_reserved_pins", facts.has_reserved_pins);

    engine
}

/// A directive-processing failure carrying the 1-based source line, so the
/// binary can surface `file:line: message`. Malformed directives, bad `#IF`
/// conditions, unbalanced `#IF`/`#ENDIF`, and target-escaping `#INCLUDE_AS`
/// paths are all hard errors rather than panics or silent no-ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    /// 1-based line number in the source template file.
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ProcessError {}

/// Whether `path` is a contained relative path: not absolute and free of any
/// `..` or drive-letter component. Used to reject `#INCLUDE_AS`/output paths
/// that would let generation write outside the target directory.
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.starts_with(['/', '\\']) {
        return false; // absolute (Unix or Windows)
    }
    if path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(1) == Some(&b':')
    {
        return false; // drive-relative, e.g. `C:foo`
    }
    path.split(['/', '\\']).all(|part| part != "..")
}

/// Process a single template file, returning the rendered contents, or `None`
/// if an `#INCLUDEFILE` directive excluded the file entirely.
///
/// `file_path` is updated in place if the file carries an `#INCLUDE_AS`
/// directive; the rewritten path is validated to stay inside the target
/// directory. Malformed directives are reported as [`ProcessError`] with the
/// offending line, never panicked.
///
/// A key absent from [`Facts::values`] is intentionally left unsubstituted
/// (the literal placeholder survives) — that's the template's "override or
/// keep the default" idiom, e.g. `rust-toolchain.toml` keeps `channel = "stable"`
/// unless an explicit `rust_toolchain` was chosen. Distinguishing an optionally-
/// absent value from a typo needs the full declared-key universe, which only
/// the binary's `check` command has; flagging unknown keys is `check`'s job.
pub fn process_file(
    contents: &str,             // Raw content of the file
    selected: &[String],        // Selected option names
    selected_groups: &[String], // Selection groups that have a pick
    facts: &Facts,              // Chip-derived facts + substitution values
    file_path: &mut String,     // File path to be modified
) -> Result<Option<String>, ProcessError> {
    let mut res = String::new();

    let mut replace: Option<Vec<(&str, String)>> = None;
    // Each open block tracks the line it opened on, so unclosed `#IF`s can be
    // reported at their source. The base `Root` frame (line 0) is never popped.
    let mut include: Vec<(BlockKind, usize)> = vec![(BlockKind::Root, 0)];
    let mut file_directives = true;

    let mut engine = build_context(selected, selected_groups, facts);

    let mut include_file = true;

    for (line_no, line) in contents.lines().enumerate() {
        let line_no = line_no + 1;
        let trimmed: &str = line.trim();

        // We check for the first line to see if we should include the file
        if file_directives {
            // Determine if the line starts with a known include directive
            if let Some(cond) = trimmed
                .strip_prefix("//INCLUDEFILE ")
                .or_else(|| trimmed.strip_prefix("#INCLUDEFILE "))
                .or_else(|| trimmed.strip_prefix("--INCLUDEFILE "))
            {
                include_file = eval_bool(&mut engine, cond, line_no, "#INCLUDEFILE")?;
                continue;
            } else if let Some(include_as) = trimmed
                .strip_prefix("//INCLUDE_AS ")
                .or_else(|| trimmed.strip_prefix("#INCLUDE_AS "))
                .or_else(|| trimmed.strip_prefix("--INCLUDE_AS "))
            {
                let mut include_as = include_as.trim().to_string();
                for (key, value) in &facts.values {
                    include_as = include_as.replace(&format!("{{{key}}}"), &value.to_string());
                }
                if !is_safe_relative_path(&include_as) {
                    return Err(ProcessError {
                        line: line_no,
                        message: format!(
                            "`#INCLUDE_AS` path `{include_as}` escapes the target directory \
                             (absolute or `..` paths are not allowed)"
                        ),
                    });
                }
                *file_path = include_as;
                continue;
            }
        }
        if !include_file {
            return Ok(None);
        }

        file_directives = false;

        // that's a bad workaround
        if trimmed == "#[rustfmt::skip]" {
            log::info!("Skipping rustfmt");
            continue;
        }

        // Check if we should replace the next line with the key/value of a variable
        if let Some(what) = trimmed
            .strip_prefix("#REPLACE ")
            .or_else(|| trimmed.strip_prefix("//REPLACE "))
            .or_else(|| trimmed.strip_prefix("--REPLACE "))
        {
            let replacements = what
                .split(" && ")
                .filter_map(|pair| {
                    let mut parts = pair.split_whitespace();
                    if let (Some(pattern), Some(var_name)) = (parts.next(), parts.next()) {
                        // Missing key → leave unsubstituted (override-or-default
                        // idiom; `check` flags typos).
                        facts
                            .values
                            .get(var_name)
                            .map(|value| (pattern, value.to_string()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            if !replacements.is_empty() {
                replace = Some(replacements);
            }
        // Check if we should include the next line(s)
        } else if let Some(cond) = strip_directive(trimmed, "IF ") {
            let active = include.last().unwrap().0.include_line();

            // Only evaluate condition if this IF is in a branch that should be included
            let current = if active {
                eval_bool(&mut engine, cond, line_no, "#IF")?
            } else {
                false
            };

            include.push((BlockKind::new_if(current), line_no));
        } else if let Some(cond) = strip_directive(trimmed, "ELIF ") {
            let (last, open_line) = pop_block(&mut include, line_no, "#ELIF")?;

            // Only evaluate condition if no other branches evaluated to true
            let current = if matches!(last, BlockKind::IfElse(false, false)) {
                eval_bool(&mut engine, cond, line_no, "#ELIF")?
            } else {
                false
            };

            include.push((last.into_else_if(current), open_line));
        } else if starts_with_directive(trimmed, "ELSE") {
            let (last, open_line) = pop_block(&mut include, line_no, "#ELSE")?;
            include.push((last.into_else(), open_line));
        } else if starts_with_directive(trimmed, "ENDIF") {
            pop_block(&mut include, line_no, "#ENDIF")?;
        // Trim #+ and //+
        } else if include.iter().all(|(b, _)| b.include_line()) {
            let mut line = line.to_string();

            if trimmed.starts_with("#+") {
                line = line.replace("#+", "");
            }

            if trimmed.starts_with("//+") {
                line = line.replace("//+", "");
            }

            if trimmed.starts_with("--+") {
                line = line.replace("--+", "");
            }

            if let Some(replacements) = &replace {
                for (pattern, value) in replacements {
                    line = line.replace(pattern, value);
                }
            }

            res.push_str(&line);
            res.push('\n');

            replace = None;
        }
    }

    if let Some((_, open_line)) = include.get(1) {
        return Err(ProcessError {
            line: *open_line,
            message: "unclosed `#IF` block (missing `#ENDIF`)".to_string(),
        });
    }

    Ok(Some(res))
}

/// Strip a directive keyword (with any of the `#`/`//`/`--` prefixes) off the
/// front of a trimmed line. `keyword` includes the trailing space for
/// argument-taking directives (e.g. `"IF "`).
fn strip_directive<'a>(trimmed: &'a str, keyword: &str) -> Option<&'a str> {
    for prefix in ["#", "//", "--"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if let Some(arg) = rest.strip_prefix(keyword) {
                return Some(arg);
            }
        }
    }
    None
}

/// Whether a trimmed line is a bare directive (`#ELSE`, `#ENDIF`, …) under any
/// comment prefix.
fn starts_with_directive(trimmed: &str, keyword: &str) -> bool {
    ["#", "//", "--"]
        .iter()
        .filter_map(|p| trimmed.strip_prefix(p))
        .any(|rest| rest.starts_with(keyword))
}

/// Evaluate a somni boolean condition; turn evaluation failures into a
/// `file:line` hard error instead of a panic.
fn eval_bool(
    engine: &mut somni_expr::Context<'_>,
    cond: &str,
    line: usize,
    directive: &str,
) -> Result<bool, ProcessError> {
    engine.evaluate::<bool>(cond).map_err(|e| ProcessError {
        line,
        // `ExpressionError`'s `Debug` is a multi-line, ANSI-coloured caret
        // diagram whose line numbers are relative to the expression fragment,
        // not the file — useless nested inside a one-line `file:line` message.
        // The inner `EvalError` carries the bare reason, which is what we want.
        message: format!(
            "invalid `{directive}` condition `{cond}`: {}",
            e.into_inner().message
        ),
    })
}

/// Pop the innermost open block for an `#ELIF`/`#ELSE`/`#ENDIF`, or report a
/// `file:line` error if there is no matching `#IF`.
fn pop_block(
    include: &mut Vec<(BlockKind, usize)>,
    line: usize,
    directive: &str,
) -> Result<(BlockKind, usize), ProcessError> {
    if include.len() <= 1 {
        return Err(ProcessError {
            line,
            message: format!("`{directive}` without matching `#IF`"),
        });
    }
    Ok(include.pop().unwrap())
}

#[cfg(test)]
mod test {
    use super::*;

    /// Process with a selected-name list and no chip facts. Expects success.
    fn process(contents: &str, selected: &[&str]) -> Option<String> {
        let selected: Vec<String> = selected.iter().map(|s| s.to_string()).collect();
        process_file(
            contents,
            &selected,
            &[],
            &Facts::default(),
            &mut String::from("main.rs"),
        )
        .expect("process_file should succeed")
    }

    /// Process with explicit option *and* group selections.
    fn process_with_groups(contents: &str, selected: &[&str], groups: &[&str]) -> Option<String> {
        let selected: Vec<String> = selected.iter().map(|s| s.to_string()).collect();
        let groups: Vec<String> = groups.iter().map(|s| s.to_string()).collect();
        process_file(
            contents,
            &selected,
            &groups,
            &Facts::default(),
            &mut String::from("main.rs"),
        )
        .expect("process_file should succeed")
    }

    #[test]
    fn test_nested_if_else1() {
        let res = process(
            r#"
        #IF option("opt1")
        opt1
        #IF option("opt2")
        opt2
        #ELSE
        !opt2
        #ENDIF
        #ELSE
        !opt1
        #ENDIF
        "#,
            &["opt1", "opt2"],
        )
        .unwrap();

        assert_eq!(
            r#"
        opt1
        opt2
        "#
            .trim(),
            res.trim()
        );
    }

    #[test]
    fn test_nested_if_else2() {
        let res = process(
            r#"
        #IF option("opt1")
        opt1
        #IF option("opt2")
        opt2
        #ELSE
        !opt2
        #ENDIF
        #ELSE
        !opt1
        #ENDIF
        "#,
            &[],
        )
        .unwrap();

        assert_eq!(
            r#"
        !opt1
        "#
            .trim(),
            res.trim()
        );
    }

    #[test]
    fn test_nested_if_else3() {
        let res = process(
            r#"
        #IF option("opt1")
        opt1
        #IF option("opt2")
        opt2
        #ELSE
        !opt2
        #ENDIF
        #ELSE
        !opt1
        #ENDIF
        "#,
            &["opt1"],
        )
        .unwrap();

        assert_eq!(
            r#"
        opt1
        !opt2
        "#
            .trim(),
            res.trim()
        );
    }

    #[test]
    fn test_nested_if_else4() {
        let res = process(
            r#"
        #IF option("opt1")
        #IF option("opt2")
        opt2
        #ELSE
        !opt2
        #ENDIF
        opt1
        #ENDIF
        "#,
            &["opt1"],
        )
        .unwrap();

        assert_eq!(
            r#"
        !opt2
        opt1
        "#
            .trim(),
            res.trim()
        );
    }

    #[test]
    fn test_nested_if_else5() {
        let res = process(
            r#"
        #IF option("opt1")
        #IF option("opt2")
        opt2
        #ELSE
        !opt2
        #ENDIF
        opt1
        #ENDIF
        "#,
            &["opt2"],
        )
        .unwrap();

        assert_eq!(r#""#.trim(), res.trim());
    }

    #[test]
    fn test_basic_elseif() {
        let template = r#"
        #IF option("opt1")
        opt1
        #ELIF option("opt2")
        opt2
        #ELIF option("opt3")
        opt3
        #ELSE
        opt4
        #ENDIF
        "#;

        const PAIRS: &[(&[&str], &str)] = &[
            (&["opt1"], "opt1"),
            (&["opt1", "opt2"], "opt1"),
            (&["opt1", "opt3"], "opt1"),
            (&["opt1", "opt2", "opt3"], "opt1"),
            (&["opt2"], "opt2"),
            (&["opt2", "opt3"], "opt2"),
            (&["opt3"], "opt3"),
            (&["opt4"], "opt4"),
            (&[], "opt4"),
        ];

        for (options, expected) in PAIRS {
            let res = process(template, options).unwrap();
            assert_eq!(*expected, res.trim(), "options: {options:?}");
        }
    }

    #[test]
    fn replace_uses_facts_values_literal_lookup() {
        // `#REPLACE` is a literal name lookup, so dashed names work.
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        let res = process_file(
            "#REPLACE PLACEHOLDER chip\nfeatures = [\"PLACEHOLDER\"]\n",
            &[],
            &[],
            &facts,
            &mut String::from("Cargo.toml"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(res.trim(), r#"features = ["esp32c6"]"#);
    }

    #[test]
    fn include_as_interpolates_values_and_rewrites_path() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        let mut path = String::from("src/chip.rs");
        let res = process_file(
            "#INCLUDE_AS src/{chip}.rs\nfn main() {}\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(path, "src/esp32c6.rs");
        assert_eq!(res.trim(), "fn main() {}");
    }

    #[test]
    fn chip_has_and_isa_predicates_drive_if() {
        let mut facts = Facts::default();
        facts.symbols.insert("soc_has_wifi".to_string());
        facts.is_xtensa = true;
        facts.is_riscv = false;

        let template = r#"
        #IF chip_has("soc_has_wifi")
        has-wifi
        #ENDIF
        #IF is_xtensa
        xtensa
        #ENDIF
        #IF is_riscv
        riscv
        #ENDIF
        #IF chip_has("soc_has_bt")
        has-bt
        #ENDIF
        "#;
        let res = process_file(template, &[], &[], &facts, &mut String::from("m.rs"))
            .unwrap()
            .unwrap();
        let out = res.trim();
        assert!(out.contains("has-wifi"), "{out}");
        assert!(out.contains("xtensa"), "{out}");
        assert!(!out.contains("riscv"), "{out}");
        assert!(!out.contains("has-bt"), "{out}");
    }

    #[test]
    fn group_selected_reads_selection_group_names() {
        let template = r#"
        #IF group_selected("chip")
        chip-picked
        #ENDIF
        "#;
        let res = process_with_groups(template, &["esp32c6"], &["chip"]).unwrap();
        assert_eq!(res.trim(), "chip-picked");
    }

    #[test]
    fn option_and_group_namespaces_are_disjoint() {
        // The bundled template has a name that is BOTH a category and a
        // selection group (`coding-agent-guidance`); the two predicates must
        // not answer for each other.
        let template = r#"
        #IF option("coding-agent-guidance")
        option-hit
        #ENDIF
        #IF group_selected("coding-agent-guidance")
        group-hit
        #ENDIF
        #IF option("claude")
        claude-hit
        #ENDIF
        #IF group_selected("claude")
        claude-group-hit
        #ENDIF
        "#;
        let out = process_with_groups(template, &["claude"], &["coding-agent-guidance"]).unwrap();
        let out = out.trim();
        // A group name is not an option...
        assert!(!out.contains("option-hit"), "{out}");
        assert!(out.contains("group-hit"), "{out}");
        // ...and an option name is not a group.
        assert!(out.contains("claude-hit"), "{out}");
        assert!(!out.contains("claude-group-hit"), "{out}");
    }

    #[test]
    fn values_are_readable_as_somni_variables() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        facts.set_value("dram2_uninit_size", 1024u64);

        let template = r#"
        #IF chip == "esp32c6"
        is-c6
        #ENDIF
        #IF chip == "esp32"
        is-esp32
        #ENDIF
        #IF dram2_uninit_size > 0
        has-dram2
        #ENDIF
        #IF dram2_uninit_size > 4096
        big-dram2
        #ENDIF
        "#;
        let out = process_file(template, &[], &[], &facts, &mut String::from("m.rs"))
            .unwrap()
            .unwrap();
        let out = out.trim();
        assert!(out.contains("is-c6"), "{out}");
        assert!(!out.contains("is-esp32"), "{out}");
        assert!(out.contains("has-dram2"), "{out}");
        assert!(!out.contains("big-dram2"), "{out}");
    }

    #[test]
    fn int_values_substitute_as_decimal_and_compare_as_numbers() {
        // The same fact must read as a number in `#IF` and splice as its
        // decimal form in `#REPLACE`.
        let mut facts = Facts::default();
        facts.set_value("dram2_uninit_size", 32768u64);
        let out = process_file(
            "#REPLACE SIZE dram2_uninit_size\nlen = SIZE;\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "len = 32768;");
    }

    #[test]
    fn dashed_value_names_stay_replace_only() {
        // Not a somni identifier → usable in `#REPLACE`, invisible to `#IF`.
        let mut facts = Facts::default();
        facts.set_value("coding-agent-guidance-file", "CLAUDE.md");

        let out = process_file(
            "#REPLACE FILE coding-agent-guidance-file\nsee FILE\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "see CLAUDE.md");

        // Referencing it from `#IF` is a hard error, not a silent false.
        let err = process_err("#IF coding-agent-guidance-file\nx\n#ENDIF\n", &facts);
        assert!(err.message.contains("invalid `#IF` condition"), "{err}");
    }

    #[test]
    fn templates_cannot_shadow_reserved_facts() {
        // A `sets` key colliding with a binary predicate must not win.
        let facts = Facts {
            is_xtensa: false,
            values: HashMap::from([("is_xtensa".to_string(), FactValue::Str("yes".to_string()))]),
            ..Default::default()
        };

        let out = process_file(
            "#IF is_xtensa\nshadowed\n#ELSE\nreserved-wins\n#ENDIF\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "reserved-wins");
    }

    #[test]
    fn somni_identifier_classifies() {
        assert!(is_somni_identifier("chip"));
        assert!(is_somni_identifier("_private"));
        assert!(is_somni_identifier("dram2_uninit_size"));
        assert!(!is_somni_identifier("coding-agent-guidance-file"));
        assert!(!is_somni_identifier("2fast"));
        assert!(!is_somni_identifier(""));
        assert!(!is_somni_identifier("has space"));
    }

    /// Returns the `ProcessError` or panics if the call unexpectedly succeeded.
    fn process_err(contents: &str, facts: &Facts) -> ProcessError {
        process_file(contents, &[], &[], facts, &mut String::from("f.rs"))
            .expect_err("expected a ProcessError")
    }

    #[test]
    fn include_as_rejects_escaping_paths() {
        let mut facts = Facts::default();
        facts.set_value("evil", "../../etc/passwd");

        for bad in [
            "#INCLUDE_AS /etc/passwd\nx\n",
            "#INCLUDE_AS ../outside.rs\nx\n",
            "#INCLUDE_AS ../../etc/passwd\nx\n",
            "#INCLUDE_AS sub/../../escape.rs\nx\n",
            "#INCLUDE_AS {evil}\nx\n", // interpolation must not smuggle an escape
        ] {
            let err = process_err(bad, &facts);
            assert_eq!(err.line, 1, "{bad:?}");
            assert!(err.message.contains("escapes the target"), "{err}");
        }

        // A contained path with an interior `.` is fine.
        let mut path = String::from("orig.rs");
        process_file("#INCLUDE_AS ./src/a.rs\nx\n", &[], &[], &facts, &mut path)
            .expect("contained path is allowed");
        assert_eq!(path, "./src/a.rs");
    }

    #[test]
    fn is_safe_relative_path_classifies() {
        assert!(is_safe_relative_path("src/main.rs"));
        assert!(is_safe_relative_path("./a/b.rs"));
        assert!(!is_safe_relative_path("/abs"));
        assert!(!is_safe_relative_path("../x"));
        assert!(!is_safe_relative_path("a/../../b"));
        // Windows drive-relative — Path treats as Normal on Unix, but the
        // resolution semantics are unsafe on Windows.
        assert!(!is_safe_relative_path("C:foo"));
        assert!(!is_safe_relative_path("C:\\foo"));
        assert!(!is_safe_relative_path("z:"));
    }

    #[test]
    fn unbalanced_directives_are_hard_errors_not_panics() {
        // #ENDIF / #ELSE / #ELIF without a matching #IF.
        let err = process_err("body\n#ENDIF\n", &Facts::default());
        assert_eq!(err.line, 2);
        assert!(
            err.message.contains("`#ENDIF` without matching `#IF`"),
            "{err}"
        );

        let err = process_err("#ELSE\nbody\n", &Facts::default());
        assert_eq!(err.line, 1);
        assert!(
            err.message.contains("`#ELSE` without matching `#IF`"),
            "{err}"
        );

        let err = process_err("#ELIF option(\"x\")\n", &Facts::default());
        assert!(
            err.message.contains("`#ELIF` without matching `#IF`"),
            "{err}"
        );
    }

    #[test]
    fn unclosed_if_is_reported_at_its_opening_line() {
        let err = process_err("l1\n#IF option(\"x\")\nl3\n", &Facts::default());
        assert_eq!(err.line, 2, "should point at the unclosed #IF");
        assert!(err.message.contains("unclosed `#IF`"), "{err}");
    }

    #[test]
    fn bad_if_condition_is_an_error_not_a_panic() {
        let err = process_err("#IF definitely_not_a_fact\nx\n#ENDIF\n", &Facts::default());
        assert_eq!(err.line, 1);
        assert!(err.message.contains("invalid `#IF` condition"), "{err}");
        // The unknown name must be named, so the message is actionable.
        assert!(err.message.contains("definitely_not_a_fact"), "{err}");
    }

    #[test]
    fn condition_errors_are_a_single_plain_line() {
        // somni's `Debug` rendering is an ANSI-coloured multi-line caret
        // diagram whose line numbers are relative to the expression, not the
        // file — it must not end up inside our `file:line` message.
        let err = process_err("#IF definitely_not_a_fact\nx\n#ENDIF\n", &Facts::default());
        let rendered = err.to_string();
        assert!(
            !rendered.contains('\u{1b}'),
            "ANSI escape leaked: {rendered:?}"
        );
        assert!(!rendered.contains('\n'), "multi-line message: {rendered:?}");
    }
}
