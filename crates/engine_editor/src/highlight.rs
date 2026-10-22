//! Luau's highlighting in the code editor (ADR 0107 E8-1c,
//! `docs/research/luau-highlighting.md`): a line cut into runs of a kind (a keyword, a
//! string, a comment, a number, a function's name, the language's own names, a type), by
//! Luau's lexical rules. What a line starts inside (a long comment or string, a string
//! going on past its line's end) comes from the lines before it: kept per line, and made
//! again from the first line changed up to the lines shown. A tab's [`Language`] picks the
//! rules: Slang's are `crate::slang_highlight`'s, text files' `crate::text_highlight`'s.

use crate::shell::Token;
use std::path::Path;

/// The language of a code tab, by its file's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Language {
    /// A Luau script (`.luau`).
    #[default]
    Luau,
    /// A Slang module (`.slang`, ADR 0107's E10-2a).
    Slang,
    /// A text file of the project: settings, data, localization, notes.
    Text(TextKind),
}

/// A text file's format, by its extension: how it is colored and outlined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
    /// `.toml`.
    Toml,
    /// `.ron`.
    Ron,
    /// `.json`.
    Json,
    /// Fluent's `.ftl` (the game's text, ADR 0084).
    Fluent,
    /// `.md`.
    Markdown,
    /// `.txt`, `.cfg`, `.ini`, `.yaml`, `.csv`: as it is.
    Plain,
}

impl Language {
    /// The language of the file at `path`, if the code editor opens it.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "luau" => Self::Luau,
            "slang" => Self::Slang,
            "toml" => Self::Text(TextKind::Toml),
            "ron" => Self::Text(TextKind::Ron),
            "json" => Self::Text(TextKind::Json),
            "ftl" => Self::Text(TextKind::Fluent),
            "md" | "markdown" => Self::Text(TextKind::Markdown),
            "txt" | "cfg" | "ini" | "yaml" | "yml" | "csv" => Self::Text(TextKind::Plain),
            _ => return None,
        })
    }

    /// Whether its checker answers more than a check: completion, hover, definitions and
    /// signatures. Slang's only checks, until its language server (E10-2b).
    pub(crate) fn answers(self) -> bool {
        self == Self::Luau
    }

    /// The runs of `line`, which starts in `state`, by this language's rules
    /// ([`line`]'s).
    pub(crate) fn cut(
        self,
        line: &str,
        state: State,
        out: &mut Vec<(usize, usize, Token)>,
    ) -> State {
        match self {
            Self::Luau => self::line(line, state, out),
            Self::Slang => crate::slang_highlight::line(line, state, out),
            Self::Text(kind) => crate::text_highlight::line(kind, line, state, out),
        }
    }
}

/// What a line starts inside, from the lines before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum State {
    /// Code.
    #[default]
    Code,
    /// A long comment `--[==[`, of that many `=`.
    Comment(u8),
    /// A long string `[==[`, of that many `=`.
    Long(u8),
    /// A quoted string whose line ended in `\` or `\z`: it goes on, closed by this quote.
    Quoted(u8),
    /// A block comment, `/*` without its `*/` (Slang).
    Block,
}

/// Luau's reserved words.
const RESERVED: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// The standard library's globals (Luau's `typeof` among them), and `_G`.
const BUILTINS: &[&str] = &[
    "_G",
    "_VERSION",
    "assert",
    "bit32",
    "buffer",
    "collectgarbage",
    "coroutine",
    "debug",
    "error",
    "gcinfo",
    "getfenv",
    "getmetatable",
    "ipairs",
    "loadstring",
    "math",
    "newproxy",
    "next",
    "os",
    "pairs",
    "pcall",
    "print",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "require",
    "select",
    "setfenv",
    "setmetatable",
    "string",
    "table",
    "tonumber",
    "tostring",
    "type",
    "typeof",
    "unpack",
    "utf8",
    "vector",
    "xpcall",
];

/// The runs of `line`, which starts in `state`: each a byte range and its kind, pushed onto
/// `out` (plain code left out); and the state the next line starts in.
pub(crate) fn line(line: &str, state: State, out: &mut Vec<(usize, usize, Token)>) -> State {
    let b = line.as_bytes();
    let mut i = 0;
    match state {
        State::Code | State::Block => {}
        State::Comment(level) | State::Long(level) => {
            let kind = if matches!(state, State::Comment(_)) {
                Token::Comment
            } else {
                Token::String
            };
            let Some(end) = close_long(b, 0, level) else {
                out.push((0, b.len(), kind));
                return state;
            };
            out.push((0, end, kind));
            i = end;
        }
        State::Quoted(quote) => {
            let (end, goes_on) = quoted(b, 0, quote);
            out.push((0, end, Token::String));
            if goes_on {
                return state;
            }
            i = end;
        }
    }
    // Brace depths of the interpolated strings' expressions open, the innermost last.
    let mut interpolated: Vec<u32> = Vec::new();
    // What the last word or sign was: a function's name comes after `function`, a type's
    // after `:`, `::`, `->` and `type`.
    let mut after = After::Nothing;
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        // Comments: `--` to the line's end, `--[==[` to its close.
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            if let Some((level, open)) = long_open(b, i + 2) {
                match close_long(b, open, level) {
                    Some(end) => {
                        out.push((i, end, Token::Comment));
                        i = end;
                        continue;
                    }
                    None => {
                        out.push((i, b.len(), Token::Comment));
                        return State::Comment(level);
                    }
                }
            }
            out.push((i, b.len(), Token::Comment));
            return State::Code;
        }
        if c == b'"' || c == b'\'' {
            let (end, goes_on) = quoted(b, i + 1, c);
            out.push((i, end, Token::String));
            if goes_on {
                return State::Quoted(c);
            }
            i = end;
            after = After::Nothing;
            continue;
        }
        // An interpolated string's text, up to its end or an expression's `{`; and its
        // text again after the expression's `}`.
        let resumes = c == b'}' && interpolated.last() == Some(&0);
        if c == b'`' || resumes {
            if resumes {
                interpolated.pop();
            }
            let (end, expression) = interpolated_text(b, i + 1);
            out.push((i, end, Token::String));
            if expression {
                interpolated.push(0);
            }
            i = end;
            after = After::Nothing;
            continue;
        }
        if let Some(depth) = interpolated.last_mut() {
            match c {
                b'{' => *depth += 1,
                b'}' => *depth -= 1,
                _ => {}
            }
        }
        if c == b'['
            && let Some((level, open)) = long_open(b, i)
        {
            match close_long(b, open, level) {
                Some(end) => {
                    out.push((i, end, Token::String));
                    i = end;
                    after = After::Nothing;
                    continue;
                }
                None => {
                    out.push((i, b.len(), Token::String));
                    return State::Long(level);
                }
            }
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let end = number(b, i);
            out.push((i, end, Token::Number));
            i = end;
            after = After::Nothing;
            continue;
        }
        // `@native` and the other attributes.
        if c == b'@'
            && b.get(i + 1)
                .is_some_and(|&n| n.is_ascii_alphabetic() || n == b'_')
        {
            let end = word_end(b, i + 1);
            out.push((i, end, Token::Keyword));
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let end = word_end(b, i);
            let kind = word_kind(line, i, end, after);
            if let Some(kind) = kind {
                out.push((i, end, kind));
            }
            let word = &line[i..end];
            after = match kind {
                Some(Token::Keyword) if word == "function" => After::Function,
                Some(Token::Keyword) if word == "type" => After::Type,
                // `function Door.on_tick`: the name after the dots.
                _ if after == After::Function && next_byte(b, end) == Some(b'.') => After::Function,
                _ => After::Nothing,
            };
            i = end;
            continue;
        }
        after = match c {
            b':' if b.get(i + 1) == Some(&b':') => {
                i += 1;
                After::Colon
            }
            b':' => After::Colon,
            b'>' if i > 0 && b[i - 1] == b'-' => After::Colon,
            b'.' if after == After::Function => After::Function,
            _ => After::Nothing,
        };
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    State::Code
}

/// What came last before a word, for what the word is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Nothing,
    /// `function` (and its name's dots): a function's name comes.
    Function,
    /// `:`, `::` or `->`: a type, unless a call follows (a method's `obj:name(`).
    Colon,
    /// `type`: the type's name comes.
    Type,
}

/// What the word `line[at..end]` is, coming after `after`; `None` for plain code.
fn word_kind(line: &str, at: usize, end: usize, after: After) -> Option<Token> {
    let b = line.as_bytes();
    let word = &line[at..end];
    let next = next_byte(b, end);
    // A field or a method (`a.end`, `a:type`): a name, whatever its word.
    let member = at > 0 && matches!(b[at - 1], b'.' | b':') && !(at > 1 && b[at - 2] == b'.');
    let calls = matches!(next, Some(b'(' | b'{' | b'"' | b'\'' | b'`'))
        || line[end..].trim_start().starts_with("[[");
    if !member && RESERVED.contains(&word) {
        // `function` followed by `,` or `:` is a name (a table's key).
        return (word != "function" || !matches!(next, Some(b',' | b':')))
            .then_some(Token::Keyword);
    }
    if after == After::Function {
        return Some(Token::Function);
    }
    if after == After::Type {
        return Some(Token::Type);
    }
    if !member && contextual(line, at, end, word) {
        return Some(Token::Keyword);
    }
    if calls {
        return Some(Token::Function);
    }
    if after == After::Colon && b[..at].ends_with(b"::") {
        return Some(Token::Type);
    }
    if after == After::Colon && !member_call(b, at) {
        return Some(Token::Type);
    }
    if !member && (word == "self" || BUILTINS.contains(&word)) {
        return Some(Token::Builtin);
    }
    None
}

/// Whether the `:` before `at` is a method's (`obj:name`, no space between), not a type's.
fn member_call(b: &[u8], at: usize) -> bool {
    at > 0 && b[at - 1] == b':' && !(at > 1 && b[at - 2] == b':')
}

/// Whether `word` is a keyword there: Luau's contextual ones are names elsewhere.
/// `continue` is one but as a field; `export`, `type` and `const` start a statement, a
/// name after them (`type function` too).
fn contextual(line: &str, at: usize, end: usize, word: &str) -> bool {
    let next_is_name = line[end..]
        .strip_prefix([' ', '\t'])
        .map(str::trim_start)
        .and_then(|rest| rest.bytes().next())
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
    let starts = line[..at].trim().is_empty() || line[..at].trim_end().ends_with("export");
    match word {
        "continue" => !matches!(next_byte(line.as_bytes(), end), Some(b'(' | b'=' | b'.')),
        "export" => starts && next_is_name,
        "type" | "const" => starts && next_is_name,
        _ => false,
    }
}

/// The first byte from `at` on that is not a space.
pub(crate) fn next_byte(b: &[u8], at: usize) -> Option<u8> {
    b[at..].iter().copied().find(|c| *c != b' ' && *c != b'\t')
}

/// Where a name starting at `at` ends.
pub(crate) fn word_end(b: &[u8], at: usize) -> usize {
    let mut end = at;
    while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
        end += 1;
    }
    end
}

/// A long bracket's opening at `at` (`[`, `=`s, `[`): its level and where its text starts.
fn long_open(b: &[u8], at: usize) -> Option<(u8, usize)> {
    if b.get(at) != Some(&b'[') {
        return None;
    }
    let mut i = at + 1;
    while b.get(i) == Some(&b'=') {
        i += 1;
    }
    (b.get(i) == Some(&b'[')).then(|| ((i - at - 1).min(255) as u8, i + 1))
}

/// Where a long bracket of `level` closes (`]`, `=`s, `]`) from `from` on: past its end.
fn close_long(b: &[u8], from: usize, level: u8) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == b']' {
            let mut k = i + 1;
            while b.get(k) == Some(&b'=') {
                k += 1;
            }
            if k - i - 1 == usize::from(level) && b.get(k) == Some(&b']') {
                return Some(k + 1);
            }
        }
        i += 1;
    }
    None
}

/// A quoted string's end from `from` (after its opening `quote`): past its closing quote,
/// or the line's end; and whether it goes on to the next line (`\`, or `\z` and spaces,
/// last on the line).
fn quoted(b: &[u8], from: usize, quote: u8) -> (usize, bool) {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1) == Some(&b'z') => {
                i += 2;
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= b.len() {
                    return (b.len(), true);
                }
            }
            b'\\' if i + 1 >= b.len() => return (b.len(), true),
            b'\\' => i += 2,
            c if c == quote => return (i + 1, false),
            _ => i += 1,
        }
    }
    (b.len().min(i), false)
}

/// An interpolated string's text from `from`: past its closing backtick, or past an
/// expression's `{` (`true`), or the line's end.
fn interpolated_text(b: &[u8], from: usize) -> (usize, bool) {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => return (i + 1, false),
            b'{' => return (i + 1, true),
            _ => i += 1,
        }
    }
    (b.len(), false)
}

/// A number's end from `at`, as Luau reads one: digits, `.` and `_`, an exponent and its
/// sign, then letters and digits (`0x1F`, `0b101`; Slang's `1.5f`, `2u` too).
pub(crate) fn number(b: &[u8], at: usize) -> usize {
    let mut i = at;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'_') {
        i += 1;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
    }
    word_end(b, i)
}

/// The states lines start in, kept for a document: dropped from the first line changed
/// ([`Highlight::changed`]) and made again up to the lines asked for.
#[derive(Debug, Default)]
pub(crate) struct Highlight {
    /// The language whose rules cut the lines.
    language: Language,
    /// The state each of the first `states.len()` lines starts in.
    states: Vec<State>,
    /// Runs made while finding states, not kept.
    scratch: Vec<(usize, usize, Token)>,
}

impl Highlight {
    /// The states of a document in `language`.
    pub(crate) fn new(language: Language) -> Self {
        Self {
            language,
            ..Self::default()
        }
    }

    /// The language whose rules cut the lines.
    pub(crate) fn language(&self) -> Language {
        self.language
    }

    /// The lines from `line` on changed: the states after it are made again when needed
    /// (a line's own comes from the lines before it).
    pub(crate) fn changed(&mut self, line: usize) {
        self.states.truncate(line + 1);
    }

    /// The state line `index` starts in, those before it made from `lines` as needed.
    pub(crate) fn state_at<'a>(&mut self, index: usize, lines: impl Fn(usize) -> &'a str) -> State {
        if self.states.is_empty() {
            self.states.push(State::Code);
        }
        while self.states.len() <= index {
            let at = self.states.len() - 1;
            self.scratch.clear();
            let next = self
                .language
                .cut(lines(at), self.states[at], &mut self.scratch);
            self.states.push(next);
        }
        self.states[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(text: &str, state: State) -> (Vec<(&str, Token)>, State) {
        let mut out = Vec::new();
        let next = line(text, state, &mut out);
        (
            out.iter().map(|&(a, b, k)| (&text[a..b], k)).collect(),
            next,
        )
    }

    #[test]
    fn keywords_strings_numbers_and_comments() {
        let (r, next) = runs("local speed = 0x1F + 2.5e-3 -- fast", State::Code);
        assert_eq!(
            r,
            [
                ("local", Token::Keyword),
                ("0x1F", Token::Number),
                ("2.5e-3", Token::Number),
                ("-- fast", Token::Comment),
            ]
        );
        assert_eq!(next, State::Code);
        let (r, _) = runs(r#"print("a \"b\"", 'c', 1_000)"#, State::Code);
        assert_eq!(
            r,
            [
                ("print", Token::Function),
                (r#""a \"b\"""#, Token::String),
                ("'c'", Token::String),
                ("1_000", Token::Number),
            ]
        );
    }

    #[test]
    fn functions_members_types_and_the_languages_own_names() {
        let (r, _) = runs("function Door.on_tick(self, dt: number): ()", State::Code);
        assert_eq!(
            r,
            [
                ("function", Token::Keyword),
                ("Door", Token::Function),
                ("on_tick", Token::Function),
                ("self", Token::Builtin),
                ("number", Token::Type),
            ]
        );
        let (r, _) = runs("self.angle = math.min(a.end, b:open())", State::Code);
        assert_eq!(
            r,
            [
                ("self", Token::Builtin),
                ("math", Token::Builtin),
                ("min", Token::Function),
                ("open", Token::Function),
            ],
            "a field named like a keyword stays a name"
        );
        let (r, _) = runs("export type Point = { x: number }", State::Code);
        assert_eq!(
            r,
            [
                ("export", Token::Keyword),
                ("type", Token::Keyword),
                ("Point", Token::Type),
                ("number", Token::Type),
            ]
        );
        let (r, _) = runs("local type = typeof(x) :: string", State::Code);
        assert_eq!(
            r,
            [
                ("local", Token::Keyword),
                ("type", Token::Builtin),
                ("typeof", Token::Function),
                ("string", Token::Type),
            ],
            "`type` a variable's name, the library's function's"
        );
        let (r, _) = runs("export type function Pair(a)", State::Code);
        assert_eq!(
            r,
            [
                ("export", Token::Keyword),
                ("type", Token::Keyword),
                ("function", Token::Keyword),
                ("Pair", Token::Function),
            ]
        );
        let (r, _) = runs("@native local function f() continue end", State::Code);
        assert_eq!(r[0], ("@native", Token::Keyword));
        assert!(r.contains(&("continue", Token::Keyword)));
    }

    #[test]
    fn interpolated_strings_hold_code() {
        let (r, next) = runs("print(`{name} has {#items} {f(`{x}`)}!`)", State::Code);
        assert_eq!(
            r,
            [
                ("print", Token::Function),
                ("`{", Token::String),
                ("} has {", Token::String),
                ("} {", Token::String),
                ("f", Token::Function),
                ("`{", Token::String),
                ("}`", Token::String),
                ("}!`", Token::String),
            ]
        );
        assert_eq!(next, State::Code);
    }

    #[test]
    fn long_comments_and_strings_go_on_across_lines() {
        let (r, next) = runs("x = 1 --[==[ a long", State::Code);
        assert_eq!(r.last(), Some(&("--[==[ a long", Token::Comment)));
        assert_eq!(next, State::Comment(2));
        let (r, next) = runs("comment ]] still ]==] y = 2", next);
        assert_eq!(r[0], ("comment ]] still ]==]", Token::Comment));
        assert_eq!(r[1], ("2", Token::Number));
        assert_eq!(next, State::Code);
        let (_, next) = runs("local s = [[first", State::Code);
        assert_eq!(next, State::Long(0));
        let (r, next) = runs("last]] .. \"x\\", next);
        assert_eq!(r[0], ("last]]", Token::String));
        assert_eq!(
            next,
            State::Quoted(b'"'),
            "a string's line ended in a backslash"
        );
        let (_, next) = runs("local t = 'a\\z   ", State::Code);
        assert_eq!(next, State::Quoted(b'\''), "`\\z` skips the line break too");
        // `--[=x` is a line comment.
        let (_, next) = runs("--[=x", State::Code);
        assert_eq!(next, State::Code);
    }

    #[test]
    fn states_are_kept_and_made_again_from_a_change() {
        let lines = core::cell::RefCell::new(vec!["--[[", "inside", "]]", "local x"]);
        let mut h = Highlight::default();
        assert_eq!(h.state_at(1, |i| lines.borrow()[i]), State::Comment(0));
        assert_eq!(h.state_at(3, |i| lines.borrow()[i]), State::Code);
        lines.borrow_mut()[0] = "-- [[";
        h.changed(0);
        assert_eq!(h.state_at(1, |i| lines.borrow()[i]), State::Code);
        assert_eq!(h.state_at(2, |i| lines.borrow()[i]), State::Code);
    }

    /// What highlighting costs: `cargo test --release -p engine_editor --lib
    /// measure_highlighting -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    #[allow(clippy::print_stdout)]
    fn measure_highlighting() {
        use std::time::Instant;
        let code = [
            "function Door.on_tick(self, dt: number)",
            "    local near = self:distance_to(self.world.player) < 3 -- metres",
            "    if near and self.angle < Door.open_angle then",
            "        self.angle = math.min(self.angle + Door.speed * dt * 60, 90)",
            "        print(`{self.name} opens: {self.angle}`)",
            "    end",
            "end",
            "--[[ a block",
            "of comment ]]",
            "local names = { \"front\", 'back', [[long]] }",
        ];
        let lines: Vec<&str> = (0..100_000).map(|i| code[i % code.len()]).collect();
        let t = Instant::now();
        let mut h = Highlight::default();
        let last = h.state_at(lines.len() - 1, |i| lines[i]);
        let all = t.elapsed();
        assert_eq!(last, State::Code);
        let mut runs = Vec::new();
        let t = Instant::now();
        for frame in 0..100 {
            let first = frame * 50;
            let mut state = h.state_at(first, |i| lines[i]);
            for line in &lines[first..first + 60] {
                runs.clear();
                state = super::line(line, state, &mut runs);
            }
        }
        let frame = t.elapsed() / 100;
        println!(
            "100,000 lines' states from nothing: {all:?}; a frame's 60 lines cut into runs: \
             {frame:?}"
        );
    }
}
