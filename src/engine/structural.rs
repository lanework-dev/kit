//! `search_structural`: code matching an ast-grep pattern, one entry per
//! file, with `ast-grep-core` and `ast-grep-language` (the grammars of
//! `STRUCTURAL_LANGS` only). `electron/kit/structural.ts` in Rust.
//!
//! - `target` is real and already in scope; `pattern` at most `QUERY_MAX`.
//! - The files are what ripgrep would search (`.gitignore` respected, hidden
//!   files skipped), narrowed by the globs as `rg --glob` narrows them
//!   (search_text's code), kept to the languages' extensions. A symlink is
//!   never read: the folder's scope ends at it.
//! - A file as the target: that file alone, under the name it was given,
//!   in `lang` or else its extension's language; the globs do not apply.
//! - `lang` None: each file in its own language, from its extension. With a
//!   `lang`, only that language's extensions are read.
//! - A pattern that does not parse in a language is tried once more with
//!   the body a declaration pattern needs ([`fixed_pattern`]: `def f()` in
//!   Python, `fn f()` and `impl X` in Rust, `function f()` in TS and JS,
//!   `func f()` in Go); the answer's note says when the fixed one ran. In
//!   TS and TSX a bodiless `function f()` parses, as an overload signature
//!   that matches no function with a body, so the fix is tried first.
//! - A language the pattern does not parse in is left out; when that is all
//!   of them, the search is `invalid`.
//! - Stops at `max_files` files or `max_lines` lines, or at the timeout,
//!   with the last file `truncated`. A timeout before any match is `timeout`.
//! - Every offset in an answer is UTF-16, as the contract says; ast-grep's
//!   are bytes.

use super::files::Cancel;
use super::scope::Target;
use super::types::{
    FileMatch, KitError, MatchLine, Range, StructuralAnswer, StructuralLang, StructuralOptions,
    MAX_LINES,
};
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{AstGrep, Language, Node, Pattern};
use ast_grep_language::SupportLang;
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The whole search, listing included.
const TIMEOUT: Duration = Duration::from_millis(10_000);
/// A larger file is skipped: generated or minified, and slow to parse.
const FILE_BYTES_MAX: u64 = 1024 * 1024;
/// Files read and parsed at once.
const BATCH: usize = 64;
/// The longest line kept, in UTF-16 units; a longer one is cut to a window
/// around its first match.
const LINE_MAX: usize = 400;
/// How much of a cut line is kept before its first match.
const LINE_LEAD: usize = 80;
/// Worker threads parse deep trees; give them room.
const STACK_BYTES: usize = 16 * 1024 * 1024;

/// What the engine needs to know about one `STRUCTURAL_LANGS` language.
struct LangSpec {
    /// ast-grep's language.
    support: SupportLang,
    /// What it reads in, lower case, without the dot.
    extensions: &'static [&'static str],
    /// Name for an error message.
    label: &'static str,
    /// HTML takes a pattern with a missing end tag (`<div>`) as usable.
    html: bool,
}

fn spec_of(lang: StructuralLang) -> LangSpec {
    let (support, extensions, label, html): (SupportLang, &[&str], &str, bool) = match lang {
        StructuralLang::Ts => (
            SupportLang::TypeScript,
            &["ts", "mts", "cts"],
            "TypeScript",
            false,
        ),
        StructuralLang::Tsx => (SupportLang::Tsx, &["tsx"], "TSX", false),
        StructuralLang::Js => (
            SupportLang::JavaScript,
            &["js", "mjs", "cjs"],
            "JavaScript",
            false,
        ),
        // NOTE: ast-grep's JavaScript grammar parses JSX.
        StructuralLang::Jsx => (SupportLang::JavaScript, &["jsx"], "JSX", false),
        StructuralLang::Json => (SupportLang::Json, &["json"], "JSON", false),
        StructuralLang::Css => (SupportLang::Css, &["css"], "CSS", false),
        StructuralLang::Html => (SupportLang::Html, &["html", "htm"], "HTML", true),
        StructuralLang::Python => (SupportLang::Python, &["py", "pyi"], "Python", false),
        StructuralLang::Rust => (SupportLang::Rust, &["rs"], "Rust", false),
        StructuralLang::Go => (SupportLang::Go, &["go"], "Go", false),
    };
    LangSpec {
        support,
        extensions,
        label,
        html,
    }
}

/// Code in `target` (every file under a folder, or the one file) matching
/// `options.pattern`, one entry per file, with the note when the pattern
/// was fixed.
pub fn search_structural(
    target: &Target,
    options: &StructuralOptions,
) -> Result<StructuralAnswer, KitError> {
    let pattern = options.pattern.as_str();
    if pattern.trim().is_empty() {
        return Ok(StructuralAnswer::default());
    }
    let started = Instant::now();
    // Set when `lwk serve`'s caller cancelled the search.
    let cancel = Cancel::current();
    let max_files = options.max_files.min(super::types::SEARCH_STRUCTURAL_LIMIT.max);
    let max_lines = options.max_lines.clamp(1, MAX_LINES.max);

    let langs: Vec<StructuralLang> = match options.lang {
        Some(lang) => vec![lang],
        None => StructuralLang::ALL.to_vec(),
    };
    let specs: Vec<LangSpec> = langs.iter().map(|lang| spec_of(*lang)).collect();

    // The folder read from, the files in it, and the name a file is
    // answered under when one file was named.
    let (root, mut files, named) = match target {
        Target::Folder(root) => {
            let files = list_files(root, &specs, &options.globs, started, &cancel)?;
            (root.as_path(), files, None)
        }
        Target::File { file, name } => {
            let one = one_file(file, &specs, options.lang.is_some())?;
            let root = file.parent().unwrap_or(file);
            (root, one, Some(name.as_str()))
        }
    };
    files.sort_by(|a, b| a.path.cmp(&b.path));

    // Only the languages there are files for are checked, and kept when the
    // pattern (or its fix) parses in them.
    let mut patterns: Vec<Option<Pattern>> = Vec::with_capacity(specs.len());
    let mut errors: Vec<String> = Vec::new();
    let mut hints: Vec<&str> = Vec::new();
    // Each fixed pattern searched with, and its note.
    let mut fixes: Vec<(String, String)> = Vec::new();
    for (at, spec) in specs.iter().enumerate() {
        if !files.iter().any(|file| file.spec == at) {
            patterns.push(None);
            continue;
        }
        // NOTE: TypeScript reads `function f()` as an overload signature,
        // which matches no function with a body: the fix is tried first.
        let fixed = fixed_pattern(langs[at], pattern);
        if matches!(langs[at], StructuralLang::Ts | StructuralLang::Tsx) {
            if let Some(Ok(compiled)) = fixed.as_deref().map(|fixed| check_fixed(spec, fixed)) {
                let fixed = fixed.unwrap_or_default();
                if !fixes.iter().any(|(known, _)| *known == fixed) {
                    let note = fix_note(langs[at], pattern, &fixed);
                    fixes.push((fixed, note));
                }
                patterns.push(Some(compiled));
                continue;
            }
        }
        let error = match check_pattern(spec, pattern) {
            Ok(compiled) => {
                patterns.push(Some(compiled));
                continue;
            }
            Err(error) => error.lines().next().unwrap_or(&error).to_owned(),
        };
        let Some(fixed) = fixed else {
            errors.push(error);
            patterns.push(None);
            continue;
        };
        match check_fixed(spec, &fixed) {
            Ok(compiled) => {
                if !fixes.iter().any(|(known, _)| *known == fixed) {
                    let note = fix_note(langs[at], pattern, &fixed);
                    fixes.push((fixed, note));
                }
                patterns.push(Some(compiled));
            }
            Err(_) => {
                let hint = fix_hint(langs[at]);
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
                errors.push(format!("{error} {hint}"));
                patterns.push(None);
            }
        }
    }
    if !files.is_empty() && patterns.iter().all(Option::is_none) {
        return Err(KitError::invalid(if errors.len() == 1 {
            errors.remove(0)
        } else {
            let mut message = "That pattern does not parse as code in any of this folder's languages. Check its brackets and quotes.".to_owned();
            for hint in hints {
                message.push(' ');
                message.push_str(hint);
            }
            message
        }));
    }
    files.retain(|file| patterns[file.spec].is_some());
    // Said only when a fixed pattern was searched with.
    let note = (!fixes.is_empty() && !files.is_empty()).then(|| {
        let notes: Vec<String> = fixes.into_iter().map(|(_, note)| note).collect();
        notes.join("; ")
    });

    let word = required_word(pattern);
    let mut found: Vec<FileMatch> = Vec::new();
    let mut matches = 0usize;
    let mut timed_out = false;
    'outer: for batch in files.chunks(BATCH) {
        if cancel.is_set() {
            return Err(Cancel::error());
        }
        if started.elapsed() >= TIMEOUT {
            timed_out = true;
            break;
        }
        let results = match_batch(root, batch, &specs, &patterns, word.as_deref(), &cancel);
        for mut result in results.into_iter().flatten() {
            if found.len() >= max_files {
                if let Some(last) = found.last_mut() {
                    last.truncated = true;
                }
                break 'outer;
            }
            let room = max_lines - matches;
            if result.lines.len() > room {
                result.lines.truncate(room);
                result.truncated = true;
            }
            matches += result.lines.len();
            let full = matches >= max_lines;
            if full {
                result.truncated = true;
            }
            found.push(result);
            if full {
                break 'outer;
            }
        }
    }
    if timed_out || started.elapsed() >= TIMEOUT {
        match found.last_mut() {
            Some(last) => last.truncated = true,
            None => return Err(KitError::timeout()),
        }
    }
    if let Some(name) = named {
        for file in &mut found {
            file.path = name.to_owned();
        }
    }
    Ok(StructuralAnswer { files: found, note })
}

/// `pattern` with the body its declaration needs to parse in `lang`, or
/// None when it is not a bodiless declaration this fixes:
///
/// - Python: a `def`, `async def` or `class` header gets `: $$$BODY`.
/// - Rust: `fn ...`, `impl X` and `impl X for Y` get ` { $$$BODY }`.
/// - TS, TSX, JS and JSX: `function f(...)` gets ` { $$$BODY }`.
/// - Go: `func ...` gets ` { $$$BODY }`.
pub(crate) fn fixed_pattern(lang: StructuralLang, pattern: &str) -> Option<String> {
    let text = pattern.trim();
    // `word` starts `text`, followed by a space or one of `then`.
    fn starts(text: &str, word: &str, then: &[char]) -> bool {
        text.strip_prefix(word)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|next| next.is_whitespace() || then.contains(&next))
    }
    // `text` without leading words from `skip` (and `pub(...)` in Rust).
    fn strip<'t>(mut text: &'t str, skip: &[&str]) -> &'t str {
        loop {
            let before = text;
            for word in skip {
                if starts(text, word, &[]) {
                    text = text[word.len()..].trim_start();
                }
            }
            if text.starts_with("pub(") {
                if let Some(close) = text.find(')') {
                    text = text[close + 1..].trim_start();
                }
            }
            if text == before {
                return text;
            }
        }
    }
    let braced = |text: &str| format!("{} {{ $$$BODY }}", text.trim_end_matches(';').trim_end());
    match lang {
        StructuralLang::Python => {
            let head = strip(text, &["async"]);
            if !(starts(head, "def", &[]) || starts(head, "class", &[])) {
                return None;
            }
            Some(if text.ends_with(':') {
                format!("{text} $$$BODY")
            } else {
                format!("{text}: $$$BODY")
            })
        }
        StructuralLang::Rust => {
            let head = strip(text, &["pub", "async", "unsafe", "const", "default"]);
            let wanted = starts(head, "fn", &[]) || starts(head, "impl", &['<']);
            (wanted && !text.ends_with('}')).then(|| braced(text))
        }
        StructuralLang::Ts | StructuralLang::Tsx | StructuralLang::Js | StructuralLang::Jsx => {
            let head = strip(text, &["export", "default", "async", "declare"]);
            let wanted = starts(head, "function", &['*', '(', '$']);
            (wanted && !text.contains('{')).then(|| braced(text))
        }
        StructuralLang::Go => {
            let wanted = starts(text, "func", &['(']);
            (wanted && !text.ends_with('}')).then(|| braced(text))
        }
        StructuralLang::Json | StructuralLang::Css | StructuralLang::Html => None,
    }
}

/// The note for a search run with `fixed` in place of `pattern`: which
/// pattern ran, and what it does not match ([`fix_caveat`]).
pub(crate) fn fix_note(lang: StructuralLang, pattern: &str, fixed: &str) -> String {
    match fix_caveat(lang, pattern) {
        Some(caveat) => format!("searched `{fixed}` (added the body the pattern needs; {caveat})"),
        None => format!("searched `{fixed}` (added the body the pattern needs)"),
    }
}

/// What the fixed form of `pattern` skips, and what to add to it to match
/// those; None when it skips nothing worth saying.
///
/// NOTE: ast-grep skips no named node of the code that the pattern lacks,
/// so `fn $N($$$A) { $$$BODY }` misses `pub fn` and `fn f() -> i32`. An
/// `export` (TS) or a decorator (Python) wraps the declaration, which
/// still matches; those need no word here.
fn fix_caveat(lang: StructuralLang, pattern: &str) -> Option<String> {
    let text = pattern.trim();
    // (what is skipped, what to add), each kept when the pattern lacks it.
    let mut items: Vec<(&str, &str)> = Vec::new();
    let mut add = |missing: bool, what, how| {
        if missing {
            items.push((what, how));
        }
    };
    let (before, after) = match lang {
        StructuralLang::Rust if text.contains("impl") && !text.contains("fn ") => {
            add(!text.contains(" for "), "trait", "` for $U` after the type");
            add(!text.contains('<'), "generic", "`<$G>` after `impl`");
            ("", " impls")
        }
        StructuralLang::Rust => {
            add(!text.contains("pub"), "`pub`", "`pub`");
            add(!text.contains("->"), "a return type", "`-> $R`");
            add(!text.contains('<'), "generics", "`<$G>` after the name");
            ("fns with ", "")
        }
        StructuralLang::Ts | StructuralLang::Tsx => {
            add(!text.contains("):"), "a return type", "`: $R` after the parameters");
            add(!text.contains('<'), "type parameters", "`<$G>` after the name");
            ("functions with ", "")
        }
        StructuralLang::Python if text.contains("class") => {
            add(!text.contains('('), "bases", "`($$$B)` after the name");
            ("classes with ", "")
        }
        StructuralLang::Python => {
            add(!text.contains("->"), "a return annotation", "`-> $R` before the colon");
            ("defs with ", "")
        }
        StructuralLang::Go => {
            add(text.ends_with(')'), "a result type", "`$R` after the parameters");
            ("funcs with ", "")
        }
        _ => return None,
    };
    if items.is_empty() {
        return None;
    }
    let whats: Vec<&str> = items.iter().map(|(what, _)| *what).collect();
    let hows: Vec<&str> = items.iter().map(|(_, how)| *how).collect();
    Some(format!(
        "it skips {before}{}{after}; to match those, add {}",
        or_list(&whats),
        or_list(&hows)
    ))
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// The language a file is read in, from its extension; None for any other.
pub(crate) fn lang_of_extension(extension: &str) -> Option<StructuralLang> {
    let extension = extension.to_ascii_lowercase();
    StructuralLang::ALL
        .into_iter()
        .find(|lang| spec_of(*lang).extensions.contains(&extension.as_str()))
}

/// The fix [`search_structural`] would make, made before the search: for
/// a caller that hands the pattern to an app that does not fix it (`lwk`
/// and `lwk mcp` before the kit socket). The fixed pattern and its note,
/// or None when there is nothing to fix or no language to fix it for.
///
/// The languages: `lang`; else the file's (by `extension`); else every
/// language whose fix fits the pattern's shape (`def` is Python, `fn` and
/// `impl` Rust, `function` TS and JS, `func` Go). In each the pattern must
/// not parse (or, in TS, be a bodiless `function`) and its fix must; and
/// all must fix it alike. Otherwise None: the search goes as it is.
pub fn fix_ahead(
    pattern: &str,
    lang: Option<StructuralLang>,
    extension: Option<&str>,
) -> Option<(String, String)> {
    let langs: Vec<StructuralLang> = match (lang, extension.and_then(lang_of_extension)) {
        (Some(lang), _) | (None, Some(lang)) => vec![lang],
        (None, None) => StructuralLang::ALL
            .into_iter()
            .filter(|lang| fixed_pattern(*lang, pattern).is_some())
            .collect(),
    };
    let mut found: Option<(String, String)> = None;
    for lang in langs {
        let spec = spec_of(lang);
        let fixed = fixed_pattern(lang, pattern)?;
        let prefer_fix = matches!(lang, StructuralLang::Ts | StructuralLang::Tsx);
        if !prefer_fix && check_pattern(&spec, pattern).is_ok() {
            return None;
        }
        check_fixed(&spec, &fixed).ok()?;
        match &found {
            Some((known, _)) if *known != fixed => return None,
            Some(_) => {}
            None => found = Some((fixed.clone(), fix_note(lang, pattern, &fixed))),
        }
    }
    found
}

/// What a declaration pattern in `lang` needs, with an example, for when
/// neither it nor its fix parses.
fn fix_hint(lang: StructuralLang) -> &'static str {
    match lang {
        StructuralLang::Python => {
            "A def or class pattern needs a body, e.g. `def $F($$$ARGS): $$$BODY`."
        }
        StructuralLang::Rust => {
            "A fn or impl pattern needs a block, e.g. `fn $N($$$ARGS) { $$$BODY }` or `impl $T { $$$BODY }`."
        }
        StructuralLang::Go => "A func pattern needs a body, e.g. `func $F($$$ARGS) { $$$BODY }`.",
        _ => "A function pattern needs braces, e.g. `function $F($$$ARGS) { $$$BODY }`.",
    }
}

/// The one file `file` as a candidate: its real name, in the first spec
/// (`lang` given) or its extension's.
fn one_file(file: &Path, specs: &[LangSpec], lang_given: bool) -> Result<Vec<Candidate>, KitError> {
    let path = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let by_extension = specs
        .iter()
        .position(|spec| spec.extensions.contains(&extension.as_str()));
    let spec = match (by_extension, lang_given) {
        (Some(spec), _) => spec,
        (None, true) => 0,
        (None, false) => {
            return Err(KitError::invalid(format!(
                "Cannot tell the language of {path} from its name; pass lang."
            )))
        }
    };
    Ok(vec![Candidate { path, spec }])
}

/// A file to search: its path relative to the folder, and its language's
/// index in the specs.
struct Candidate {
    path: String,
    spec: usize,
}

/// The files under `root` that ripgrep would search, kept to the specs'
/// extensions; symlinks and files over 1 MB left out.
fn list_files(
    root: &Path,
    specs: &[LangSpec],
    globs: &[String],
    started: Instant,
    cancel: &Cancel,
) -> Result<Vec<Candidate>, KitError> {
    let mut builder = WalkBuilder::new(root);
    builder.follow_links(false);
    if let Some(overrides) = super::text::overrides(root, globs)? {
        builder.overrides(overrides);
    }
    let walk = builder.build();
    let mut files = Vec::new();
    for entry in walk {
        if cancel.is_set() {
            return Err(Cancel::error());
        }
        if started.elapsed() >= TIMEOUT {
            return Err(KitError::timeout());
        }
        let Ok(entry) = entry else { continue };
        // A symlink is not a file here: `file_type` is the link's own.
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(extension) = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
        else {
            continue;
        };
        let extension = extension.to_ascii_lowercase();
        let Some(spec) = specs
            .iter()
            .position(|spec| spec.extensions.contains(&extension.as_str()))
        else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let path = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push(Candidate { path, spec });
    }
    Ok(files)
}

/// `pattern` compiled for `spec`, or ast-grep's complaint if it does not
/// parse as that language's code.
fn check_pattern(spec: &LangSpec, pattern: &str) -> Result<Pattern, String> {
    let lang = spec.support;
    // ast-grep refuses some patterns outright (no node, several nodes).
    let compiled = Pattern::try_new(pattern, lang).map_err(|error| error.to_string())?;
    // A pattern with a syntax error matches nothing, silently.
    let source = lang.pre_process_pattern(pattern);
    let doc = StrDoc::try_new(&source, lang).map_err(|error| error.to_string())?;
    let tree = AstGrep::doc(doc);
    let top = tree.root();
    // NOTE: HTML leaves a missing end tag out of a `<div>` pattern, which is
    // still a usable one; only its ERROR nodes count.
    if has_syntax_error(&top, !spec.html) {
        return Err(format!("That pattern is not valid {}.", spec.label));
    }
    // HTML takes any text as a text node; a pattern there starts with a tag.
    if spec.html && top.children().all(|child| child.kind() == "text") {
        return Err("An HTML pattern starts with a tag.".to_owned());
    }
    Ok(compiled)
}

/// A fixed pattern ([`fixed_pattern`]) compiled for `spec`: checked as
/// [`check_pattern`] checks one, or with its `$$$BODY` left out.
///
/// NOTE: tree-sitter-rust reads `$$$BODY` in an impl's body as an item it
/// cannot place (an ERROR node), yet ast-grep matches `impl $T { $$$BODY }`
/// as meant: what the body holds is the metavariable's to match.
fn check_fixed(spec: &LangSpec, fixed: &str) -> Result<Pattern, String> {
    check_pattern(spec, fixed).or_else(|error| {
        check_pattern(spec, &fixed.replace("$$$BODY", ""))
            .map_err(|_| error)
            .and_then(|_| Pattern::try_new(fixed, spec.support).map_err(|error| error.to_string()))
    })
}

/// An ERROR node, or (with `missing`) a node the parser made up.
fn has_syntax_error<D: ast_grep_core::Doc>(node: &Node<'_, D>, missing: bool) -> bool {
    if node.is_error() || node.kind() == "ERROR" {
        return true;
    }
    if missing && node.is_leaf() {
        let range = node.range();
        if range.start == range.end && node.text().is_empty() {
            return true;
        }
    }
    node.children().any(|child| has_syntax_error(&child, missing))
}

/// The longest plain word of `pattern` (not a metavariable): a file without
/// it cannot match, so it is never parsed. None when there is none.
fn required_word(pattern: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let bytes = pattern.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        if !(bytes[at].is_ascii_alphabetic() || bytes[at] == b'_') {
            at += 1;
            continue;
        }
        while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
            at += 1;
        }
        // Not a word when a `$` or word character comes right before it, as
        // in `$A` or `$$$ARGS`, or when a non-ASCII letter touches it.
        let before = pattern[..start].chars().next_back();
        let after = pattern[at..].chars().next();
        let touches = |c: Option<char>| c.is_some_and(|c| c == '$' || c.is_alphanumeric() || c == '_');
        if touches(before) || after.is_some_and(|c| !c.is_ascii() && c.is_alphanumeric()) {
            continue;
        }
        let word = &pattern[start..at];
        if word.len() >= 2 && word.len() > best.map_or(0, str::len) {
            best = Some(word);
        }
    }
    best.map(str::to_owned)
}

/// Each file's matches, in the batch's order, found on several threads.
fn match_batch(
    root: &Path,
    batch: &[Candidate],
    specs: &[LangSpec],
    patterns: &[Option<Pattern>],
    word: Option<&str>,
    cancel: &Cancel,
) -> Vec<Option<FileMatch>> {
    let slots: Vec<Mutex<Option<FileMatch>>> = batch.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(batch.len());
    let work = || loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(file) = batch.get(at) else { break };
        // Cancelled: the rest of the batch is left unread, its answer unsent.
        if cancel.is_set() {
            break;
        }
        let Some(pattern) = patterns[file.spec].as_ref() else {
            continue;
        };
        let found = match_file(root, file, specs[file.spec].support, pattern, word);
        if let Ok(mut slot) = slots[at].lock() {
            *slot = found;
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..workers {
            let spawned = std::thread::Builder::new()
                .stack_size(STACK_BYTES)
                .spawn_scoped(scope, work);
            if spawned.is_err() {
                break;
            }
        }
        work();
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().ok().flatten())
        .collect()
}

/// A line's start byte, and each match's start and end byte on it.
type LineSpans = (usize, Vec<(usize, Option<usize>)>);

/// `file`'s matching lines, or None when it has none, is too large, or
/// cannot be read or parsed.
fn match_file(
    root: &Path,
    file: &Candidate,
    lang: SupportLang,
    pattern: &Pattern,
    word: Option<&str>,
) -> Option<FileMatch> {
    let full = root.join(&file.path);
    // symlink_metadata: a symlink may lead out of the folder, and is not
    // followed.
    let info = std::fs::symlink_metadata(&full).ok()?;
    if !info.is_file() || info.len() > FILE_BYTES_MAX {
        return None;
    }
    // NOTE: Lossy where node's `utf8` decoding is too: a bad byte is U+FFFD.
    let source = String::from_utf8_lossy(&std::fs::read(&full).ok()?).into_owned();
    if word.is_some_and(|word| !source.contains(word)) {
        return None;
    }
    let doc = StrDoc::try_new(&source, lang).ok()?;
    let tree = AstGrep::doc(doc);
    let top = tree.root();

    // Byte offsets in `source`, per line: where it starts, and each match's
    // start and end on it (the end is None when the match runs on to a later
    // line: marked to its first line's end).
    let mut by_line: BTreeMap<usize, LineSpans> = BTreeMap::new();
    for found in top.find_all(pattern) {
        let range = found.range();
        let (line, column) = found.start_pos().byte_point();
        let (end_line, end_column) = found.end_pos().byte_point();
        let entry = by_line
            .entry(line)
            .or_insert_with(|| (range.start - column, Vec::new()));
        entry
            .1
            .push((column, (end_line == line).then_some(end_column)));
    }
    if by_line.is_empty() {
        return None;
    }
    let mut lines: Vec<MatchLine> = Vec::with_capacity(by_line.len());
    for (line, (start, spans)) in by_line {
        let rest = source.get(start..)?;
        let text = rest.find('\n').map_or(rest, |newline| &rest[..newline]);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let ranges: Vec<(usize, usize)> = spans
            .into_iter()
            .map(|(from, to)| {
                let to = to.unwrap_or(text.len());
                (utf16_at(text, from), utf16_at(text, to))
            })
            .collect();
        lines.push(to_line(text, line as u64 + 1, merge_ranges(ranges)));
    }
    Some(FileMatch {
        path: file.path.clone(),
        lines,
        truncated: false,
    })
}

/// The UTF-16 offset of byte `at` in `text`, clamped to its end.
fn utf16_at(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[..at].encode_utf16().count()
}

/// Sorted, and nested or overlapping ones joined.
fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<Range> {
    ranges.sort_unstable();
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start < last[1] => last[1] = last[1].max(end),
            _ => merged.push([start, end]),
        }
    }
    merged
}

/// One match line, a long one cut to a window around its first match.
///
/// NOTE: The same cut as `search_text` makes, so both modes show a minified
/// line alike.
fn to_line(text: &str, line: u64, ranges: Vec<Range>) -> MatchLine {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= LINE_MAX {
        return MatchLine {
            line,
            text: text.to_owned(),
            ranges,
        };
    }
    let mut from = ranges.first().map_or(0, |range| range[0]).saturating_sub(LINE_LEAD);
    let mut to = units.len().min(from + LINE_MAX);
    // Never cut a surrogate pair in half.
    if is_low_surrogate(units[from]) {
        from -= 1;
    }
    if to < units.len() && is_low_surrogate(units[to]) {
        to -= 1;
    }
    MatchLine {
        line,
        text: String::from_utf16_lossy(&units[from..to]),
        ranges: ranges
            .into_iter()
            .filter(|range| range[0] >= from && range[0] < to)
            .map(|range| [range[0] - from, range[1].min(to) - from])
            .collect(),
    }
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pattern: &str, lang: Option<StructuralLang>) -> StructuralOptions {
        StructuralOptions {
            pattern: pattern.to_owned(),
            lang,
            globs: Vec::new(),
            max_files: 50,
            max_lines: MAX_LINES.default,
        }
    }

    fn project(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lwk-test-sg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in [
            ("a.py", "def foo(x):\n    return x\n\nclass K:\n    def m(self):\n        pass\n"),
            ("tests/t.py", "def test_x():\n    pass\n"),
            ("src/a.rs", "fn one(a: i32) { a; }\nstruct S;\nimpl S { fn x(&self) {} }\nimpl Clone for S { fn clone(&self) -> S { S } }\n"),
            ("a.ts", "function f(a, b) { return a }\n"),
            ("a.js", "function g() { return 1 }\n"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        root.canonicalize().unwrap()
    }

    fn paths(answer: &StructuralAnswer) -> Vec<&str> {
        answer.files.iter().map(|file| file.path.as_str()).collect()
    }

    #[test]
    fn fixes_a_bodiless_declaration() {
        use StructuralLang::*;
        let fixed = |lang, pattern| fixed_pattern(lang, pattern);
        assert_eq!(fixed(Python, "def $F($$$ARGS)").as_deref(), Some("def $F($$$ARGS): $$$BODY"));
        assert_eq!(fixed(Python, "async def $F()").as_deref(), Some("async def $F(): $$$BODY"));
        assert_eq!(fixed(Python, "class $C:").as_deref(), Some("class $C: $$$BODY"));
        assert_eq!(fixed(Python, "print($A)"), None);
        assert_eq!(fixed(Python, "define($A)"), None);
        assert_eq!(fixed(Rust, "fn $N($$$A)").as_deref(), Some("fn $N($$$A) { $$$BODY }"));
        assert_eq!(fixed(Rust, "pub(crate) async fn $N()").as_deref(), Some("pub(crate) async fn $N() { $$$BODY }"));
        assert_eq!(fixed(Rust, "impl $T for $U").as_deref(), Some("impl $T for $U { $$$BODY }"));
        assert_eq!(fixed(Rust, "impl<T> $X").as_deref(), Some("impl<T> $X { $$$BODY }"));
        assert_eq!(fixed(Rust, "fn $N() {}"), None);
        assert_eq!(fixed(Rust, "fnord($A)"), None);
        for lang in [Ts, Tsx, Js, Jsx] {
            assert_eq!(fixed(lang, "function $F($$$A)").as_deref(), Some("function $F($$$A) { $$$BODY }"));
            assert_eq!(fixed(lang, "export async function $F()").as_deref(), Some("export async function $F() { $$$BODY }"));
            assert_eq!(fixed(lang, "functional($A)"), None);
        }
        assert_eq!(fixed(Go, "func $F($$$A)").as_deref(), Some("func $F($$$A) { $$$BODY }"));
        assert_eq!(fixed(Go, "func ($R $T) $M()").as_deref(), Some("func ($R $T) $M() { $$$BODY }"));
        assert_eq!(fixed(Json, "def x"), None);
    }

    #[test]
    fn runs_the_fixed_pattern_and_says_so() {
        let root = project("fix");
        let target = Target::Folder(root.clone());
        for (pattern, lang, fixed, files) in [
            ("def $F($$$ARGS)", Some(StructuralLang::Python), "def $F($$$ARGS): $$$BODY", vec!["a.py", "tests/t.py"]),
            ("def $F($$$ARGS)", None, "def $F($$$ARGS): $$$BODY", vec!["a.py", "tests/t.py"]),
            ("fn $N($$$A)", Some(StructuralLang::Rust), "fn $N($$$A) { $$$BODY }", vec!["src/a.rs"]),
            ("impl $T for $U", Some(StructuralLang::Rust), "impl $T for $U { $$$BODY }", vec!["src/a.rs"]),
            ("function $F($$$A)", Some(StructuralLang::Ts), "function $F($$$A) { $$$BODY }", vec!["a.ts"]),
            ("function $F($$$A)", Some(StructuralLang::Js), "function $F($$$A) { $$$BODY }", vec!["a.js"]),
        ] {
            let got = search_structural(&target, &options(pattern, lang)).unwrap();
            assert_eq!(paths(&got), files, "{pattern} {lang:?}");
            let note = got.note.unwrap_or_default();
            assert!(
                note.starts_with(&format!("searched `{fixed}` (added the body the pattern needs")),
                "{pattern} {lang:?}: {note}"
            );
        }
        // A pattern that parses as it is is run as it is.
        let plain = search_structural(&target, &options("return $X", None)).unwrap();
        assert_eq!(paths(&plain), vec!["a.js", "a.py", "a.ts"]);
        assert_eq!(plain.note, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_note_says_what_the_fix_skips() {
        use StructuralLang::*;
        let note = |lang, pattern: &str| fix_note(lang, pattern, &fixed_pattern(lang, pattern).unwrap());
        assert_eq!(
            note(Rust, "fn $N($$$A)"),
            "searched `fn $N($$$A) { $$$BODY }` (added the body the pattern needs; it skips fns with `pub`, a return type or generics; to match those, add `pub`, `-> $R` or `<$G>` after the name)"
        );
        assert_eq!(
            note(Rust, "pub fn $N($$$A) -> $R"),
            "searched `pub fn $N($$$A) -> $R { $$$BODY }` (added the body the pattern needs; it skips fns with generics; to match those, add `<$G>` after the name)"
        );
        assert_eq!(
            note(Rust, "impl $T"),
            "searched `impl $T { $$$BODY }` (added the body the pattern needs; it skips trait or generic impls; to match those, add ` for $U` after the type or `<$G>` after `impl`)"
        );
        assert_eq!(
            note(Ts, "function $F($$$A)"),
            "searched `function $F($$$A) { $$$BODY }` (added the body the pattern needs; it skips functions with a return type or type parameters; to match those, add `: $R` after the parameters or `<$G>` after the name)"
        );
        assert_eq!(
            note(Js, "function $F($$$A)"),
            "searched `function $F($$$A) { $$$BODY }` (added the body the pattern needs)"
        );
        assert_eq!(
            note(Python, "def $F($$$A)"),
            "searched `def $F($$$A): $$$BODY` (added the body the pattern needs; it skips defs with a return annotation; to match those, add `-> $R` before the colon)"
        );
        assert_eq!(
            note(Python, "class $C"),
            "searched `class $C: $$$BODY` (added the body the pattern needs; it skips classes with bases; to match those, add `($$$B)` after the name)"
        );
        assert_eq!(
            note(Python, "def $F($$$A) -> $R"),
            "searched `def $F($$$A) -> $R: $$$BODY` (added the body the pattern needs)"
        );
    }

    #[test]
    fn fixes_ahead_for_a_known_language_only() {
        use StructuralLang::*;
        let ahead = |pattern, lang, extension| fix_ahead(pattern, lang, extension).map(|(fixed, _)| fixed);
        assert_eq!(ahead("def $F($$$A)", Some(Python), None).as_deref(), Some("def $F($$$A): $$$BODY"));
        assert_eq!(ahead("def $F($$$A)", None, Some("py")).as_deref(), Some("def $F($$$A): $$$BODY"));
        assert_eq!(ahead("def $F($$$A)", None, None).as_deref(), Some("def $F($$$A): $$$BODY"));
        assert_eq!(ahead("fn $N($$$A)", None, None).as_deref(), Some("fn $N($$$A) { $$$BODY }"));
        assert_eq!(ahead("impl $T for $U", None, Some("rs")).as_deref(), Some("impl $T for $U { $$$BODY }"));
        assert_eq!(ahead("function $F($$$A)", None, None).as_deref(), Some("function $F($$$A) { $$$BODY }"));
        assert_eq!(ahead("function $F($$$A)", Some(Js), None).as_deref(), Some("function $F($$$A) { $$$BODY }"));
        // The note goes with it.
        let (_, note) = fix_ahead("fn $N($$$A)", Some(Rust), None).unwrap();
        assert!(note.contains("`-> $R`"), "{note}");
        // Nothing to fix: parses as it is, or no fix fits.
        assert_eq!(ahead("def $F($$$A): $$$B", Some(Python), None), None);
        assert_eq!(ahead("console.log($A)", None, None), None);
        assert_eq!(ahead("func $F($$$A)", Some(Go), None), None);
        // A fix for another language than the file's is not made.
        assert_eq!(ahead("def $F($$$A)", None, Some("rs")), None);
        assert_eq!(ahead("def $F($$$A)", Some(Rust), None), None);
        // Neither parses: left to the search, which names the fix.
        assert_eq!(ahead("def ($", Some(Python), None), None);
    }

    #[test]
    fn names_the_fix_when_neither_parses() {
        let root = project("hint");
        let target = Target::Folder(root.clone());
        let error = search_structural(&target, &options("def ($", Some(StructuralLang::Python))).unwrap_err();
        assert_eq!(error.code, super::super::types::KitErrorCode::Invalid);
        assert!(error.message.contains("`def $F($$$ARGS): $$$BODY`"), "{}", error.message);
        let error = search_structural(&target, &options("fn (", Some(StructuralLang::Rust))).unwrap_err();
        assert!(error.message.contains("`fn $N($$$ARGS) { $$$BODY }`"), "{}", error.message);
        // No fix to try: the message as before.
        let error = search_structural(&target, &options("foo(", Some(StructuralLang::Python))).unwrap_err();
        assert_eq!(error.message, "That pattern is not valid Python.");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn globs_narrow_the_files() {
        let root = project("globs");
        let target = Target::Folder(root.clone());
        let with = |globs: &[&str]| {
            let options = StructuralOptions {
                globs: globs.iter().map(|g| (*g).to_owned()).collect(),
                ..options("def $F($$$A): $$$B", None)
            };
            search_structural(&target, &options).map(|got| paths(&got).join(" "))
        };
        assert_eq!(with(&[]).unwrap(), "a.py tests/t.py");
        assert_eq!(with(&["*.py"]).unwrap(), "a.py tests/t.py");
        assert_eq!(with(&["!tests/**"]).unwrap(), "a.py");
        assert_eq!(with(&["tests/**"]).unwrap(), "tests/t.py");
        assert_eq!(with(&["*.none"]).unwrap(), "");
        let bad = with(&["a{"]).unwrap_err();
        assert!(bad.message.starts_with("Not a valid file filter"), "{}", bad.message);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_lines_and_searches_one_file() {
        let root = project("one");
        let target = Target::Folder(root.clone());
        let options_lines = |max_lines| StructuralOptions {
            max_lines,
            ..options("def $F($$$A): $$$B", None)
        };
        let all = search_structural(&target, &options_lines(2000)).unwrap();
        assert_eq!(all.files.iter().map(|f| f.lines.len()).sum::<usize>(), 3);
        for max_lines in 1..3 {
            let got = search_structural(&target, &options_lines(max_lines)).unwrap();
            assert_eq!(got.files.iter().map(|f| f.lines.len()).sum::<usize>(), max_lines);
            assert!(got.files.last().unwrap().truncated);
        }
        let file = Target::File {
            file: root.join("tests/t.py"),
            name: "typed.py".to_owned(),
        };
        let got = search_structural(&file, &options("def $F($$$A): $$$B", None)).unwrap();
        assert_eq!(paths(&got), vec!["typed.py"]);
        // In `lang` whatever its extension; with none, by its extension.
        let text = root.join("code.txt");
        std::fs::write(&text, "def g():\n    pass\n").unwrap();
        let file = Target::File { file: text, name: "code.txt".to_owned() };
        let got = search_structural(&file, &options("def $F(): $$$B", Some(StructuralLang::Python))).unwrap();
        assert_eq!(paths(&got), vec!["code.txt"]);
        let error = search_structural(&file, &options("def $F(): $$$B", None)).unwrap_err();
        assert!(error.message.contains("pass lang"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&root);
    }
}
