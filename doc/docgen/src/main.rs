//! bch-docgen: extract documentation from bcachefs source code.
//!
//! Two extraction mechanisms:
//!
//!   1. X-macro tables — structured data (e.g. BCH_BTREE_IDS()) becomes LaTeX lists
//!   2. DOC(key) comment blocks — prose in source becomes LaTeX fragments
//!
//! The options table and the CLI reference are `bcachefs _doc_gen`'s, from
//! the tables in the binary; run it first.
//!
//! Generated fragments live in doc/generated/ and are pulled into the
//! Principles of Operation via \bchdoc{key}.  The tool validates that every
//! \bchdoc reference has a corresponding source and every DOC() block is
//! referenced, failing the build on mismatches.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// X-macro parsing (adapted from fs/bch_bindgen/build.rs)
// ---------------------------------------------------------------------------

fn parse_xmacro(source: &str, macro_name: &str) -> Vec<Vec<String>> {
    let define_prefix = format!("#define {}", macro_name);
    let mut in_macro = false;
    let mut macro_text = String::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if !in_macro {
            if trimmed.starts_with(&define_prefix) {
                in_macro = true;
                if let Some(pos) = trimmed.find(&define_prefix) {
                    let after = &trimmed[pos + define_prefix.len()..];
                    let after = if let Some(i) = after.find(')') {
                        &after[i + 1..]
                    } else {
                        after
                    };
                    macro_text.push_str(after.trim_end_matches('\\').trim());
                    macro_text.push(' ');
                }
                if !trimmed.ends_with('\\') {
                    break;
                }
            }
        } else {
            macro_text.push_str(trimmed.trim_end_matches('\\').trim());
            macro_text.push(' ');
            if !trimmed.ends_with('\\') {
                break;
            }
        }
    }

    let mut entries = Vec::new();
    let bytes = macro_text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let Some(start) = macro_text[pos..].find("x(") else {
            break;
        };
        let open = pos + start + 2;
        let mut depth = 1usize;
        let mut i = open;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                i += 1;
            }
        }
        if depth == 0 {
            entries.push(split_xmacro_args(&macro_text[open..i]));
            pos = i + 1;
        } else {
            break;
        }
    }
    entries
}

/// Split comma-separated arguments, respecting nested parens and C strings.
fn split_xmacro_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut current = String::new();
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_string = !in_string;
                current.push(ch);
            }
            '\\' if in_string => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '(' if !in_string => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_string => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_string => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        args.push(tail);
    }
    args
}

/// Extract the content of adjacent C string literals and unescape.
fn join_c_strings(s: &str) -> String {
    let mut result = String::new();
    let mut in_string = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '"' {
            in_string = !in_string;
        } else if in_string {
            if ch == '\\' {
                match chars.peek() {
                    Some('n') => {
                        result.push('\n');
                        chars.next();
                    }
                    Some('t') => {
                        result.push('\t');
                        chars.next();
                    }
                    Some('\\') => {
                        result.push('\\');
                        chars.next();
                    }
                    Some('\'') => {
                        result.push('\'');
                        chars.next();
                    }
                    Some('"') => {
                        result.push('"');
                        chars.next();
                    }
                    _ => result.push(ch),
                }
            } else {
                result.push(ch);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// DOC() block extraction
// ---------------------------------------------------------------------------

/// If true, content is raw LaTeX and should not be processed by markup_to_latex.
struct DocBlock {
    raw_latex: bool,
    key: String,
    content: String,
    file: PathBuf,
    line: usize,
}

fn extract_doc_blocks(dir: &Path) -> Vec<DocBlock> {
    let mut blocks = Vec::new();
    walk_c_files(dir, &mut |path| {
        let Ok(content) = fs::read_to_string(path) else {
            return;
        };
        extract_doc_blocks_from(path, &content, &mut blocks);
    });
    blocks
}

fn walk_c_files(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_c_files(&path, f);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("h" | "c" | "rs")) {
            f(&path);
        }
    }
}

/// Parse a Rust string literal starting at the first `"` in @text: returns
/// the unescaped contents and the byte length consumed through the closing
/// quote. Handles \n, \t, \", \\, and \<newline> line continuations (which,
/// like rustc, skip all following whitespace).
fn parse_rust_string_literal(text: &str) -> Option<(String, usize)> {
    let start = text.find('"')?;
    let mut out = String::new();
    let mut chars = text[start + 1..].char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        match ch {
            '"' => return Some((out, start + 1 + i + ch.len_utf8())),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\n' => {
                    while chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
                        chars.next();
                    }
                }
                other => out.push(other),
            },
            _ => out.push(ch),
        }
    }
    None
}

fn extract_doc_blocks_from(path: &Path, source: &str, blocks: &mut Vec<DocBlock>) {
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // DOC_STRING(key) — the following Rust string literal (typically a
        // helptext constant, so one text serves --help and the PoO),
        // converted like a DOC() block.
        if let Some(rest) = trimmed.strip_prefix("// DOC_STRING(") {
            if let Some(key) = rest.strip_suffix(')') {
                let remaining = lines[i + 1..].join("\n");
                if let Some((content, consumed)) = parse_rust_string_literal(&remaining) {
                    blocks.push(DocBlock {
                        raw_latex: false,
                        key: key.trim().to_string(),
                        content: content.trim().to_string(),
                        file: path.to_path_buf(),
                        line: i + 2,
                    });
                    i += remaining[..consumed].matches('\n').count() + 1;
                }
            }
            i += 1;
            continue;
        }
        // DOC_LATEX(key) — raw LaTeX, passed through verbatim
        // DOC(key)       — simple markup, converted to LaTeX
        let (rest, raw_latex) = if let Some(rest) = trimmed.strip_prefix("/* DOC_LATEX(") {
            (rest, true)
        } else if let Some(rest) = trimmed.strip_prefix("/* DOC(") {
            (rest, false)
        } else {
            i += 1;
            continue;
        };
        if let Some(key) = rest.strip_suffix(')') {
            let key = key.trim().to_string();
            let start_line = i + 1;
            let mut content = String::new();
            i += 1;
            while i < lines.len() {
                let line = lines[i].trim();
                if line == "*/" || line.ends_with("*/") {
                    break;
                }
                /*
                 * Strip the comment's leading '*', then at most one space -
                 * not "* " as a unit. A continuation line indented with a tab
                 * (kernel style for nested list items) has no space after the
                 * '*', so matching the pair left the asterisk in the output
                 * and pdflatex died on the literal '*' before an \item.
                 */
                let stripped = if let Some(rest) = line.strip_prefix('*') {
                    rest.strip_prefix(' ').unwrap_or(rest)
                } else {
                    line
                };
                content.push_str(stripped);
                content.push('\n');
                i += 1;
            }
            blocks.push(DocBlock {
                raw_latex,
                key,
                content: content.trim().to_string(),
                file: path.to_path_buf(),
                line: start_line,
            });
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// LaTeX helpers
// ---------------------------------------------------------------------------

fn escape_latex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '_' => out.push_str("\\_"),
            '#' => out.push_str("\\#"),
            '%' => out.push_str("\\%"),
            '&' => out.push_str("\\&"),
            '$' => out.push_str("\\$"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            _ => out.push(ch),
        }
    }
    out
}

/// Convert inline markup to LaTeX: `code`, **bold**, *italic*.
fn convert_inline(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '`' {
            let mut code = String::new();
            for c in chars.by_ref() {
                if c == '`' {
                    break;
                }
                code.push(c);
            }
            out.push_str("\\texttt{");
            out.push_str(&escape_latex(&code));
            out.push('}');
        } else if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second *
            let mut bold = String::new();
            loop {
                match chars.next() {
                    Some('*') if chars.peek() == Some(&'*') => {
                        chars.next();
                        break;
                    }
                    Some(c) => bold.push(c),
                    None => break,
                }
            }
            out.push_str("\\textbf{");
            out.push_str(&escape_latex(&bold));
            out.push('}');
        } else if ch == '*' {
            let mut emph = String::new();
            for c in chars.by_ref() {
                if c == '*' {
                    break;
                }
                emph.push(c);
            }
            out.push_str("\\emph{");
            out.push_str(&escape_latex(&emph));
            out.push('}');
        } else if ch == '>' && out.ends_with('=') {
            // >= → $\geq$
            out.pop();
            out.push_str("$\\geq$");
        } else if ch == '-' && chars.peek() == Some(&'-') && {
            // -- → ---
            let after_second = {
                let mut tmp = chars.clone();
                tmp.next();
                tmp.peek().copied()
            };
            after_second == Some('-')
        } {
            chars.next();
            chars.next();
            out.push_str("---");
        } else if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            out.push_str("--");
        } else {
            match ch {
                '_' => out.push_str("\\_"),
                '#' => out.push_str("\\#"),
                '%' => out.push_str("\\%"),
                '&' => out.push_str("\\&"),
                '$' => out.push_str("\\$"),
                '{' => out.push_str("\\{"),
                '}' => out.push_str("\\}"),
                '~' => out.push_str("\\textasciitilde{}"),
                '^' => out.push_str("\\textasciicircum{}"),
                _ => out.push(ch),
            }
        }
    }
    out
}

/// Convert DOC() block markup to LaTeX.
///
/// Supported:
///   - paragraphs (blank lines)
///   - `code` → \texttt{code}, **bold** → \textbf{}, *italic* → \emph{}
///   - lines starting with "- " → \begin{itemize} lists
///   - lines starting with "[term] " → \begin{description} lists
///   - "  " continuation lines append to the previous description item
fn markup_to_latex(content: &str) -> String {
    let mut out = String::new();

    #[derive(PartialEq)]
    enum ListState { None, Itemize, Description }
    let mut list_state = ListState::None;
    let mut in_verbatim = false;

    for line in content.lines() {
        // Lines indented four or more spaces form a verbatim block -
        // command tables, examples - emitted raw:
        if line.starts_with("    ") {
            if !in_verbatim {
                match list_state {
                    ListState::Itemize => out.push_str("\\end{itemize}\n"),
                    ListState::Description => out.push_str("\\end{description}\n"),
                    ListState::None => {}
                }
                list_state = ListState::None;
                out.push_str("\\begin{verbatim}\n");
                in_verbatim = true;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_verbatim {
            out.push_str("\\end{verbatim}\n");
            in_verbatim = false;
        }
        if line.is_empty() {
            match list_state {
                ListState::Itemize => {
                    out.push_str("\\end{itemize}\n");
                    list_state = ListState::None;
                }
                ListState::Description => {
                    out.push_str("\\end{description}\n");
                    list_state = ListState::None;
                }
                ListState::None => {}
            }
            out.push('\n');
            continue;
        }
        if let Some(item) = line.strip_prefix("- ") {
            if list_state == ListState::Description {
                out.push_str("\\end{description}\n");
            }
            if list_state != ListState::Itemize {
                out.push_str("\\begin{itemize}\n");
                list_state = ListState::Itemize;
            }
            out.push_str("\\item ");
            out.push_str(&convert_inline(item));
            out.push('\n');
        } else if line.starts_with('[') {
            if let Some(close) = line.find("] ") {
                let term = &line[1..close];
                let desc = &line[close + 2..];
                if list_state == ListState::Itemize {
                    out.push_str("\\end{itemize}\n");
                }
                if list_state != ListState::Description {
                    out.push_str("\\begin{description}\n");
                    list_state = ListState::Description;
                }
                out.push_str("\\item[");
                out.push_str(&convert_inline(term));
                out.push_str("] ");
                out.push_str(&convert_inline(desc));
                out.push('\n');
            } else {
                // Not a description item, just a line starting with [
                if list_state != ListState::None {
                    match list_state {
                        ListState::Itemize => out.push_str("\\end{itemize}\n"),
                        ListState::Description => out.push_str("\\end{description}\n"),
                        _ => {}
                    }
                    list_state = ListState::None;
                }
                out.push_str(&convert_inline(line));
                out.push('\n');
            }
        } else if line.starts_with("  ") && list_state != ListState::None {
            // Continuation of the previous item
            out.push_str(&convert_inline(line.trim_start()));
            out.push('\n');
        } else {
            if list_state != ListState::None {
                match list_state {
                    ListState::Itemize => out.push_str("\\end{itemize}\n"),
                    ListState::Description => out.push_str("\\end{description}\n"),
                    _ => {}
                }
                list_state = ListState::None;
            }
            out.push_str(&convert_inline(line));
            out.push('\n');
        }
    }
    if in_verbatim {
        out.push_str("\\end{verbatim}\n");
    }
    match list_state {
        ListState::Itemize => out.push_str("\\end{itemize}\n"),
        ListState::Description => out.push_str("\\end{description}\n"),
        ListState::None => {}
    }
    out
}

// ---------------------------------------------------------------------------
// Simple enum lists from x-macros: x(name, value) → \item[{\tt name}]
// ---------------------------------------------------------------------------

struct EnumList {
    key: &'static str,
    header: &'static str,
    macro_name: &'static str,
    default: Option<&'static str>,
    /// Which x-macro argument index contains the doc string (None = no descriptions)
    doc_field: Option<usize>,
    /// Which x-macro argument index contains btree-style flags (appends annotations)
    flags_field: Option<usize>,
    /// Which x-macro argument index contains a date string (e.g. "2023-07")
    date_field: Option<usize>,
    /// Which x-macro argument index contains BCH_VERSION(major, minor)
    version_field: Option<usize>,
    /// Which x-macro argument index contains PASS_* flags (annotates + hides SILENT)
    pass_flags_field: Option<usize>,
}

const ENUM_LISTS: &[EnumList] = &[
    EnumList {
        key: "error-actions",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_ERROR_ACTIONS",
        default: Some("fix_safe"),
        doc_field: Some(2),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "csum-opts",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_CSUM_OPTS",
        default: Some("crc32c"),
        doc_field: None,
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "compression-opts",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_COMPRESSION_OPTS",
        default: Some("none"),
        doc_field: None,
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "str-hash-opts",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_STR_HASH_OPTS",
        default: Some("siphash"),
        doc_field: None,
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "btree-ids",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_BTREE_IDS",
        default: None,
        doc_field: Some(4),
        flags_field: Some(2),
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "time-stats",
        header: "fs/types.h",
        macro_name: "BCH_TIME_STATS",
        default: None,
        doc_field: Some(1),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "sb-fields",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_SB_FIELDS",
        default: None,
        doc_field: Some(2),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "jset-entry-types",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_JSET_ENTRY_TYPES",
        default: None,
        doc_field: Some(2),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "counters",
        header: "fs/sb/counters_format.h",
        macro_name: "BCH_PERSISTENT_COUNTERS",
        default: None,
        doc_field: Some(3),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "bkey-types",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_BKEY_TYPES",
        default: None,
        doc_field: Some(3),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: None,
    },
    EnumList {
        key: "metadata-versions",
        header: "fs/bcachefs_format_types.h",
        macro_name: "BCH_METADATA_VERSIONS",
        default: None,
        doc_field: Some(2),
        flags_field: None,
        date_field: Some(3),
        version_field: Some(1),
        pass_flags_field: None,
    },
    EnumList {
        key: "recovery-passes",
        header: "fs/init/passes_format.h",
        macro_name: "BCH_RECOVERY_PASSES",
        default: None,
        doc_field: Some(4),
        flags_field: None,
        date_field: None,
        version_field: None,
        pass_flags_field: Some(2),
    },
];

/// Parse btree-style flags field and return human-readable annotations.
fn btree_flags_annotations(flags: &str) -> Vec<&'static str> {
    let mut annotations = Vec::new();
    if flags.contains("BTREE_IS_snapshots") {
        annotations.push("snapshot-aware");
    } else if flags.contains("BTREE_IS_snapshot_field") {
        annotations.push("snapshot-field");
    }
    if flags.contains("BTREE_IS_extents") {
        annotations.push("extent-based");
    }
    if flags.contains("BTREE_IS_write_buffer") {
        annotations.push("write-buffered");
    }
    annotations
}

/// Parse PASS_* flags and return (annotations, is_silent).
/// Handles compound macros like PASS_FSCK_ALLOC = PASS_FSCK|PASS_ALLOC.
fn pass_flags_annotations(flags: &str) -> (Vec<&'static str>, bool) {
    let mut annotations = Vec::new();
    let is_silent = flags.contains("PASS_SILENT");
    if flags.contains("PASS_ALWAYS") {
        annotations.push("always");
    }
    if flags.contains("PASS_FSCK_ALLOC") || flags.contains("PASS_FSCK") {
        annotations.push("fsck");
    }
    if flags.contains("PASS_ONLINE") {
        annotations.push("online");
    }
    if flags.contains("PASS_FSCK_ALLOC") || flags.contains("PASS_ALLOC") {
        annotations.push("alloc");
    }
    if flags.contains("PASS_NODEFER") {
        annotations.push("nodefer");
    }
    (annotations, is_silent)
}

/// Extract "major.minor" from "BCH_VERSION(major, minor)".
fn parse_bch_version(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix("BCH_VERSION(")?;
    let inner = inner.strip_suffix(')')?;
    let mut parts = inner.split(',');
    let major = parts.next()?.trim();
    let minor = parts.next()?.trim();
    Some(format!("{major}.{minor}"))
}

fn generate_enum_list(el: &EnumList, entries: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("\\begin{description}\n");
    for entry in entries {
        let name = &entry[0];
        let escaped = escape_latex(name);

        // Parse pass flags: skip PASS_SILENT entries, collect annotations
        let (pass_annotations, is_silent) = el
            .pass_flags_field
            .and_then(|i| entry.get(i))
            .map(|s| pass_flags_annotations(s))
            .unwrap_or_default();
        if is_silent {
            continue;
        }

        let doc = el
            .doc_field
            .and_then(|i| entry.get(i))
            .map(|s| join_c_strings(s))
            .filter(|s| !s.is_empty());
        let btree_annotations = el
            .flags_field
            .and_then(|i| entry.get(i))
            .map(|s| btree_flags_annotations(s))
            .unwrap_or_default();

        let label = format!("\\label{{{}:{}}}", el.key, name);
        if el.default == Some(name.as_str()) {
            out.push_str(&format!("{label}\\item[{{\\tt {escaped}}}] (default)"));
        } else {
            out.push_str(&format!("{label}\\item[{{\\tt {escaped}}}]"));
        }
        let version = el
            .version_field
            .and_then(|i| entry.get(i))
            .and_then(|s| parse_bch_version(s));
        let date = el
            .date_field
            .and_then(|i| entry.get(i))
            .map(|s| join_c_strings(s))
            .filter(|s| !s.is_empty());
        match (&version, &date) {
            (Some(v), Some(d)) => out.push_str(&format!(" \\textit{{({v}, {d})}}")),
            (Some(v), None) => out.push_str(&format!(" \\textit{{({v})}}")),
            (None, Some(d)) => out.push_str(&format!(" \\textit{{({d})}}")),
            (None, None) => {}
        }
        if let Some(desc) = doc {
            out.push_str(&format!(" {}", escape_latex(&desc)));
        }
        // Combine all annotations (btree flags + pass flags)
        let mut all_annotations = btree_annotations;
        all_annotations.extend(pass_annotations);
        if !all_annotations.is_empty() {
            out.push_str(&format!(" \\textit{{({})}}", all_annotations.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("\\end{description}\n");
    out
}

// ---------------------------------------------------------------------------
// Reference validation
// ---------------------------------------------------------------------------

fn extract_bchdoc_refs(tex: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut pos = 0;
    while let Some(start) = tex[pos..].find("\\bchdoc{") {
        let key_start = pos + start + 8;
        if let Some(end) = tex[key_start..].find('}') {
            refs.push(tex[key_start..key_start + end].to_string());
            pos = key_start + end + 1;
        } else {
            break;
        }
    }
    refs
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn find_root() -> PathBuf {
    if let Some(path) = std::env::args().nth(1) {
        return PathBuf::from(path);
    }
    let mut dir = std::env::current_dir().unwrap();
    loop {
        if dir.join("fs").is_dir() {
            return dir;
        }
        if !dir.pop() {
            eprintln!("error: cannot find bcachefs-tools root (no fs/ found)");
            std::process::exit(1);
        }
    }
}

fn main() {
    let root = find_root();
    let generated_dir = root.join("doc/generated");
    fs::create_dir_all(&generated_dir).unwrap();

    let mut available_keys = HashSet::new();
    let mut errors = 0;

    // --- DOC() blocks from C and Rust sources ---
    let mut doc_blocks = extract_doc_blocks(&root.join("fs"));
    doc_blocks.append(&mut extract_doc_blocks(&root.join("c_src")));
    doc_blocks.append(&mut extract_doc_blocks(&root.join("src")));

    for block in &doc_blocks {
        let latex = if block.raw_latex {
            format!("{}\n", block.content)
        } else {
            markup_to_latex(&block.content)
        };
        fs::write(generated_dir.join(format!("{}.tex", block.key)), &latex).unwrap();
        if !available_keys.insert(block.key.clone()) {
            eprintln!(
                "error: duplicate DOC({}) in {}:{}",
                block.key,
                block.file.display(),
                block.line
            );
            errors += 1;
        }
    }

    // --- Simple enum lists ---
    for el in ENUM_LISTS {
        let source = fs::read_to_string(root.join(el.header)).unwrap();
        let entries = parse_xmacro(&source, el.macro_name);
        if entries.is_empty() {
            eprintln!("warning: {} in {} produced no entries", el.macro_name, el.header);
            continue;
        }
        let latex = generate_enum_list(el, &entries);
        fs::write(generated_dir.join(format!("{}.tex", el.key)), &latex).unwrap();
        available_keys.insert(el.key.into());
    }

    // --- Validate references ---
    let tex_path = root.join("doc/bcachefs-principles-of-operation.tex");
    let tex = fs::read_to_string(&tex_path).unwrap();
    let mut refs = extract_bchdoc_refs(&tex);
    // Also scan DOC_LATEX blocks for nested \bchdoc references
    for block in &doc_blocks {
        if block.raw_latex {
            refs.extend(extract_bchdoc_refs(&block.content));
        }
    }
    let ref_set: HashSet<_> = refs.iter().cloned().collect();

    for r in &refs {
        if !available_keys.contains(r)
            && !generated_dir.join(format!("{r}.tex")).exists()
        {
            eprintln!(
                "error: \\bchdoc{{{r}}} in PoO has no matching DOC({r}) in source"
            );
            errors += 1;
        }
    }

    for key in &available_keys {
        if !ref_set.contains(key) {
            eprintln!("warning: DOC({key}) in source is not referenced by PoO");
        }
    }

    if errors > 0 {
        eprintln!("\n{errors} error(s)");
        std::process::exit(1);
    }

    eprintln!(
        "bch-docgen: {} fragment(s) in {}",
        available_keys.len(),
        generated_dir.display()
    );
}
