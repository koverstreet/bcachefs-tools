// SPDX-License-Identifier: GPL-2.0
//! c_xmacro_from_c!: an x-macro list C defines, for Rust - until its header
//! is converted and the list is a c_xmacro!.
//!
//!   c_xmacro_from_c!(BCH_INODE_OPTS, "fs/inode_format.h");
//!
//! is the c_xmacro! of the header's #define BCH_INODE_OPTS() - c_xmacro! {
//! BCH_INODE_OPTS(x) { (data_checksum, 8), ... } } - expanded the same way,
//! only not recorded: C has the list already. Rust gets the same NAME!(cb),
//! so nothing using the list changes when a c_xmacro! replaces it.
//!
//! The header is named relative to the crate, CARGO_MANIFEST_DIR: cargo sets
//! it, and kbuild sets it for mod.o (fs/Makefile.rust). It's include_bytes!'d
//! for rustc's dep-info, so a change to the list rebuilds what uses it.
//!
//! The #define is read as written, one call per entry:
//!
//!  - x(args) - or whatever the list's callback is called, one name for all
//!    its entries: BLK_ERRS()'s is BLK_STS - is (args);
//!  - OTHER() splices in another list from the same header. If OTHER's
//!    callback isn't this list's, the header defines it in terms of this
//!    one's, #define CB(params) x(template), and the splice is
//!    OTHER(CB(params) => (template)).
//!
//! Anything else is an error, as is a list defined more than once: there's
//! no preprocessor here to choose between definitions.

use std::collections::HashMap;

/// A function-like #define: its parameters, and its body as one line.
struct Define {
    params: Vec<String>,
    body:   String,
}

/// c_xmacro_from_c!'s input, NAME, "header": c_xmacro!'s input for the
/// list, and the header's path, to include_bytes!.
pub fn read(input: proc_macro::TokenStream) -> Result<(String, String), String> {
    use proc_macro::TokenTree;

    let toks: Vec<TokenTree> = input.into_iter().collect();
    let (name, header) = match toks.as_slice() {
        [TokenTree::Ident(name), TokenTree::Punct(comma), TokenTree::Literal(header)]
            if comma.as_char() == ',' => (name.to_string(), header.to_string()),
        _ => return Err("expected NAME, \"header\"".into()),
    };
    let header = header.strip_prefix('"').and_then(|h| h.strip_suffix('"'))
        .ok_or_else(|| format!("{name}: the header is a string, got {header}"))?;

    let dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| format!(
        "{name}: CARGO_MANIFEST_DIR isn't set - cargo sets it, and kbuild has to, for mod.o"))?;
    let path = format!("{dir}/{header}");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{name}: reading {path}: {e}"))?;

    let xmacro = translate(&defines(&text), &name).map_err(|e| format!("{e}, in {header}"))?;
    Ok((xmacro, path))
}

/// The function-like #defines in @text, by name: each name's definitions,
/// in order.
fn defines(text: &str) -> HashMap<String, Vec<Define>> {
    // As the preprocessor: lines spliced, then comments gone.
    let text = strip_comments(&text.replace("\\\n", " "));
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';

    let mut out: HashMap<String, Vec<Define>> = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('#') else { continue };
        let Some(rest) = rest.trim_start().strip_prefix("define") else { continue };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let (name, rest) = rest.split_at(rest.find(|c| !ident(c)).unwrap_or(rest.len()));
        // Function-like: its ( right after the name.
        let Some(rest) = rest.strip_prefix('(') else { continue };
        let Some(close) = rest.find(')') else { continue };

        let params = rest[..close].split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        out.entry(name.to_string()).or_default()
            .push(Define { params, body: rest[close + 1..].trim().to_string() });
    }
    out
}

/// @text without its comments: a block comment is a space, as in C, and a
/// line comment ends at its line. String and character literals are skipped.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '"' | '\'' => {
                out.push(c);
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() {
                            out.push(e);
                        }
                    } else if d == c {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// @body as the calls it is, NAME(args) each: their names and argument text.
fn calls(body: &str) -> Result<Vec<(String, String)>, String> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    let mut rest = body.trim_start();

    while !rest.is_empty() {
        let len = rest.find(|c| !ident(c)).unwrap_or(rest.len());
        let (name, after) = rest.split_at(len);
        let after = after.trim_start();
        if name.is_empty() || !after.starts_with('(') {
            let context: String = rest.chars().take(40).collect();
            return Err(format!("expected NAME(args), at \"{context}\""));
        }

        // The matching ), past nested parentheses and literals:
        let mut depth = 0;
        let mut quote = None;
        let mut escaped = false;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match quote {
                Some(_) if escaped => escaped = false,
                Some(_) if c == '\\' => escaped = true,
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None => match c {
                    '"' | '\'' => quote = Some(c),
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                },
            }
        }
        let end = end.ok_or_else(|| format!("{name}(: unterminated"))?;
        out.push((name.to_string(), after[1..end].trim().to_string()));
        rest = after[end + 1..].trim_start();
    }
    Ok(out)
}

/// @name's one definition in @defs.
fn one<'a>(defs: &'a HashMap<String, Vec<Define>>, name: &str) -> Result<&'a Define, String> {
    match defs.get(name).map(Vec::as_slice) {
        Some([d]) => Ok(d),
        None | Some([]) => Err(format!("no #define {name}()")),
        Some(ds) => Err(format!("#define {name}() {} times - choosing one takes the preprocessor",
                                ds.len())),
    }
}

/// The callback list @name's entries call: x unless they say otherwise.
fn callback(name: &str, calls: &[(String, String)]) -> Result<String, String> {
    let mut cb: Option<&str> = None;
    for (f, args) in calls {
        if args.is_empty() {
            continue;
        }
        match cb {
            None => cb = Some(f),
            Some(c) if c == f => {}
            Some(c) => return Err(format!("{name}()'s entries call both {c}() and {f}()")),
        }
    }
    Ok(cb.unwrap_or("x").to_string())
}

/// List @name, from @defs, as c_xmacro!'s input: NAME(cb) { entries }.
fn translate(defs: &HashMap<String, Vec<Define>>, name: &str) -> Result<String, String> {
    let d = one(defs, name)?;
    if !d.params.is_empty() {
        return Err(format!("#define {name}({}): a list takes no parameters", d.params.join(", ")));
    }
    let calls = calls(&d.body).map_err(|e| format!("{name}(): {e}"))?;
    let cb = callback(name, &calls)?;

    let mut entries = Vec::new();
    for (f, args) in &calls {
        if !args.is_empty() {
            entries.push(format!("({args})"));
            continue;
        }

        // A splice: OTHER(), its entries through a mapping macro if they
        // call something else.
        let sub = one(defs, f).map_err(|e| format!("{name}() splices {f}(): {e}"))?;
        let sub_cb = callback(f, &calls_of(f, sub)?)?;
        if sub_cb == cb {
            entries.push(format!("{f}()"));
            continue;
        }
        let map = one(defs, &sub_cb)
            .map_err(|e| format!("{name}() splices {f}(), whose entries call {sub_cb}(): {e}"))?;
        let template = match calls_of(&sub_cb, map)?.as_slice() {
            [(x, template)] if *x == cb => template.replace("##", " ## "),
            _ => return Err(format!("#define {sub_cb}({}) isn't one call of {cb}()",
                                    map.params.join(", "))),
        };
        entries.push(format!("{f}({sub_cb}({}) => ({template}))", map.params.join(", ")));
    }
    Ok(format!("{name}({cb}) {{ {} }}", entries.join(", ")))
}

fn calls_of(name: &str, d: &Define) -> Result<Vec<(String, String)>, String> {
    calls(&d.body).map_err(|e| format!("{name}(): {e}"))
}
