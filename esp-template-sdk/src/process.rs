//! The file-directive processor — a thin shell around [somni-template].
//!
//! [`Renderer::render`] turns one template file into a string. The template
//! *language* — `if`/`else if`/`else`/`endif`, `for`, `replace`,
//! `{{ interpolation }}` and the expression grammar — belongs to
//! somni-template and is versioned by that dependency's semver. This module
//! owns the **facts** that language evaluates against, and nothing else.
//!
//! In particular it does **not** decide whether a file is emitted or what it is
//! called. `Template::render` maps a string to a string; expressing file
//! lifecycle as directives (`includefile`, `include_as`) put those decisions
//! inside the content layer, where the binary could not see them without
//! parsing every file. They are now rules in the template's `metadata.toml`,
//! and reach the SDK through [`Renderer::evaluate`] and [`Renderer::output_path`]
//! — so a manifest condition and the same condition in a template body mean
//! exactly the same thing.
//!
//! The engine's own `include` covers content composition: a file that comes in
//! two variants is one emitted file that includes one of two **partials**, and
//! the partials live under the manifest's reserved directory so nothing has to
//! mark them as "not a file".
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
//! let chip = "{{ chip.name }}";
//! ```
//!
//! The `%` is load-bearing. With a bare `//` marker the engine would try to
//! parse *every* comment as a directive (`unknown directive keyword 'an'`), so
//! the marker has to be something that cannot begin an ordinary comment.
//!
//! `//+` (`#+`, `--+`) is somni-template's `text_prefix`: the leading
//! whitespace *and* the prefix are dropped, and the rest of the line is
//! emitted as template text. That lets conditional output sit behind a comment
//! marker so the template source still compiles.
//!
//! Because the leading whitespace goes too, indentation that should survive
//! into the generated file is written *after* the marker:
//! `    //+    let p = init();` emits `    let p = init();`.
//!
//! ## The fact `Env`
//!
//! Conditions and interpolations are somni expressions evaluated against one
//! [`Env`](somni_template::Env) built from [`Facts`] plus the selected option
//! names:
//!
//! - `option(name)` — is that option selected?
//! - `group_selected(group)` — does that selection group have a pick?
//! - `has_reserved_pins` — does the selected module reserve any GPIOs?
//! - `chip` — a **struct** of everything derived from the selected chip:
//!   `chip.name`, `chip.rust_target`, `chip.dram2_uninit_size`, and one field
//!   per `esp-metadata` symbol (`chip.riscv`, `chip.soc_has_wifi`, and the
//!   string-valued ones such as `chip.bt_controller`).
//!
//! Namespacing the chip replaces a `chip_has(symbol)` predicate and the
//! purpose-picked `is_xtensa` / `is_riscv` flags. The gain is diagnostics:
//! somni reports `chip.soc_has_wfi` as an unknown field of `Chip` and points
//! at it in the source, where a predicate taking a string could only ever
//! return `false` and leave the block it guards silently disabled.
//!
//! Every [`Facts::values`] entry whose name is a somni identifier is *also*
//! registered as a value, so `#%if dram2_uninit_size > 0` style comparisons
//! work for non-chip facts too. Contract names are reserved: a template-scoped
//! value can never shadow one.
//!
//! Interpolation emits strings, so an [`Int`](FactValue::Int) fact is written
//! `{{ str(dram2_uninit_size) }}`; the bare form is a render-time type error.
//! It still compares as a number inside a condition.
//!
//! The manifest's output path is a **literal name lookup** into [`Facts::values`] (never
//! a somni expression), so substitution-only names may contain dashes — those
//! simply aren't reachable from an expression.
//!
//! [somni-template]: https://docs.rs/somni-template

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use indexmap::IndexMap;
use somni_template::{BlockStyle, Env, SomniStruct, Syntax, Template, TypedValue};

use crate::contract::is_reserved_name;

/// A substitution value. The [`Display`](std::fmt::Display) form is what
/// an output path splices in; the variant is what somni sees, so an
/// [`Int`](FactValue::Int) fact supports arithmetic and ordering in a condition
/// while a [`Str`](FactValue::Str) supports string equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactValue {
    Str(String),
    Int(u64),
    Bool(bool),
}

impl std::fmt::Display for FactValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FactValue::Str(s) => f.write_str(s),
            FactValue::Int(i) => write!(f, "{i}"),
            FactValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl From<bool> for FactValue {
    fn from(value: bool) -> Self {
        FactValue::Bool(value)
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
    /// The selected chip, exposed to templates as the `chip` struct:
    /// `chip.name`, `chip.riscv`, `chip.soc_has_wifi`, … `None` when no chip is
    /// selected, in which case any `chip.…` reference is an error naming the
    /// variable — which is the right answer for a template that needs one.
    ///
    /// The map must hold **every** field the template could reference, not just
    /// the ones true for this chip: somni reports an unknown field as an error,
    /// so a capability the chip merely lacks has to be present-and-`false`, or
    /// a cross-chip `#%if chip.soc_has_wifi` would fail instead of taking its
    /// else branch. Getting that union right is what buys the typo diagnostic —
    /// `chip.soc_has_wfi` is then the *only* unknown name.
    pub chip: Option<IndexMap<String, FactValue>>,
    /// Known-name vocabularies backing the unknown-name hard error.
    pub vocabulary: Vocabulary,
    /// Substitution values: spliced literally into an output path, and exposed to
    /// expressions as somni values when the name is an identifier.
    pub values: HashMap<String, FactValue>,
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

/// A processing failure. Malformed directives, bad conditions, and
/// unresolvable includes are hard errors rather than panics or silent no-ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessError {
    /// 1-based line in the template file, when the failure has one.
    ///
    /// `None` for failures that are not *in* a file — a manifest condition, for
    /// instance, which is authored in `metadata.toml` and has no line the SDK
    /// could name. Modelled as an option rather than a sentinel because a
    /// rendered `line 0:` is worse than no line at all.
    pub line: Option<usize>,
    pub message: String,
}

impl ProcessError {
    /// A failure at a known 1-based line.
    fn at(line: usize, message: String) -> Self {
        ProcessError {
            line: Some(line),
            message,
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
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
        syntax.text_prefix = Some(self.text_prefix());
        syntax
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
            FactValue::Bool(b) => env.value(name, *b),
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

    // The chip goes in last, so it wins over any template-scoped value that
    // slipped past `is_reserved_name`.
    if let Some(chip) = &shared.facts.chip {
        let fields = chip
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    FactValue::Str(s) => TypedValue::String(s.as_str().into()),
                    FactValue::Int(i) => TypedValue::Int(*i),
                    FactValue::Bool(b) => TypedValue::Bool(*b),
                };
                (name.as_str().into(), value)
            })
            .collect();
        env.value("chip", SomniStruct::new("Chip".into(), fields));
    }

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
/// `..` or drive-letter component. Used to reject include and output paths
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

/// A prepared context: the selections and facts one generation run shares.
///
/// Built **once** and reused for every file and every manifest condition. That
/// is the whole reason this is a type rather than a set of parameters — the
/// closures [`Env::function`] takes must be `'static`, so the context has to
/// live behind an `Rc`, and threading facts through as `&Facts` meant deep-
/// cloning the chip's symbol map once per file *and* once per condition.
pub struct Renderer {
    shared: Rc<Shared>,
}

impl Renderer {
    /// Prepare a context for one generation run.
    pub fn new(selected: &[String], selected_groups: &[String], facts: &Facts) -> Self {
        Renderer {
            shared: Rc::new(Shared {
                selected: selected.to_vec(),
                selected_groups: selected_groups.to_vec(),
                facts: facts.clone(),
                unknown: RefCell::new(None),
            }),
        }
    }

    /// Evaluate a standalone boolean condition against the same facts a
    /// template body sees.
    ///
    /// This is the manifest's entry point: the binary reads
    /// `when = 'option("wokwi")'` out of `metadata.toml` as data and asks here
    /// whether it holds. The binary decides *which files exist*; the SDK only
    /// answers what the expression means, so a condition in a manifest and the
    /// same condition in a template body can never disagree.
    ///
    /// `what` names the condition's source — a manifest key, say — and is
    /// placed in the message directly rather than patched into it afterwards.
    /// There is no line to report: the condition is not in a template file.
    pub fn evaluate(&self, condition: &str, what: &str) -> Result<bool, ProcessError> {
        // Any comment style renders the same probe; `#` is as good as another.
        let style = CommentStyle { base: "#" };
        self.eval_condition(condition, &style.syntax(), style, what, None)
    }

    /// Expand `{name}` placeholders in an output path from [`Facts::values`].
    ///
    /// For the manifest's `as` key, which names the file a template renders to.
    /// Deliberately **not** a somni expression — a path is not code, and
    /// keeping it a literal lookup lets substitution-only names contain dashes.
    /// An unknown `{name}` is left verbatim.
    pub fn output_path(&self, path: &str) -> String {
        interpolate(path, &self.shared.facts.values)
    }
}

/// Resolves an `include` path to that template file's raw contents.
///
/// Paths are **template-root-relative** — the same keys the caller stores its
/// template files under. Returning `Err` turns the include into a compile
/// error naming the path.
pub type IncludeLoader<'a> = &'a mut dyn FnMut(&str) -> Result<String, String>;

/// Render a single template file.
///
/// Whether this file is written at all, and under what name, is not decided
/// here — see the module docs. By the time `process_file` is called the caller
/// has already established the file is wanted.
///
/// Malformed directives are reported as [`ProcessError`] with the offending
/// line, never panicked.
///
/// `load` resolves `include` directives. An included file is a **partial**: its
/// body is inlined into the caller and shares the caller's facts.
///
/// A key absent from [`Facts::values`] is intentionally left unsubstituted by
/// [`interpolate_path`] (the literal placeholder survives) — that's the
/// template's "override or keep the default" idiom. Distinguishing an
/// optionally-absent
/// value from a typo needs the full declared-key universe, which only the
/// binary's `check` command has; flagging unknown keys is `check`'s job.
impl Renderer {
    /// Render one template file.
    pub fn render(&self, contents: &str, load: IncludeLoader<'_>) -> Result<String, ProcessError> {
        // The unknown-name slot is shared across every use of this context, so
        // clear it first. An earlier file that failed to *compile* never
        // reached the point where the slot is taken, and a leftover would then
        // be reported against this file.
        self.shared.unknown.borrow_mut().take();

        // A leading UTF-8 BOM is a file-level encoding marker, not content:
        // editors on Windows add it silently. `str::trim` won't remove it
        // (U+FEFF is not `char::is_whitespace`), so leaving it attached would
        // demote a first-line directive to literal text and corrupt the first
        // token of the generated file. Only the file-leading one is a BOM — a
        // U+FEFF anywhere else is ordinary content and is left alone.
        let body = contents.strip_prefix('\u{feff}').unwrap_or(contents);

        let style = CommentStyle::infer(body);
        let syntax = style.syntax();

        // The path is checked here as well as by the loader, so a
        // filesystem-backed loader can't be walked out of its root.
        let mut load_partial = |path: &str| -> Result<String, String> {
            if !is_safe_relative_path(path) {
                return Err(format!(
                    "`{path}` escapes the template root (absolute or `..` paths are not allowed)"
                ));
            }
            let raw = load(path)?;
            Ok(raw.strip_prefix('\u{feff}').unwrap_or(&raw).to_string())
        };

        let template = Template::compile_with(body, &syntax, &mut load_partial)
            .map_err(|e| template_error(body, e, "invalid template directive"))?;

        let rendered = template
            .render(build_env(&self.shared))
            .map_err(|e| template_error(body, e, "render failed"))?;

        // A vocabulary miss outranks whatever the expression evaluated to,
        // since "unknown capability" is the more actionable diagnostic.
        // Rendering may have succeeded regardless, so this is checked on the
        // success path too.
        if let Some(reason) = self.shared.unknown.borrow_mut().take() {
            return Err(ProcessError::at(1, reason));
        }

        Ok(rendered)
    }
}

/// Evaluate a standalone boolean condition by rendering it as a one-line
/// template, so conditions and template bodies always go through the same
/// engine and the same [`Env`] — there is no second expression evaluator to
/// drift out of sync.
impl Renderer {
    fn eval_condition(
        &self,
        cond: &str,
        syntax: &Syntax,
        style: CommentStyle,
        what: &str,
        line: Option<usize>,
    ) -> Result<bool, ProcessError> {
        let marker = style.marker();
        let probe = format!("{marker}if {cond}\nyes\n{marker}endif\n");

        let render =
            Template::compile(&probe, syntax).and_then(|t| t.render(build_env(&self.shared)));

        // `what` is interpolated here, at the point the message is built,
        // rather than substituted into a finished string afterwards. Patching
        // it in was fragile in both directions: it depended on this wording
        // containing the word it looked for, and it could rewrite an
        // occurrence inside the author's own condition text.
        let fail = |reason: String| ProcessError {
            line,
            message: format!("invalid {what} `{cond}`: {reason}"),
        };

        if let Some(reason) = self.shared.unknown.borrow_mut().take() {
            return Err(fail(reason));
        }

        match render {
            Ok(out) => Ok(out.trim() == "yes"),
            Err(e) => Err(fail(e.message.to_string())),
        }
    }
}

/// Map a [`somni_template::TemplateError`] back onto a source line.
fn template_error(body: &str, error: somni_template::TemplateError, what: &str) -> ProcessError {
    ProcessError::at(
        line_of(body, error.location.start),
        format!("{what}: {}", error.message),
    )
}

#[cfg(test)]
mod test {
    use super::*;

    /// A loader for the majority of tests, which use no `include` directives.
    fn no_partials() -> impl FnMut(&str) -> Result<String, String> {
        |path: &str| Err(format!("this test declares no partial `{path}`"))
    }

    /// A loader over a fixed `(path, contents)` table.
    fn partials(
        files: &'static [(&'static str, &'static str)],
    ) -> impl FnMut(&str) -> Result<String, String> {
        move |path: &str| {
            files
                .iter()
                .find_map(|(p, c)| (*p == path).then(|| c.to_string()))
                .ok_or_else(|| format!("no such partial: {path}"))
        }
    }

    /// Render with a selected-name list and no chip facts. Expects success.
    fn process(contents: &str, selected: &[&str]) -> String {
        let selected: Vec<String> = selected.iter().map(|s| s.to_string()).collect();
        Renderer::new(&selected, &[], &Facts::default())
            .render(contents, &mut no_partials())
            .expect("process_file should succeed")
    }

    /// Render with explicit option *and* group selections.
    fn process_with_groups(contents: &str, selected: &[&str], groups: &[&str]) -> String {
        let selected: Vec<String> = selected.iter().map(|s| s.to_string()).collect();
        let groups: Vec<String> = groups.iter().map(|s| s.to_string()).collect();
        Renderer::new(&selected, &groups, &Facts::default())
            .render(contents, &mut no_partials())
            .expect("process_file should succeed")
    }

    /// Render with facts and no selections.
    fn process_facts(contents: &str, facts: &Facts) -> String {
        Renderer::new(&[], &[], facts)
            .render(contents, &mut no_partials())
            .expect("process_file should succeed")
    }

    /// Returns the `ProcessError` or panics if the call unexpectedly succeeded.
    fn process_err(contents: &str, facts: &Facts) -> ProcessError {
        Renderer::new(&[], &[], facts)
            .render(contents, &mut no_partials())
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
        assert_eq!(process(NESTED, &["opt1", "opt2"]), "opt1\nopt2\n");
    }

    #[test]
    fn nested_if_else_takes_the_outer_else_branch() {
        assert_eq!(process(NESTED, &[]), "!opt1\n");
    }

    #[test]
    fn nested_if_else_takes_the_inner_else_branch() {
        assert_eq!(process(NESTED, &["opt1"]), "opt1\n!opt2\n");
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
        assert_eq!(process(src, &["a", "b"]), "a\n");
        assert_eq!(process(src, &["b", "c"]), "b\n");
        assert_eq!(process(src, &["c"]), "c\n");
        assert_eq!(process(src, &[]), "none\n");
    }

    #[test]
    fn indented_directives_are_recognized() {
        // The bundled `Cargo.toml` indents directives four spaces inside a
        // feature list, so leading whitespace before the marker must be fine.
        let src = "deps = [\n    #%if option(\"a\")\n    \"a\",\n    #%endif\n]\n";
        assert_eq!(process(src, &["a"]), "deps = [\n    \"a\",\n]\n");
        assert_eq!(process(src, &[]), "deps = [\n]\n");
    }

    #[test]
    fn ordinary_comments_are_not_directives() {
        // The whole reason the marker carries a `%`: a bare `//` prefix would
        // make the engine try to parse every comment as a directive.
        let src = "// just a comment\n//%if option(\"a\")\n//+let x = 1;\n//%endif\n";
        assert_eq!(process(src, &["a"]), "// just a comment\nlet x = 1;\n");
        assert_eq!(process(src, &[]), "// just a comment\n");
    }

    #[test]
    fn text_prefix_lines_are_emitted_uncommented() {
        // `//+` keeps the template itself compilable while emitting live code.
        assert_eq!(process("//+let x = 1;\n", &[]), "let x = 1;\n");
        assert_eq!(process("#+key = 1\n", &[]), "key = 1\n");
        assert_eq!(process("--+local x = 1\n", &[]), "local x = 1\n");
    }

    #[test]
    fn text_prefix_drops_the_indentation_before_the_marker() {
        // somni-template's `text_prefix` removes the leading whitespace along
        // with the prefix. Indentation that must survive into the generated
        // file therefore goes *after* the marker — worth pinning, because the
        // difference is invisible until you diff the output.
        assert_eq!(
            process("fn main() {\n    //+    let p = init();\n}\n", &[]),
            "fn main() {\n    let p = init();\n}\n"
        );
        assert_eq!(
            process("fn main() {\n    //+let p = init();\n}\n", &[]),
            "fn main() {\nlet p = init();\n}\n",
            "indentation before the marker is not preserved"
        );
    }

    #[test]
    fn text_prefix_is_only_stripped_at_line_start() {
        // A `//+` later in the line is ordinary content, not a marker.
        assert_eq!(
            process("let s = \"a //+ b\";\n", &[]),
            "let s = \"a //+ b\";\n"
        );
    }

    #[test]
    fn interpolation_reads_facts_values() {
        let mut facts = Facts::default();
        facts.set_value("project_name", "my-app");
        assert_eq!(
            process_facts("name = \"{{ project_name }}\"\n", &facts),
            "name = \"my-app\"\n"
        );
    }

    /// Rendering no longer decides whether or where a file lands — that is the
    /// manifest's, so a head directive is just text now.
    #[test]
    fn the_renderer_no_longer_owns_the_file_lifecycle() {
        let out = process_facts("plain content\n", &Facts::default());
        assert_eq!(out, "plain content\n");
    }

    /// The variant-pair pattern: one emitted file picks one of two partials,
    /// so the choice is structurally exclusive instead of relying on two
    /// conditions staying exact negations of each other.
    #[test]
    fn a_conditional_include_picks_one_partial() {
        const FILES: &[(&str, &str)] = &[
            // Partials carry `includefile false` so they are not emitted on
            // their own; that head directive must not leak into the caller.
            ("src/bin/main_async.rs", "async on {{ chip.name }}\n"),
            ("src/bin/main_blocking.rs", "blocking\n"),
        ];
        let src = "\
//%if option(\"embassy\")
//%include \"src/bin/main_async.rs\"
//%else
//%include \"src/bin/main_blocking.rs\"
//%endif
";
        // The partial shares the caller's facts, chip struct included.
        let facts = facts_with_chip();

        let render = |selected: &[&str]| {
            let selected: Vec<String> = selected.iter().map(|s| s.to_string()).collect();
            Renderer::new(&selected, &[], &facts)
                .render(src, &mut partials(FILES))
                .unwrap()
        };

        assert_eq!(render(&["embassy"]), "async on esp32s3\n");
        assert_eq!(render(&[]), "blocking\n");
    }

    #[test]
    fn an_include_cannot_escape_the_template_root() {
        let err = Renderer::new(&[], &[], &Facts::default())
            .render("//%include \"../../etc/passwd\"\n", &mut partials(&[]))
            .expect_err("escaping include must be rejected");
        assert!(err.message.contains("escapes the template root"), "{err}");
    }

    #[test]
    fn a_missing_partial_is_a_hard_error() {
        let err = Renderer::new(&[], &[], &Facts::default())
            .render("//%include \"nope.rs\"\n", &mut partials(&[]))
            .expect_err("a missing partial must not render as empty");
        assert!(err.message.contains("nope.rs"), "{err}");
    }

    /// A file whose *content* legitimately contains `{{ … }}` — a GitHub
    /// Actions workflow — declares its own delimiters in frontmatter. Without
    /// this the bundled `rust_ci.yml` renders `${{ secrets.GITHUB_TOKEN }}` as
    /// an interpolation and fails with "Variable secrets was not found".
    #[test]
    fn frontmatter_can_move_the_interpolation_delimiters() {
        let mut facts = facts_with_chip();
        facts.set_value("project_name", "my-app");

        let src = "\
---
expr: <% %>
---
name: <% project_name %>
run: cargo ${{ matrix.action.command }}
";
        let out = Renderer::new(&[], &[], &facts)
            .render(src, &mut no_partials())
            .unwrap();

        // Frontmatter itself is consumed, ours is substituted, GitHub's is not.
        assert_eq!(
            out,
            "name: my-app\nrun: cargo ${{ matrix.action.command }}\n"
        );
    }

    #[test]
    fn a_manifest_condition_sees_the_same_facts_as_a_template_body() {
        // The point of routing the manifest's `when` through the SDK: the
        // binary owns *which* files exist, but the meaning of the condition is
        // defined in exactly one place.
        let facts = facts_with_chip();
        let selected = vec!["wokwi".to_string()];
        let groups = vec!["chip".to_string()];

        let renderer = Renderer::new(&selected, &groups, &facts);
        let eval = |cond: &str| renderer.evaluate(cond, "`when` condition").unwrap();

        assert!(eval("option(\"wokwi\")"));
        assert!(!eval("option(\"embassy\")"));
        assert!(eval("group_selected(\"chip\")"));
        assert!(eval("chip.xtensa"));
        assert!(!eval("chip.riscv"));
        assert!(eval("option(\"wokwi\") && !chip.riscv"));
    }

    #[test]
    fn a_bad_manifest_condition_names_its_source() {
        let facts = facts_with_chip();
        let renderer = Renderer::new(&[], &[], &facts);
        let err = renderer
            .evaluate("chip.soc_has_wfi", "`emit.when` condition for `wokwi.toml`")
            .expect_err("a mistyped field must not be silently false");

        // The whole message, not a substring: the descriptor used to be spliced
        // into a finished string by rewriting the first occurrence of the word
        // "condition", which read badly and depended on wording this function
        // does not own.
        assert_eq!(
            err.message,
            "invalid `emit.when` condition for `wokwi.toml` `chip.soc_has_wfi`: \
             Struct `Chip` has no field `soc_has_wfi`"
        );
    }

    /// A manifest condition is not *in* a template file, so there is no line to
    /// report — and `line 0:` would be worse than saying nothing.
    #[test]
    fn a_manifest_condition_error_carries_no_line() {
        let facts = facts_with_chip();
        let renderer = Renderer::new(&[], &[], &facts);
        let err = renderer
            .evaluate("chip.nope", "`emit.when` condition")
            .unwrap_err();

        assert_eq!(err.line, None);
        assert!(
            !err.to_string().starts_with("line "),
            "rendered as {:?}",
            err.to_string()
        );

        // A failure inside a file still points at one.
        let in_file = process_err("#%if nope(\nx\n#%endif\n", &facts);
        assert!(in_file.line.is_some());
        assert!(in_file.to_string().starts_with("line "));
    }

    /// The context is reused across files, so the out-of-band unknown-name slot
    /// must not carry a miss from one file into the next. A file that fails to
    /// *compile* never reaches the point where the slot is taken.
    #[test]
    fn an_unknown_name_does_not_leak_between_files() {
        let facts = facts_with_vocabulary();
        let renderer = Renderer::new(&[], &[], &facts);

        // Records an unknown option, then fails to compile before taking it.
        let _ = renderer.render(
            "#%if option(\"wifii\")\nx\n#%endif\n#%if\n",
            &mut no_partials(),
        );

        // A clean file afterwards must render, not inherit the stale miss.
        let out = renderer
            .render("clean\n", &mut no_partials())
            .expect("a stale unknown name leaked into the next file");
        assert_eq!(out, "clean\n");
    }

    #[test]
    fn an_output_path_interpolates_from_the_facts() {
        let mut facts = Facts::default();
        facts.set_value("coding_agent_guidance_file", "CLAUDE.md");
        facts.set_value("chip", "esp32c6");

        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("{coding_agent_guidance_file}"),
            "CLAUDE.md"
        );
        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("src/{chip}.rs"),
            "src/esp32c6.rs"
        );
        // Unknown names survive, matching the override-or-default idiom.
        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("src/{nope}.rs"),
            "src/{nope}.rs"
        );
        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("src/{unclosed.rs"),
            "src/{unclosed.rs"
        );
    }

    #[test]
    fn an_interpolated_path_cannot_smuggle_an_escape() {
        // The manifest is template-authored, so a value that expands to `..`
        // must still be caught. `interpolate_path` substitutes; the caller
        // checks — pinned together so the pairing isn't lost.
        let mut facts = Facts::default();
        facts.set_value("evil", "../../etc/passwd");

        let path = Renderer::new(&[], &[], &facts).output_path("{evil}");
        assert_eq!(path, "../../etc/passwd");
        assert!(
            !is_safe_relative_path(&path),
            "an escaping expansion must fail the path check"
        );
    }

    #[test]
    fn path_interpolation_is_order_independent() {
        // `values` is a `HashMap`; a single left-to-right pass is what stops
        // the same manifest rendering two different paths across runs.
        for _ in 0..64 {
            let mut facts = Facts::default();
            facts.set_value("outer", "{inner}");
            facts.set_value("inner", "leaf");
            facts.set_value("chip", "esp32c6");

            // `{outer}` expands once; its `{inner}` is output, not re-expanded.
            assert_eq!(
                Renderer::new(&[], &[], &facts).output_path("src/{chip}/{outer}.rs"),
                "src/esp32c6/{inner}.rs"
            );
        }
    }

    /// A chip whose fields cover both capabilities, only one of which it has.
    fn facts_with_chip() -> Facts {
        Facts {
            chip: Some(IndexMap::from([
                ("name".to_string(), FactValue::Str("esp32s3".into())),
                ("xtensa".to_string(), FactValue::Bool(true)),
                ("riscv".to_string(), FactValue::Bool(false)),
                ("soc_has_wifi".to_string(), FactValue::Bool(true)),
                ("soc_has_bt".to_string(), FactValue::Bool(false)),
                ("bt_controller".to_string(), FactValue::Str(String::new())),
                ("dram2_uninit_size".to_string(), FactValue::Int(65536)),
            ])),
            ..Default::default()
        }
    }

    #[test]
    fn chip_fields_drive_conditions() {
        let out = process_facts(
            "\
#%if chip.soc_has_wifi
has-wifi
#%endif
#%if chip.xtensa
xtensa
#%endif
#%if chip.riscv
riscv
#%endif
#%if chip.soc_has_bt
has-bt
#%endif
",
            &facts_with_chip(),
        );
        assert_eq!(out, "has-wifi\nxtensa\n");
    }

    #[test]
    fn chip_fields_interpolate_by_type() {
        let facts = facts_with_chip();
        assert_eq!(process_facts("{{ chip.name }}\n", &facts), "esp32s3\n");
        assert_eq!(
            process_facts("{{ str(chip.dram2_uninit_size) }}\n", &facts),
            "65536\n"
        );
    }

    /// The reason the chip is a struct rather than a `chip_has(...)` predicate:
    /// a mistyped capability is an error naming the field, where a predicate
    /// taking a string could only return `false` and silently disable the block
    /// it guards. The counterpart matters just as much — a capability the chip
    /// *lacks* must stay falsy, which is why every field is present.
    #[test]
    fn a_mistyped_capability_is_an_error_but_an_absent_one_is_false() {
        let facts = facts_with_chip();

        let out = process_facts("#%if chip.soc_has_bt\nyes\n#%else\nno\n#%endif\n", &facts);
        assert_eq!(out, "no\n", "an absent capability must stay falsy");

        let err = process_err("#%if chip.soc_has_wfi\nx\n#%endif\n", &facts);
        assert!(err.message.contains("soc_has_wfi"), "{err}");
        assert!(err.message.contains("no field"), "{err}");
    }

    #[test]
    fn a_template_value_cannot_shadow_the_chip_struct() {
        let mut facts = facts_with_chip();
        facts
            .values
            .insert("chip".to_string(), FactValue::Str("nonsense".into()));
        assert_eq!(process_facts("{{ chip.name }}\n", &facts), "esp32s3\n");
    }

    #[test]
    fn without_a_chip_the_namespace_does_not_exist() {
        // Not silently false: a template that needs a chip should say so.
        let err = process_err("#%if chip.riscv\nx\n#%endif\n", &Facts::default());
        assert!(err.message.contains("chip"), "{err}");
    }

    #[test]
    fn group_selected_reads_selection_group_names() {
        let out = process_with_groups(
            "#%if group_selected(\"chip\")\nchip-picked\n#%endif\n",
            &["esp32c6"],
            &["chip"],
        );
        assert_eq!(out, "chip-picked\n");
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
        );
        assert_eq!(out, "group-hit\nclaude-hit\n");
    }

    #[test]
    fn values_are_readable_in_conditions() {
        let mut facts = facts_with_chip();
        facts.set_value("dram2_uninit_size", 1024u64);

        let out = process_facts(
            "\
#%if chip.name == \"esp32s3\"
is-s3
#%endif
#%if chip.name == \"esp32\"
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
        assert_eq!(out, "is-s3\nhas-dram2\n");
    }

    #[test]
    fn templates_cannot_shadow_reserved_facts() {
        // A `sets` key colliding with a binary predicate must not win.
        let facts = Facts {
            has_reserved_pins: false,
            values: HashMap::from([(
                "has_reserved_pins".to_string(),
                FactValue::Str("yes".to_string()),
            )]),
            ..Default::default()
        };
        let out = process_facts(
            "#%if has_reserved_pins\nshadowed\n#%else\nreserved-wins\n#%endif\n",
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

    /// Facts carrying the option/group vocabularies that back the unknown-name
    /// error for the two string-argument predicates. Chip capabilities need no
    /// vocabulary — they are struct fields, and somni reports an unknown one.
    fn facts_with_vocabulary() -> Facts {
        Facts {
            vocabulary: Vocabulary {
                options: HashSet::from(["alloc".to_string(), "wifi".to_string()]),
                groups: HashSet::from(["chip".to_string(), "flashing".to_string()]),
            },
            ..Default::default()
        }
    }

    #[test]
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
            "#%if option(\"nope\")\nx\n#%endif\n",
            "#%if option(\"alloc\")\nx\n#%else if option(\"nope\")\ny\n#%endif\n",
        ] {
            let err = process_err(template, &facts);
            assert!(
                err.message.contains("unknown option"),
                "{template:?} -> {err}"
            );
        }
    }

    #[test]
    fn an_empty_vocabulary_disables_the_check() {
        // Consumers with no vocabulary to supply keep the permissive behaviour
        // rather than having every name rejected.
        let out = process_facts(
            "#%if option(\"whatever\")\nyes\n#%else\nno\n#%endif\n",
            &Facts::default(),
        );
        assert_eq!(out, "no\n");
    }

    #[test]
    fn a_stale_unknown_name_does_not_leak_into_a_later_condition() {
        // A miss inside a *skipped* branch is never evaluated, so it must not
        // surface later and misattribute the error to an innocent line.
        let facts = facts_with_vocabulary();
        let out = Renderer::new(&["alloc".to_string()], &[], &facts).render("#%if option(\"alloc\")\nkept\n#%else\n#%if option(\"bogus\")\nx\n#%endif\n#%endif\n", &mut no_partials())
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

        // Multi-byte text directly adjacent to output-path braces.
        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("src/日本{chip}語/{emoji}.rs"),
            "src/日本esp32c6語/🦀.rs"
        );

        // And a non-ASCII string literal inside a condition.
        let out = process(
            "#%if option(\"öpt\")\nhit\n#%else\nmiss\n#%endif\n",
            &["öpt"],
        );
        assert_eq!(out, "hit\n");
    }

    #[test]
    fn leading_byte_order_mark_does_not_hide_directives() {
        // Editors on Windows happily write a UTF-8 BOM. U+FEFF is *not*
        // `char::is_whitespace`, so `trim()` leaves it attached to the first
        // directive — which would demote it to literal text and corrupt the
        // first token of the generated file.

        // A block directive on the first line.
        let out = process("\u{feff}#%if option(\"x\")\nbody\n#%endif\n", &[]);
        assert_eq!(out, "", "BOM hid the `if`");

        // A BOM ahead of ordinary content is stripped, not emitted: it would
        // otherwise corrupt the first token of a generated Rust/TOML file.
        assert_eq!(process("\u{feff}fn main() {}\n", &[]), "fn main() {}\n");

        // Only the file-leading one is a BOM; mid-file U+FEFF is content.
        assert_eq!(process("a\u{feff}b\n", &[]), "a\u{feff}b\n");
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
        assert_eq!(process(src, &["a"]), "x\n");
        assert_eq!(process(src, &[]), "");

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
        assert_eq!(err.line, Some(3));
    }

    #[test]
    fn unbalanced_directives_are_hard_errors_not_panics() {
        for bad in [
            "body\n#%endif\n",
            "#%else\nbody\n",
            "#%if option(\"x\")\nbody\n",
        ] {
            let err = process_err(bad, &Facts::default());
            assert!(err.line.is_some(), "{bad:?} -> {err}");
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

        assert_eq!(
            Renderer::new(&[], &[], &facts).output_path("{coding-agent-guidance-file}"),
            "CLAUDE.md"
        );

        // Referencing it from an expression is a hard error, not a silent false.
        let err = process_err("#%if coding-agent-guidance-file\nx\n#%endif\n", &facts);
        assert!(!err.message.is_empty(), "{err}");
    }
}
