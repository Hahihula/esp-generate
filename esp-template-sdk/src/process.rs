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

/// The set of *valid names* each string-argument predicate accepts.
#[derive(Debug, Default, Clone)]
pub struct Vocabulary {
    /// Every capability name declared by any supported chip. Backs `chip_has`.
    pub symbols: HashSet<String>,
    /// Every option name the template declares. Backs `option`.
    pub options: HashSet<String>,
    /// Every selection-group name the template declares. Backs `group_selected`.
    pub groups: HashSet<String>,
}

/// Chip-derived facts passed from the binary to the SDK — the single conduit
/// for everything the directive engine needs to know about the target that
/// isn't a user selection.
#[derive(Debug, Default, Clone)]
pub struct Facts {
    /// Metadata symbols the chip declares. Backs `chip_has(symbol)`.
    pub symbols: HashSet<String>,
    /// Known-name vocabularies backing the unknown-name hard error.
    pub vocabulary: Vocabulary,
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

/// Out-of-band slot for the first unknown name seen while evaluating one
/// condition.
///
/// somni predicates return a plain `bool` with no error channel, so a
/// vocabulary miss can't be reported from inside the closure. It is recorded
/// here instead and promoted to a [`ProcessError`] by [`eval_bool`] once
/// evaluation returns. Only the first is kept: short-circuiting means later
/// names in the same condition may not even have been reached, so reporting
/// them all would be misleading.
type UnknownName = std::cell::RefCell<Option<String>>;

/// Record an unknown-name failure, keeping the first.
fn note_unknown(slot: &UnknownName, reason: String) {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
        *slot = Some(reason);
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
    unknown: &'a UnknownName,
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
        let vocab = &facts.vocabulary.options;
        if !vocab.is_empty() && !vocab.contains(cond) {
            note_unknown(
                unknown,
                format!("unknown option `{cond}` — the template declares no such option"),
            );
            return false;
        }
        selected.iter().any(|c| c == cond)
    });

    engine.add_function("group_selected", move |group: &str| -> bool {
        let vocab = &facts.vocabulary.groups;
        if !vocab.is_empty() && !vocab.contains(group) {
            note_unknown(
                unknown,
                format!("unknown selection group `{group}` — the template declares no such group"),
            );
            return false;
        }
        selected_groups.iter().any(|g| g == group)
    });

    engine.add_function("chip_has", move |symbol: &str| -> bool {
        let vocab = &facts.vocabulary.symbols;
        if !vocab.is_empty() && !vocab.contains(symbol) {
            note_unknown(
                unknown,
                format!("unknown capability `{symbol}` — no supported chip declares it"),
            );
            return false;
        }
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

/// Expand `{name}` placeholders in `template` from `values`, in a **single
/// left-to-right pass**.
///
/// Deliberately not "for each value, global-replace its placeholder": `values`
/// is a `HashMap`, so that walks the keys in a randomized order, and a value
/// that itself contains `{...}` would then expand or not depending on which key
/// came first — the same template could render differently run to run. One pass
/// also means a substituted value is never rescanned, so expansion can't chain.
///
/// An unknown name is left verbatim, matching `#REPLACE`'s
/// "override or keep the default" idiom; an unbalanced `{` is copied through.
fn interpolate(template: &str, values: &HashMap<String, FactValue>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];

        let Some(close) = after.find('}') else {
            // No closing brace — the remainder is literal.
            out.push('{');
            out.push_str(after);
            return out;
        };

        let name = &after[..close];
        match values.get(name) {
            Some(value) => out.push_str(&value.to_string()),
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }

    out.push_str(rest);
    out
}

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
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);

    let mut res = String::new();

    let mut replace: Option<Vec<(&str, String)>> = None;
    // Each open block tracks the line it opened on, so unclosed `#IF`s can be
    // reported at their source. The base `Root` frame (line 0) is never popped.
    let mut include: Vec<(BlockKind, usize)> = vec![(BlockKind::Root, 0)];
    let mut file_directives = true;

    let unknown = UnknownName::default();
    let mut engine = build_context(selected, selected_groups, facts, &unknown);

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
                include_file = eval_bool(&mut engine, cond, line_no, "#INCLUDEFILE", &unknown)?;
                continue;
            } else if let Some(include_as) = trimmed
                .strip_prefix("//INCLUDE_AS ")
                .or_else(|| trimmed.strip_prefix("#INCLUDE_AS "))
                .or_else(|| trimmed.strip_prefix("--INCLUDE_AS "))
            {
                let include_as = interpolate(include_as.trim(), &facts.values);
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

        // `#REPLACE` binds to the **next line only**. Anything else — an
        // emitted line, a dropped one, or another directive — consumes or
        // discards the pending replacement, so it can never survive a branch
        // that wasn't taken and land on the first line after the `#ENDIF`.
        let mut is_replace_directive = false;

        // Check if we should replace the next line with the key/value of a variable
        if let Some(what) = trimmed
            .strip_prefix("#REPLACE ")
            .or_else(|| trimmed.strip_prefix("//REPLACE "))
            .or_else(|| trimmed.strip_prefix("--REPLACE "))
        {
            is_replace_directive = true;
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
                eval_bool(&mut engine, cond, line_no, "#IF", &unknown)?
            } else {
                false
            };

            include.push((BlockKind::new_if(current), line_no));
        } else if let Some(cond) = strip_directive(trimmed, "ELIF ") {
            let (last, open_line) = pop_block(&mut include, line_no, "#ELIF")?;

            // Only evaluate condition if no other branches evaluated to true
            let current = if matches!(last, BlockKind::IfElse(false, false)) {
                eval_bool(&mut engine, cond, line_no, "#ELIF", &unknown)?
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
        }

        if !is_replace_directive {
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
    unknown: &UnknownName,
) -> Result<bool, ProcessError> {
    let result = engine.evaluate::<bool>(cond);

    // A vocabulary miss outranks whatever the expression evaluated to — and
    // any somni error, since "unknown capability" is the more actionable
    // diagnostic. Taking it also clears the slot for the next condition.
    if let Some(reason) = unknown.borrow_mut().take() {
        return Err(ProcessError {
            line,
            message: format!("invalid `{directive}` condition `{cond}`: {reason}"),
        });
    }

    result.map_err(|e| ProcessError {
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
    fn interpolate_handles_edge_cases() {
        let values = HashMap::from([
            ("a".to_string(), FactValue::Str("A".to_string())),
            ("b".to_string(), FactValue::Int(7)),
        ]);
        let go = |s: &str| interpolate(s, &values);

        assert_eq!(go(""), "");
        assert_eq!(go("no placeholders"), "no placeholders");
        assert_eq!(go("{a}"), "A");
        assert_eq!(go("{a}{b}"), "A7"); // adjacent
        assert_eq!(go("x/{a}/y/{b}.rs"), "x/A/y/7.rs");
        assert_eq!(go("{unknown}"), "{unknown}");
        assert_eq!(go("{}"), "{}"); // empty name is just unknown
        assert_eq!(go("trailing {"), "trailing {");
        assert_eq!(go("{a"), "{a");
        assert_eq!(go("}{a}"), "}A"); // stray close brace
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
    fn replace_applies_only_to_the_next_line() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");

        // A `#REPLACE` inside a branch that is NOT taken must not survive the
        // `#ENDIF` and hit the first line after it.
        let out = process_file(
            "#IF option(\"never\")\n#REPLACE PLACEHOLDER chip\nPLACEHOLDER\n#ENDIF\nPLACEHOLDER\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "PLACEHOLDER", "leaked out of a skipped branch");

        // Same for a branch that IS taken but whose next line is the `#ENDIF`:
        // the directive consumes the pending replacement either way.
        let out = process_file(
            "#IF option(\"yes\")\n#REPLACE PLACEHOLDER chip\n#ENDIF\nPLACEHOLDER\n",
            &["yes".to_string()],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "PLACEHOLDER", "leaked past an `#ENDIF`");

        // The ordinary case still works.
        let out = process_file(
            "#REPLACE PLACEHOLDER chip\nPLACEHOLDER\nPLACEHOLDER\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out.trim(),
            "esp32c6\nPLACEHOLDER",
            "should apply to exactly one line"
        );
    }

    #[test]
    fn include_as_interpolation_is_order_independent() {
        for _ in 0..64 {
            let mut facts = Facts::default();
            facts.set_value("outer", "{inner}");
            facts.set_value("inner", "leaf");
            facts.set_value("chip", "esp32c6");

            let mut path = String::from("orig.rs");
            process_file(
                "#INCLUDE_AS src/{chip}/{outer}.rs\nx\n",
                &[],
                &[],
                &facts,
                &mut path,
            )
            .unwrap();
            // `{outer}` expands once; its `{inner}` is output, not re-expanded.
            assert_eq!(path, "src/esp32c6/{inner}.rs");
        }
    }

    #[test]
    fn include_as_keeps_unknown_placeholders_verbatim() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");

        let mut path = String::from("orig.rs");
        process_file(
            "#INCLUDE_AS src/{chip}/{nope}/{unclosed.rs\nx\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap();
        assert_eq!(path, "src/esp32c6/{nope}/{unclosed.rs");
    }

    /// Facts carrying a full vocabulary, plus one chip that has only
    /// `soc_has_wifi` out of the two capabilities that exist.
    fn facts_with_vocabulary() -> Facts {
        Facts {
            symbols: HashSet::from(["soc_has_wifi".to_string()]),
            vocabulary: Vocabulary {
                symbols: HashSet::from(["soc_has_wifi".to_string(), "soc_has_pcnt".to_string()]),
                options: HashSet::from(["alloc".to_string(), "wifi".to_string()]),
                groups: HashSet::from(["chip".to_string(), "flashing".to_string()]),
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_typo_is_distinguishable_from_an_absent_capability() {
        let facts = facts_with_vocabulary();

        // In the vocabulary but not this chip's set → plain `false`, which is
        // the whole point of `chip_has`.
        let out = process_file(
            "#IF chip_has(\"soc_has_pcnt\")\nyes\n#ELSE\nno\n#ENDIF\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "no", "an absent capability must stay falsy");

        // Outside the vocabulary → author error, not a silent `false`.
        let err = process_err("#IF chip_has(\"soc_has_wfi\")\nx\n#ENDIF\n", &facts);
        assert_eq!(err.line, 1);
        assert!(err.message.contains("unknown capability"), "{err}");
        assert!(err.message.contains("soc_has_wfi"), "{err}");
    }

    #[test]
    fn unknown_option_and_group_names_are_hard_errors() {
        let facts = facts_with_vocabulary();

        // A declared-but-unselected option is falsy...
        let out = process_file(
            "#IF option(\"wifi\")\nyes\n#ELSE\nno\n#ENDIF\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "no");

        // ...but a misspelled one is an error. `wifii` would otherwise just
        // silently disable the block it guards.
        let err = process_err("#IF option(\"wifii\")\nx\n#ENDIF\n", &facts);
        assert!(err.message.contains("unknown option"), "{err}");
        assert!(err.message.contains("wifii"), "{err}");

        let err = process_err("#IF group_selected(\"flashng\")\nx\n#ENDIF\n", &facts);
        assert!(err.message.contains("unknown selection group"), "{err}");

        // The namespaces stay disjoint: a real group name is still not a real
        // option name, and now says so instead of quietly returning false.
        let err = process_err("#IF option(\"flashing\")\nx\n#ENDIF\n", &facts);
        assert!(err.message.contains("unknown option"), "{err}");
    }

    #[test]
    fn unknown_names_are_caught_in_every_condition_directive() {
        let facts = facts_with_vocabulary();

        for template in [
            "#INCLUDEFILE chip_has(\"nope\")\nx\n",
            "#IF chip_has(\"nope\")\nx\n#ENDIF\n",
            "#IF option(\"alloc\")\nx\n#ELIF chip_has(\"nope\")\ny\n#ENDIF\n",
        ] {
            let err = process_err(template, &facts);
            assert!(
                err.message.contains("unknown capability"),
                "{template:?} -> {err}"
            );
        }
    }

    #[test]
    fn an_empty_vocabulary_disables_the_check() {
        // Consumers that have no vocabulary to supply keep the permissive
        // behaviour rather than having every name rejected.
        let facts = Facts::default();
        let out = process_file(
            "#IF chip_has(\"anything\") || option(\"whatever\")\nyes\n#ELSE\nno\n#ENDIF\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "no");
    }

    #[test]
    fn a_stale_unknown_name_does_not_leak_into_a_later_condition() {
        // The slot is per-evaluation. A miss inside a *skipped* branch is
        // never evaluated, so it must not surface later and misattribute the
        // error to an innocent line.
        let facts = facts_with_vocabulary();
        let out = process_file(
            "#IF option(\"alloc\")\nkept\n#ELSE\n#IF chip_has(\"bogus\")\nx\n#ENDIF\n#ENDIF\n",
            &["alloc".to_string()],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "kept");
    }

    #[test]
    fn non_ascii_text_survives_every_stage() {
        // Directive parsing is byte-oriented in places (`find('{')`,
        // `as_bytes()`), so multi-byte content must not be corrupted or panic a
        // slice on a non-char boundary.
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        facts.set_value("emoji", "🦀");
        // A non-ASCII *name* isn't a somni identifier, so it stays
        // `#REPLACE`-only — but it must still work there.
        facts.set_value("café", "naïve");

        // Body text is copied through byte-for-byte.
        let out = process_file(
            "let s = \"héllo → wörld 日本語 🦀\";\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out, "let s = \"héllo → wörld 日本語 🦀\";\n");

        // Multi-byte `#REPLACE` pattern and value.
        let out = process_file(
            "#REPLACE ☃ café\nlet x = \"☃\";\n",
            &[],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "let x = \"naïve\";");

        // Multi-byte text directly adjacent to `#INCLUDE_AS` braces.
        let mut path = String::from("orig.rs");
        process_file(
            "#INCLUDE_AS src/日本{chip}語/{emoji}.rs\nx\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap();
        assert_eq!(path, "src/日本esp32c6語/🦀.rs");

        // And a non-ASCII string literal inside a condition.
        let out = process_file(
            "#IF option(\"öpt\")\nhit\n#ELSE\nmiss\n#ENDIF\n",
            &["öpt".to_string()],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "hit");
    }

    #[test]
    fn leading_byte_order_mark_does_not_hide_directives() {
        // Editors on Windows happily write a UTF-8 BOM. U+FEFF is *not*
        // `char::is_whitespace`, so `trim()` leaves it attached to the first
        // directive — which would silently demote `#INCLUDEFILE` to literal
        // text, emitting a file that should have been skipped (with the
        // directive line still in it).
        let out = process_file(
            "\u{feff}#INCLUDEFILE false\nbody\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap();
        assert_eq!(out, None, "BOM hid the `#INCLUDEFILE`");

        // Same for a block directive on the first line.
        let out = process_file(
            "\u{feff}#IF option(\"x\")\nbody\n#ENDIF\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.trim(), "", "BOM hid the `#IF`");

        // A BOM ahead of ordinary content is stripped, not emitted: it would
        // otherwise corrupt the first token of a generated Rust/TOML file.
        let out = process_file(
            "\u{feff}fn main() {}\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out, "fn main() {}\n");

        // Only the file-leading one is a BOM; mid-file U+FEFF is content.
        let out = process_file(
            "a\u{feff}b\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out, "a\u{feff}b\n");
    }

    #[test]
    fn crlf_templates_are_recognized_and_normalized() {
        // `str::lines()` splits off the `\r`, so directives are matched, but
        // the emitted file is always LF — worth pinning so a future switch to
        // manual splitting doesn't silently start leaking `\r` into output.
        let out = process_file(
            "a\r\n#IF option(\"x\")\r\nb\r\n#ENDIF\r\nc\r\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out, "a\nc\n");
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
