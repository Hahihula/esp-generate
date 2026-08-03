//! The file-directive processor — a thin shell around [somni-template].
//!
//! `process_file` renders a single template file. The template *language* —
//! `if`/`else if`/`else`/`endif`, `for`, `replace`, `{{ interpolation }}` and
//! the expression grammar — belongs to somni-template and is versioned by that
//! dependency's semver. This module owns only the two things the engine has no
//! concept of, plus the facts:
//!
//! - **`includefile <cond>`** — whether the file is emitted at all;
//! - **`include_as <path>`** — what the output file is called.
//!
//! Both are file-*lifecycle* directives: `Template::render` maps a string to a
//! string and has no notion of a file that should not exist, or of an output
//! path. They are stripped from the head of the source before compilation.
//!
//! ## Syntax
//!
//! Directives are comment-shaped so a template file stays valid, lintable
//! source in its own language. The marker is the file's comment prefix plus
//! `%`; a bare comment is left completely alone:
//!
//! ```text
//! // an ordinary comment, emitted verbatim
//! //%if option("wifi")
//! //+let wifi = true;              // `//+` is stripped: emits `let wifi = true;`
//! //%endif
//! let chip = "{{ chip }}";
//! ```
//!
//! The `%` is load-bearing. With a bare `//` marker the engine would try to
//! parse *every* comment as a directive (`unknown directive keyword 'an'`), so
//! the marker has to be something that cannot begin an ordinary comment.
//!
//! `//+` (`#+`, `--+`) marks a text line: the prefix is stripped and the rest
//! is emitted, so conditional output can sit behind a comment marker and the
//! template source still compiles. somni-template has a `text_prefix` for
//! this, but it drops the line's leading whitespace too — fine for markers at
//! column 0, wrong for source code — so the SDK strips the prefix itself and
//! keeps the indentation (see `CommentStyle::strip_text_prefixes`).
//!
//! ## The fact `Env`
//!
//! Conditions and interpolations are somni expressions evaluated against one
//! [`Env`](somni_template::Env) built from [`Facts`] plus the selected option
//! names:
//!
//! - `option(name)` — is that option selected?
//! - `group_selected(group)` — does that selection group have a pick?
//! - `chip_has(symbol)` — does the selected chip declare that metadata symbol?
//! - `is_xtensa` / `is_riscv` — the selected chip's ISA.
//! - `has_reserved_pins` — does the selected module reserve any GPIOs?
//!
//! Every [`Facts::values`] entry whose name is a somni identifier is *also*
//! registered as a value, so `{{ chip }}`, `#%if chip == "esp32c6"` and
//! `#%if dram2_uninit_size > 0` all work. The predicates above are reserved: a
//! template-scoped value can never shadow one.
//!
//! Interpolation emits strings, so an [`Int`](FactValue::Int) fact is written
//! `{{ str(dram2_uninit_size) }}`; the bare form is a render-time type error.
//! It still compares as a number inside a condition.
//!
//! `include_as` remains a **literal name lookup** into [`Facts::values`] (never
//! a somni expression), so substitution-only names may contain dashes — those
//! simply aren't reachable from an expression.
//!
//! [somni-template]: https://docs.rs/somni-template

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use somni_template::{BlockStyle, Env, Syntax, Template};

use crate::contract::is_reserved_name;

/// A substitution value. The [`Display`](std::fmt::Display) form is what
/// `include_as` splices into a path; the variant is what somni sees, so an
/// [`Int`](FactValue::Int) fact supports arithmetic and ordering in a condition
/// while a [`Str`](FactValue::Str) supports string equality.
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
///
/// A name outside its vocabulary is a hard [`ProcessError`], not a silent
/// `false`: with only the selected chip's symbols to go on, a typo and a
/// capability this chip merely lacks are indistinguishable, and both would
/// quietly disable the block they guard.
///
/// An **empty** set means "not supplied" and disables the check for that
/// predicate, which is what the SDK's own tests rely on. Only *evaluated*
/// conditions are covered; a typo in a branch that is never reached is
/// `check`'s job to find statically.
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
    /// Substitution values: spliced literally by `include_as`, and exposed to
    /// expressions as somni values when the name is an identifier.
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
/// registering it as a value makes it referenceable from an expression. Dashed
/// substitution-only names (`coding-agent-guidance-file`) are not.
fn is_somni_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A directive-processing failure carrying the 1-based source line, so the
/// binary can surface `file:line: message`. Malformed directives, bad
/// conditions, and target-escaping `include_as` paths are all hard errors
/// rather than panics or silent no-ops.
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

/// The comment convention a template file is written in. Fixes the directive
/// marker (`<base>%`) and the text-line prefix (`<base>+`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommentStyle {
    base: &'static str,
}

impl CommentStyle {
    /// Every convention the SDK understands, longest base first so that `--`
    /// is never mistaken for a prefix of something else.
    const ALL: &'static [CommentStyle] = &[
        CommentStyle { base: "//" },
        CommentStyle { base: "--" },
        CommentStyle { base: "#" },
    ];

    /// Infer the convention from the first marker in `source` — either a
    /// directive (`//%`) or a text-prefix line (`//+`).
    ///
    /// Both count: a file may carry `//+` lines without any directive at all
    /// (an unconditional line the template wants to keep commented in its own
    /// source), and inferring `#` for it would leave the prefix unstripped.
    ///
    /// A file with neither renders identically under any convention — it is
    /// all literal text — so the fallback is arbitrary, but must still be *a*
    /// valid syntax.
    fn infer(source: &str) -> CommentStyle {
        source
            .lines()
            .find_map(|line| {
                let trimmed = line.trim_start();
                CommentStyle::ALL.iter().copied().find(|style| {
                    trimmed.starts_with(&style.marker())
                        || trimmed.starts_with(&style.text_prefix())
                })
            })
            .unwrap_or(CommentStyle { base: "#" })
    }

    /// The directive marker, e.g. `//%`.
    fn marker(&self) -> String {
        format!("{}%", self.base)
    }

    /// The text-line prefix, e.g. `//+`.
    fn text_prefix(&self) -> String {
        format!("{}+", self.base)
    }

    fn syntax(&self) -> Syntax {
        let mut syntax = Syntax::lines();
        syntax.block = BlockStyle::Line {
            prefix: self.marker(),
        };
        // Deliberately NOT `syntax.text_prefix`: see `strip_text_prefixes`.
        syntax
    }

    /// Uncomment `//+` lines, keeping their indentation.
    ///
    /// somni-template has a `text_prefix` feature for exactly this, but it
    /// drops the leading whitespace along with the prefix — right for
    /// templates whose markers sit at column 0, wrong for source code, where
    /// `    //+let p = init();` has to emit `    let p = init();` and not
    /// shift to column 0.
    ///
    /// Only the prefix itself is removed, and only at the start of a line; a
    /// `//+` occurring later in a line is ordinary content.
    fn strip_text_prefixes(&self, body: &str) -> String {
        let prefix = self.text_prefix();
        let mut out = String::with_capacity(body.len());

        for line in body.split_inclusive('\n') {
            let indent_len = line.len() - line.trim_start().len();
            let (indent, rest) = line.split_at(indent_len);
            match rest.strip_prefix(&prefix) {
                Some(text) => {
                    out.push_str(indent);
                    out.push_str(text);
                }
                None => out.push_str(line),
            }
        }

        out
    }
}

/// Everything the registered predicates need, in one refcounted bundle.
///
/// [`Env::function`] requires `'static` closures (unlike `somni_expr::Context`,
/// which borrows), so the facts cannot simply be captured by reference. One
/// `Rc` per `process_file` call is shared by every closure and by both of the
/// `Env`s a file needs.
struct Shared {
    selected: Vec<String>,
    selected_groups: Vec<String>,
    facts: Facts,
    /// Out-of-band slot for the first unknown name seen while evaluating.
    ///
    /// somni predicates return a plain `bool` with no error channel, so a
    /// vocabulary miss can't be reported from inside the closure. It is
    /// recorded here and promoted to a [`ProcessError`] once rendering
    /// returns. Only the first is kept: short-circuiting means later names may
    /// never have been reached, so reporting them all would mislead.
    unknown: RefCell<Option<String>>,
}

impl Shared {
    fn note_unknown(&self, reason: String) {
        let mut slot = self.unknown.borrow_mut();
        if slot.is_none() {
            *slot = Some(reason);
        }
    }
}

/// Build the render environment exposing the fact predicates and values.
///
/// The option and selection-group namespaces are **disjoint**: `option(name)`
/// only ever sees option names and `group_selected(group)` only ever sees group
/// names, so an option and a group sharing a name (the bundled template's
/// `coding-agent-guidance` is both a category and a group) can't be confused
/// for one another.
fn build_env(shared: &Rc<Shared>) -> Env {
    let mut env = Env::new();

    // Template-scoped values go in first so the reserved registrations below
    // always win; names that aren't somni identifiers stay lookup-only.
    for (name, value) in &shared.facts.values {
        if !is_somni_identifier(name) || is_reserved_name(name) {
            continue;
        }
        match value {
            FactValue::Str(s) => env.value(name, s.as_str()),
            FactValue::Int(i) => env.value(name, *i),
        };
    }

    let ctx = shared.clone();
    env.function("option", move |name: &str| -> bool {
        let vocab = &ctx.facts.vocabulary.options;
        if !vocab.is_empty() && !vocab.contains(name) {
            ctx.note_unknown(format!(
                "unknown option `{name}` — the template declares no such option"
            ));
            return false;
        }
        ctx.selected.iter().any(|c| c == name)
    });

    let ctx = shared.clone();
    env.function("group_selected", move |group: &str| -> bool {
        let vocab = &ctx.facts.vocabulary.groups;
        if !vocab.is_empty() && !vocab.contains(group) {
            ctx.note_unknown(format!(
                "unknown selection group `{group}` — the template declares no such group"
            ));
            return false;
        }
        ctx.selected_groups.iter().any(|g| g == group)
    });

    let ctx = shared.clone();
    env.function("chip_has", move |symbol: &str| -> bool {
        let vocab = &ctx.facts.vocabulary.symbols;
        if !vocab.is_empty() && !vocab.contains(symbol) {
            ctx.note_unknown(format!(
                "unknown capability `{symbol}` — no supported chip declares it"
            ));
            return false;
        }
        ctx.facts.symbols.contains(symbol)
    });

    env.value("is_xtensa", shared.facts.is_xtensa);
    env.value("is_riscv", shared.facts.is_riscv);
    env.value("has_reserved_pins", shared.facts.has_reserved_pins);

    env
}

/// Expand `{name}` placeholders in `template` from `values`, in a **single
/// left-to-right pass**.
///
/// Deliberately not "for each value, global-replace its placeholder": `values`
/// is a `HashMap`, so that walks the keys in a randomized order, and a value
/// that itself contains `{...}` would then expand or not depending on which key
/// came first — the same template could render differently run to run. One pass
/// also means a substituted value is never rescanned, so expansion can't chain.
///
/// An unknown name is left verbatim, matching the "override or keep the
/// default" idiom; an unbalanced `{` is copied through.
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
/// `..` or drive-letter component. Used to reject `include_as`/output paths
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

/// The 1-based line a byte offset falls on.
fn line_of(source: &str, offset: usize) -> usize {
    let end = offset.min(source.len());
    source
        .get(..end)
        .map_or(1, |head| head.matches('\n').count() + 1)
}

/// The file-lifecycle directives lifted off the head of a template.
struct FileDirectives<'a> {
    /// The `includefile` condition, if the file declares one.
    condition: Option<&'a str>,
    /// The raw `include_as` path, if the file declares one.
    rename: Option<(&'a str, usize)>,
    /// The source with those lines removed.
    body: String,
    /// How many lines were removed, so error lines can be shifted back.
    consumed: usize,
}

/// Lift `includefile` / `include_as` off the head of the source.
///
/// They are only recognised before the first line of real content, which is
/// what makes removing them safe: every remaining line keeps its relative
/// order, and reported line numbers just need `consumed` added back.
fn take_file_directives<'a>(source: &'a str, style: CommentStyle) -> FileDirectives<'a> {
    let marker = style.marker();
    let mut condition = None;
    let mut rename = None;
    let mut consumed = 0;

    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(&marker) else {
            break;
        };
        let rest = rest.trim_start();

        if let Some(cond) = strip_keyword(rest, "includefile") {
            condition = Some(cond);
        } else if let Some(path) = strip_keyword(rest, "include_as") {
            rename = Some((path, idx + 1));
        } else {
            break;
        }
        consumed += 1;
    }

    // `lines()` drops the trailing newline, so rebuild from the raw source to
    // avoid changing whether the body ends with one.
    let body = if consumed == 0 {
        source.to_string()
    } else {
        let mut remaining = source;
        for _ in 0..consumed {
            remaining = remaining.split_once('\n').map_or("", |(_, rest)| rest);
        }
        remaining.to_string()
    };

    FileDirectives {
        condition,
        rename,
        body,
        consumed,
    }
}

/// Strip a case-insensitive directive keyword and the whitespace after it.
fn strip_keyword<'a>(rest: &'a str, keyword: &str) -> Option<&'a str> {
    let head = rest.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let arg = &rest[keyword.len()..];
    if !arg.starts_with(char::is_whitespace) {
        return None;
    }
    Some(arg.trim())
}

/// Process a single template file, returning the rendered contents, or `None`
/// if an `includefile` directive excluded the file entirely.
///
/// `file_path` is updated in place if the file carries an `include_as`
/// directive; the rewritten path is validated to stay inside the target
/// directory. Malformed directives are reported as [`ProcessError`] with the
/// offending line, never panicked.
///
/// A key absent from [`Facts::values`] is intentionally left unsubstituted by
/// `include_as` (the literal placeholder survives) — that's the template's
/// "override or keep the default" idiom. Distinguishing an optionally-absent
/// value from a typo needs the full declared-key universe, which only the
/// binary's `check` command has; flagging unknown keys is `check`'s job.
pub fn process_file(
    contents: &str,             // Raw content of the file
    selected: &[String],        // Selected option names
    selected_groups: &[String], // Selection groups that have a pick
    facts: &Facts,              // Chip-derived facts + substitution values
    file_path: &mut String,     // File path to be modified
) -> Result<Option<String>, ProcessError> {
    // A leading UTF-8 BOM is a file-level encoding marker, not content: editors
    // on Windows add it silently. `str::trim` won't remove it (U+FEFF is not
    // `char::is_whitespace`), so leaving it attached would demote a first-line
    // directive to literal text and corrupt the first token of the generated
    // file. Only the file-leading one is a BOM — a U+FEFF anywhere else is
    // ordinary content and is left alone.
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);

    let style = CommentStyle::infer(contents);
    let syntax = style.syntax();
    let directives = take_file_directives(contents, style);

    let shared = Rc::new(Shared {
        selected: selected.to_vec(),
        selected_groups: selected_groups.to_vec(),
        facts: facts.clone(),
        unknown: RefCell::new(None),
    });

    // `includefile` first: if the file is excluded there is no point compiling
    // the body, and a template may legitimately contain directives that only
    // make sense once the file is included at all.
    if let Some(cond) = directives.condition
        && !eval_condition(cond, &syntax, style, &shared, 1)?
    {
        return Ok(None);
    }

    if let Some((path, line)) = directives.rename {
        let renamed = interpolate(path, &facts.values);
        if !is_safe_relative_path(&renamed) {
            return Err(ProcessError {
                line,
                message: format!(
                    "`include_as` path `{renamed}` escapes the target directory \
                     (absolute or `..` paths are not allowed)"
                ),
            });
        }
        *file_path = renamed;
    }

    // Line-for-line, so reported line numbers are unaffected.
    let body = style.strip_text_prefixes(&directives.body);

    let template = Template::compile(&body, &syntax)
        .map_err(|e| template_error(&body, e, directives.consumed, "invalid template directive"))?;

    let rendered = template
        .render(build_env(&shared))
        .map_err(|e| template_error(&body, e, directives.consumed, "render failed"))?;

    // A vocabulary miss outranks whatever the expression evaluated to, since
    // "unknown capability" is the more actionable diagnostic. Rendering may
    // have succeeded regardless, so this is checked on the success path too.
    if let Some(reason) = shared.unknown.borrow_mut().take() {
        return Err(ProcessError {
            line: 1,
            message: reason,
        });
    }

    Ok(Some(rendered))
}

/// Evaluate a standalone boolean condition by rendering it as a one-line
/// template, so conditions and template bodies always go through the same
/// engine and the same [`Env`] — there is no second expression evaluator to
/// drift out of sync.
fn eval_condition(
    cond: &str,
    syntax: &Syntax,
    style: CommentStyle,
    shared: &Rc<Shared>,
    line: usize,
) -> Result<bool, ProcessError> {
    let marker = style.marker();
    let probe = format!("{marker}if {cond}\nyes\n{marker}endif\n");

    let render = Template::compile(&probe, syntax).and_then(|t| t.render(build_env(shared)));

    if let Some(reason) = shared.unknown.borrow_mut().take() {
        return Err(ProcessError {
            line,
            message: format!("invalid `includefile` condition `{cond}`: {reason}"),
        });
    }

    match render {
        Ok(out) => Ok(out.trim() == "yes"),
        Err(e) => Err(ProcessError {
            line,
            message: format!("invalid `includefile` condition `{cond}`: {}", e.message),
        }),
    }
}

/// Map a [`somni_template::TemplateError`] back onto a source line, adding back
/// the head lines that were stripped before compilation.
fn template_error(
    body: &str,
    error: somni_template::TemplateError,
    consumed: usize,
    what: &str,
) -> ProcessError {
    ProcessError {
        line: line_of(body, error.location.start) + consumed,
        message: format!("{what}: {}", error.message),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Render with a selected-name list and no chip facts. Expects success.
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

    /// Render with explicit option *and* group selections.
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

    /// Render with facts and no selections.
    fn process_facts(contents: &str, facts: &Facts) -> String {
        process_file(contents, &[], &[], facts, &mut String::from("m.rs"))
            .expect("process_file should succeed")
            .expect("file should be included")
    }

    /// Returns the `ProcessError` or panics if the call unexpectedly succeeded.
    fn process_err(contents: &str, facts: &Facts) -> ProcessError {
        process_file(contents, &[], &[], facts, &mut String::from("f.rs"))
            .expect_err("expected a ProcessError")
    }

    const NESTED: &str = "\
#%if option(\"opt1\")
opt1
#%if option(\"opt2\")
opt2
#%else
!opt2
#%endif
#%else
!opt1
#%endif
";

    #[test]
    fn nested_if_else_takes_the_inner_then_branch() {
        assert_eq!(process(NESTED, &["opt1", "opt2"]).unwrap(), "opt1\nopt2\n");
    }

    #[test]
    fn nested_if_else_takes_the_outer_else_branch() {
        assert_eq!(process(NESTED, &[]).unwrap(), "!opt1\n");
    }

    #[test]
    fn nested_if_else_takes_the_inner_else_branch() {
        assert_eq!(process(NESTED, &["opt1"]).unwrap(), "opt1\n!opt2\n");
    }

    #[test]
    fn else_if_chains_pick_the_first_true_arm() {
        let src = "\
#%if option(\"a\")
a
#%else if option(\"b\")
b
#%else if option(\"c\")
c
#%else
none
#%endif
";
        assert_eq!(process(src, &["a", "b"]).unwrap(), "a\n");
        assert_eq!(process(src, &["b", "c"]).unwrap(), "b\n");
        assert_eq!(process(src, &["c"]).unwrap(), "c\n");
        assert_eq!(process(src, &[]).unwrap(), "none\n");
    }

    #[test]
    fn indented_directives_are_recognized() {
        // The bundled `Cargo.toml` indents directives four spaces inside a
        // feature list, so leading whitespace before the marker must be fine.
        let src = "deps = [\n    #%if option(\"a\")\n    \"a\",\n    #%endif\n]\n";
        assert_eq!(
            process(src, &["a"]),
            Some("deps = [\n    \"a\",\n]\n".into())
        );
        assert_eq!(process(src, &[]), Some("deps = [\n]\n".into()));
    }

    #[test]
    fn ordinary_comments_are_not_directives() {
        // The whole reason the marker carries a `%`: a bare `//` prefix would
        // make the engine try to parse every comment as a directive.
        let src = "// just a comment\n//%if option(\"a\")\n//+let x = 1;\n//%endif\n";
        assert_eq!(
            process(src, &["a"]),
            Some("// just a comment\nlet x = 1;\n".into())
        );
        assert_eq!(process(src, &[]), Some("// just a comment\n".into()));
    }

    #[test]
    fn text_prefix_lines_are_emitted_uncommented() {
        // `//+` keeps the template itself compilable while emitting live code.
        assert_eq!(process("//+let x = 1;\n", &[]), Some("let x = 1;\n".into()));
        assert_eq!(process("#+key = 1\n", &[]), Some("key = 1\n".into()));
        assert_eq!(
            process("--+local x = 1\n", &[]),
            Some("local x = 1\n".into())
        );
    }

    #[test]
    fn text_prefix_keeps_the_line_indentation() {
        // The engine's own `text_prefix` drops leading whitespace with the
        // prefix, which would left-align generated code. Indentation is
        // semantically meaningful in the files this template emits.
        assert_eq!(
            process("fn main() {\n    //+let p = init();\n}\n", &[]),
            Some("fn main() {\n    let p = init();\n}\n".into())
        );
    }

    #[test]
    fn text_prefix_is_only_stripped_at_line_start() {
        // A `//+` later in the line is ordinary content, not a marker.
        assert_eq!(
            process("let s = \"a //+ b\";\n", &[]),
            Some("let s = \"a //+ b\";\n".into())
        );
    }

    #[test]
    fn interpolation_reads_facts_values() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        assert_eq!(
            process_facts("features = [\"{{ chip }}\"]\n", &facts),
            "features = [\"esp32c6\"]\n"
        );
    }

    #[test]
    fn include_as_interpolates_values_and_rewrites_path() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        let mut path = String::from("src/chip.rs");
        let res = process_file(
            "#%include_as src/{chip}.rs\nfn main() {}\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap()
        .unwrap();
        assert_eq!(path, "src/esp32c6.rs");
        assert_eq!(res, "fn main() {}\n");
    }

    #[test]
    fn includefile_excludes_the_whole_file() {
        assert_eq!(process("#%includefile option(\"a\")\nbody\n", &[]), None);
        assert_eq!(
            process("#%includefile option(\"a\")\nbody\n", &["a"]),
            Some("body\n".into())
        );
    }

    #[test]
    fn chip_has_and_isa_predicates_drive_conditions() {
        let mut facts = Facts::default();
        facts.symbols.insert("soc_has_wifi".to_string());
        facts.is_xtensa = true;
        facts.is_riscv = false;

        let out = process_facts(
            "\
#%if chip_has(\"soc_has_wifi\")
has-wifi
#%endif
#%if is_xtensa
xtensa
#%endif
#%if is_riscv
riscv
#%endif
#%if chip_has(\"soc_has_bt\")
has-bt
#%endif
",
            &facts,
        );
        assert_eq!(out, "has-wifi\nxtensa\n");
    }

    #[test]
    fn group_selected_reads_selection_group_names() {
        let out = process_with_groups(
            "#%if group_selected(\"chip\")\nchip-picked\n#%endif\n",
            &["esp32c6"],
            &["chip"],
        );
        assert_eq!(out.unwrap(), "chip-picked\n");
    }

    #[test]
    fn option_and_group_namespaces_are_disjoint() {
        // The bundled template has a name that is BOTH a category and a
        // selection group (`coding-agent-guidance`); the two predicates must
        // not answer for each other.
        let out = process_with_groups(
            "\
#%if option(\"coding-agent-guidance\")
option-hit
#%endif
#%if group_selected(\"coding-agent-guidance\")
group-hit
#%endif
#%if option(\"claude\")
claude-hit
#%endif
#%if group_selected(\"claude\")
claude-group-hit
#%endif
",
            &["claude"],
            &["coding-agent-guidance"],
        )
        .unwrap();
        assert_eq!(out, "group-hit\nclaude-hit\n");
    }

    #[test]
    fn values_are_readable_in_conditions() {
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        facts.set_value("dram2_uninit_size", 1024u64);

        let out = process_facts(
            "\
#%if chip == \"esp32c6\"
is-c6
#%endif
#%if chip == \"esp32\"
is-esp32
#%endif
#%if dram2_uninit_size > 0
has-dram2
#%endif
#%if dram2_uninit_size > 4096
big-dram2
#%endif
",
            &facts,
        );
        assert_eq!(out, "is-c6\nhas-dram2\n");
    }

    #[test]
    fn templates_cannot_shadow_reserved_facts() {
        // A `sets` key colliding with a binary predicate must not win.
        let facts = Facts {
            is_xtensa: false,
            values: HashMap::from([("is_xtensa".to_string(), FactValue::Str("yes".to_string()))]),
            ..Default::default()
        };
        let out = process_facts(
            "#%if is_xtensa\nshadowed\n#%else\nreserved-wins\n#%endif\n",
            &facts,
        );
        assert_eq!(out, "reserved-wins\n");
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

    #[test]
    fn include_as_rejects_escaping_paths() {
        let mut facts = Facts::default();
        facts.set_value("evil", "../../etc/passwd");

        for bad in [
            "#%include_as /etc/passwd\nx\n",
            "#%include_as ../outside.rs\nx\n",
            "#%include_as ../../etc/passwd\nx\n",
            "#%include_as sub/../../escape.rs\nx\n",
            "#%include_as {evil}\nx\n", // interpolation must not smuggle an escape
        ] {
            let err = process_err(bad, &facts);
            assert_eq!(err.line, 1, "{bad:?}");
            assert!(err.message.contains("escapes the target"), "{err}");
        }

        // A contained path with an interior `.` is fine.
        let mut path = String::from("orig.rs");
        process_file("#%include_as ./src/a.rs\nx\n", &[], &[], &facts, &mut path)
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
    fn include_as_interpolation_is_order_independent() {
        for _ in 0..64 {
            let mut facts = Facts::default();
            facts.set_value("outer", "{inner}");
            facts.set_value("inner", "leaf");
            facts.set_value("chip", "esp32c6");

            let mut path = String::from("orig.rs");
            process_file(
                "#%include_as src/{chip}/{outer}.rs\nx\n",
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
            "#%include_as src/{chip}/{nope}/{unclosed.rs\nx\n",
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
        let out = process_facts(
            "#%if chip_has(\"soc_has_pcnt\")\nyes\n#%else\nno\n#%endif\n",
            &facts,
        );
        assert_eq!(out, "no\n", "an absent capability must stay falsy");

        // Outside the vocabulary → author error, not a silent `false`.
        let err = process_err("#%if chip_has(\"soc_has_wfi\")\nx\n#%endif\n", &facts);
        assert!(err.message.contains("unknown capability"), "{err}");
        assert!(err.message.contains("soc_has_wfi"), "{err}");
    }

    #[test]
    fn unknown_option_and_group_names_are_hard_errors() {
        let facts = facts_with_vocabulary();

        // A declared-but-unselected option is falsy...
        let out = process_facts("#%if option(\"wifi\")\nyes\n#%else\nno\n#%endif\n", &facts);
        assert_eq!(out, "no\n");

        // ...but a misspelled one is an error. `wifii` would otherwise just
        // silently disable the block it guards.
        let err = process_err("#%if option(\"wifii\")\nx\n#%endif\n", &facts);
        assert!(err.message.contains("unknown option"), "{err}");
        assert!(err.message.contains("wifii"), "{err}");

        let err = process_err("#%if group_selected(\"flashng\")\nx\n#%endif\n", &facts);
        assert!(err.message.contains("unknown selection group"), "{err}");

        // The namespaces stay disjoint: a real group name is still not a real
        // option name, and says so instead of quietly returning false.
        let err = process_err("#%if option(\"flashing\")\nx\n#%endif\n", &facts);
        assert!(err.message.contains("unknown option"), "{err}");
    }

    #[test]
    fn unknown_names_are_caught_in_every_condition_directive() {
        let facts = facts_with_vocabulary();

        for template in [
            "#%includefile chip_has(\"nope\")\nx\n",
            "#%if chip_has(\"nope\")\nx\n#%endif\n",
            "#%if option(\"alloc\")\nx\n#%else if chip_has(\"nope\")\ny\n#%endif\n",
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
        // Consumers with no vocabulary to supply keep the permissive behaviour
        // rather than having every name rejected.
        let out = process_facts(
            "#%if chip_has(\"anything\") || option(\"whatever\")\nyes\n#%else\nno\n#%endif\n",
            &Facts::default(),
        );
        assert_eq!(out, "no\n");
    }

    #[test]
    fn a_stale_unknown_name_does_not_leak_into_a_later_condition() {
        // A miss inside a *skipped* branch is never evaluated, so it must not
        // surface later and misattribute the error to an innocent line.
        let facts = facts_with_vocabulary();
        let out = process_file(
            "#%if option(\"alloc\")\nkept\n#%else\n#%if chip_has(\"bogus\")\nx\n#%endif\n#%endif\n",
            &["alloc".to_string()],
            &[],
            &facts,
            &mut String::from("m.rs"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out, "kept\n");
    }

    #[test]
    fn non_ascii_text_survives_every_stage() {
        // Path interpolation is byte-oriented in places (`find('{')`,
        // `as_bytes()`), so multi-byte content must not be corrupted or panic a
        // slice on a non-char boundary.
        let mut facts = Facts::default();
        facts.set_value("chip", "esp32c6");
        facts.set_value("emoji", "🦀");

        // Body text is copied through byte-for-byte.
        let out = process_facts("let s = \"héllo → wörld 日本語 🦀\";\n", &facts);
        assert_eq!(out, "let s = \"héllo → wörld 日本語 🦀\";\n");

        // Multi-byte text directly adjacent to `include_as` braces.
        let mut path = String::from("orig.rs");
        process_file(
            "#%include_as src/日本{chip}語/{emoji}.rs\nx\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap();
        assert_eq!(path, "src/日本esp32c6語/🦀.rs");

        // And a non-ASCII string literal inside a condition.
        let out = process(
            "#%if option(\"öpt\")\nhit\n#%else\nmiss\n#%endif\n",
            &["öpt"],
        )
        .unwrap();
        assert_eq!(out, "hit\n");
    }

    #[test]
    fn leading_byte_order_mark_does_not_hide_directives() {
        // Editors on Windows happily write a UTF-8 BOM. U+FEFF is *not*
        // `char::is_whitespace`, so `trim()` leaves it attached to the first
        // directive — which would silently demote `includefile` to literal
        // text, emitting a file that should have been skipped.
        let out = process_file(
            "\u{feff}#%includefile false\nbody\n",
            &[],
            &[],
            &Facts::default(),
            &mut String::from("m.rs"),
        )
        .unwrap();
        assert_eq!(out, None, "BOM hid the `includefile`");

        // Same for a block directive on the first line.
        let out = process("\u{feff}#%if option(\"x\")\nbody\n#%endif\n", &[]).unwrap();
        assert_eq!(out, "", "BOM hid the `if`");

        // A BOM ahead of ordinary content is stripped, not emitted: it would
        // otherwise corrupt the first token of a generated Rust/TOML file.
        assert_eq!(
            process("\u{feff}fn main() {}\n", &[]).unwrap(),
            "fn main() {}\n"
        );

        // Only the file-leading one is a BOM; mid-file U+FEFF is content.
        assert_eq!(process("a\u{feff}b\n", &[]).unwrap(), "a\u{feff}b\n");
    }

    #[test]
    fn bad_condition_is_an_error_not_a_panic() {
        let err = process_err(
            "#%if definitely_not_a_fact\nx\n#%endif\n",
            &Facts::default(),
        );
        // The unknown name must be named, so the message is actionable.
        assert!(err.message.contains("definitely_not_a_fact"), "{err}");
    }

    #[test]
    fn condition_errors_are_a_single_plain_line() {
        // somni's `Debug` rendering is an ANSI-coloured multi-line caret
        // diagram whose line numbers are relative to the expression, not the
        // file — it must not end up inside our `file:line` message.
        let err = process_err(
            "#%if definitely_not_a_fact\nx\n#%endif\n",
            &Facts::default(),
        );
        let rendered = err.to_string();
        assert!(
            !rendered.contains('\u{1b}'),
            "ANSI escape leaked: {rendered:?}"
        );
        assert!(!rendered.contains('\n'), "multi-line message: {rendered:?}");
    }

    #[test]
    fn endif_and_else_tolerate_a_trailing_label() {
        // The bundled `Cargo.toml` annotates which block is closing
        // (`#%endif wifi || ble-trouble`) — genuinely useful in a 240-line
        // file with deep nesting. The label is ignored, not emitted, and not
        // an error. Pinned because losing it would silently degrade every
        // large template into unreadable directive soup.
        let src = "#%if option(\"a\")\nx\n#%endif a || b\n";
        assert_eq!(process(src, &["a"]).unwrap(), "x\n");
        assert_eq!(process(src, &[]).unwrap(), "");

        // `else` is the exception: it has a meaningful continuation (`else
        // if`), so it parses strictly and a label is a hard error rather than
        // being read as a malformed `else if`.
        let err = process_err(
            "#%if option(\"a\")\nx\n#%else not-a\ny\n#%endif\n",
            &Facts::default(),
        );
        assert!(
            err.message.contains("after `else`"),
            "expected a clear diagnostic, got: {err}"
        );
        assert_eq!(err.line, 3);
    }

    #[test]
    fn unbalanced_directives_are_hard_errors_not_panics() {
        for bad in [
            "body\n#%endif\n",
            "#%else\nbody\n",
            "#%if option(\"x\")\nbody\n",
        ] {
            let err = process_err(bad, &Facts::default());
            assert!(err.line >= 1, "{bad:?} -> {err}");
        }
    }

    #[test]
    fn int_values_interpolate_as_decimal_and_compare_as_numbers() {
        // The same fact reads as a number in a condition and splices as its
        // decimal form in an interpolation — but interpolation emits strings
        // only, so an int fact has to go through `str()`. Pinned because the
        // bare form fails at *render* time with a type error, which would
        // otherwise be a confusing thing to hit while authoring a template.
        let mut facts = Facts::default();
        facts.set_value("dram2_uninit_size", 32768u64);

        assert_eq!(
            process_facts("size: {{ str(dram2_uninit_size) }});\n", &facts),
            "size: 32768);\n"
        );
        assert_eq!(
            process_facts("#%if dram2_uninit_size > 4096\nbig\n#%endif\n", &facts),
            "big\n"
        );

        let err = process_err("size: {{ dram2_uninit_size }};\n", &facts);
        assert!(err.message.contains("to be &str"), "{err}");
    }

    #[test]
    fn dashed_value_names_are_path_only() {
        // Not a somni identifier, so it is reachable from `include_as` (a
        // literal `{name}` lookup) but not from an expression. Any value a
        // template needs to interpolate must therefore be snake_case.
        let mut facts = Facts::default();
        facts.set_value("coding-agent-guidance-file", "CLAUDE.md");

        let mut path = String::from("x.md");
        process_file(
            "#%include_as {coding-agent-guidance-file}\nx\n",
            &[],
            &[],
            &facts,
            &mut path,
        )
        .unwrap();
        assert_eq!(path, "CLAUDE.md");

        // Referencing it from an expression is a hard error, not a silent false.
        let err = process_err("#%if coding-agent-guidance-file\nx\n#%endif\n", &facts);
        assert!(!err.message.is_empty(), "{err}");
    }
}
