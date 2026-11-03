//! A module's Rust as Jev reads it: comments removed line for line (so a comment cannot argue the answer and line
//! numbers stay true), cut into parts of a few lines to point at, and into windows small enough to read whole.

/// One line of a module's source, numbered from 1, without its comments.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub number: usize,
    pub text: String,
}

/// A few lines that say something, offered to Jev as one place a finding may be.
#[derive(Debug, Clone)]
pub struct Part {
    /// `L<first>-L<last>`: how Jev names the part it chose.
    pub key: String,
    pub lines: Vec<Line>,
}

/// Lines in a part: few enough that Jev's choice of a part is nearly a line, enough that a long module is not a
/// choice among hundreds.
const PART_LINES: usize = 6;

/// Lines that say something per window. Jev's answers fall with state it does not need (TypeSafe's model card), and in
/// the census of Rita's apps on 26 Oct 2026 a 1,000-line module read whole gave no line Jev could point at.
pub const WINDOW_LINES: usize = 160;

/// Characters of a line Jev reads; the rest is cut. A window must fit one ask, and a route that holds the key may hold
/// an ask to a size made from this (Katagami's: 160 lines of 160 characters and the specs, about 40 KB). A line this
/// long is data, not logic to point at: rustfmt breaks code at 100.
pub const LINE_CHARS: usize = 160;

/// A module's source with every comment blanked, keeping each newline, so line numbers stay true. Strings (plain,
/// byte and raw) and character literals are kept: a status string is evidence.
pub fn without_comments(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i = skip_block_comment(&chars, i + 2, &mut out);
        } else if let Some(end) = raw_string_end(&chars, i) {
            out.extend(&chars[i..end]);
            i = end;
        } else if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(chars.len());
            out.extend(&chars[i..end]);
            i = end;
        } else if let Some(end) = char_literal_end(&chars, i) {
            out.extend(&chars[i..end]);
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    debug_assert_eq!(out.matches('\n').count(), source.matches('\n').count());
    out
}

/// Past a block comment (nested ones too), keeping its newlines; `i` is just inside its opening.
fn skip_block_comment(chars: &[char], mut i: usize, out: &mut String) -> usize {
    let mut depth = 1;
    while i < chars.len() && depth > 0 {
        match (chars[i], chars.get(i + 1).copied()) {
            ('/', Some('*')) => (depth, i) = (depth + 1, i + 2),
            ('*', Some('/')) => (depth, i) = (depth - 1, i + 2),
            (c, _) => {
                if c == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
        }
    }
    i
}

/// The end of a raw string (`r"..."`, `r#"..."#`, `br"..."`) starting at `i`, if one does.
fn raw_string_end(chars: &[char], i: usize) -> Option<usize> {
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        return None;
    }
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    while j < chars.len() {
        if chars[j] == '"' && (0..hashes).all(|k| chars.get(j + 1 + k) == Some(&'#')) {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(chars.len())
}

/// The end of a character literal (`'a'`, `'\n'`) starting at `i`, if one does; a lifetime (`'a`) is not one.
fn char_literal_end(chars: &[char], i: usize) -> Option<usize> {
    if chars[i] != '\'' {
        return None;
    }
    match (chars.get(i + 1), chars.get(i + 2), chars.get(i + 3)) {
        (Some('\\'), Some(_), Some('\'')) => Some(i + 4),
        (Some(c), Some('\''), _) if *c != '\\' && *c != '\'' => Some(i + 3),
        _ => None,
    }
}

/// The module's lines, numbered from 1, without comments or trailing space.
pub fn lines_of(source: &str) -> Vec<Line> {
    without_comments(source)
        .split('\n')
        .enumerate()
        .map(|(k, text)| Line {
            number: k + 1,
            text: cut(text.trim_end()),
        })
        .collect()
}

/// A line cut at [`LINE_CHARS`].
pub(super) fn cut(text: &str) -> String {
    match text.char_indices().nth(LINE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

fn says_something(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Braces, punctuation and `use` lines: nothing to point a finding at.
fn trivial(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.chars().all(|c| "{}()[];,".contains(c)) || t.starts_with("use ")
}

/// Lines that say something.
pub fn said(lines: &[Line]) -> usize {
    lines.iter().filter(|l| says_something(&l.text)).count()
}

/// The lines in parts of a few lines each, broken at blank lines, keeping only lines worth pointing at.
pub fn parts_of(lines: &[Line]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut current: Vec<Line> = Vec::new();
    let flush = |current: &mut Vec<Line>, parts: &mut Vec<Part>| {
        let kept: Vec<Line> = current.drain(..).filter(|l| !trivial(&l.text)).collect();
        if let (Some(first), Some(last)) = (kept.first(), kept.last()) {
            let key = format!("L{}-L{}", first.number, last.number);
            parts.push(Part { key, lines: kept });
        }
    };
    for line in lines {
        if !says_something(&line.text) {
            flush(&mut current, &mut parts);
            continue;
        }
        current.push(line.clone());
        if current.iter().filter(|l| !trivial(&l.text)).count() >= PART_LINES {
            flush(&mut current, &mut parts);
        }
    }
    flush(&mut current, &mut parts);
    parts
}

/// Whether a line starts a top-level item, where a window may break.
fn starts_item(text: &str) -> bool {
    let t = text
        .strip_prefix("pub ")
        .or_else(|| text.strip_prefix("pub(crate) "))
        .unwrap_or(text);
    [
        "fn ",
        "async fn ",
        "impl",
        "struct ",
        "enum ",
        "const ",
        "static ",
        "mod ",
        "trait ",
        "type ",
        "#[",
        "temper_module!",
    ]
    .iter()
    .any(|start| t.starts_with(start))
}

/// The module in windows of at most [`WINDOW_LINES`] lines that say something, broken at a top-level item once a
/// window is half full. A small module is one window.
pub fn windows_of(lines: &[Line]) -> Vec<Vec<Line>> {
    let mut windows = Vec::new();
    let mut current: Vec<Line> = Vec::new();
    for line in lines {
        if starts_item(&line.text) && said(&current) >= WINDOW_LINES / 2 {
            windows.push(std::mem::take(&mut current));
        }
        current.push(line.clone());
        if said(&current) >= WINDOW_LINES {
            windows.push(std::mem::take(&mut current));
        }
    }
    if said(&current) > 0 {
        windows.push(current);
    }
    if windows.is_empty() {
        windows.push(lines.to_vec());
    }
    windows
}
