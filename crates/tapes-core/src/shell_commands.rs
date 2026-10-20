//! A tolerant reading of the shell text a tool call carries, shared by the
//! observations that read commands: which programs a script runs, in what
//! order and joined by what.

use serde_json::Value;

use crate::event::{javascript_tokens, JsTokenKind};

/// The longest command text read.
pub(crate) const MAX_COMMAND_CHARS: usize = 256 * 1024;
const MAX_COMMANDS: usize = 512;
const MAX_SHELL_DEPTH: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sep {
    Start,
    And,
    Or,
    Seq,
    Pipe,
}

#[derive(Clone, Debug)]
pub(crate) struct Command {
    pub(crate) after: Sep,
    pub(crate) words: Vec<String>,
    /// The files the command redirects its output into.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) redirects: Vec<String>,
}

pub(crate) const SHELL_TOOLS: [&str; 8] = [
    "bash",
    "shell",
    "exec",
    "exec_command",
    "shell_command",
    "commandexecution",
    "container.exec",
    "local_shell",
];

const SHELLS: [&str; 5] = ["bash", "sh", "zsh", "dash", "ksh"];

pub(crate) fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// The subcommand and arguments of a `git` command, past its global options.
pub(crate) fn git_subcommand(words: &[String]) -> Option<(&str, Vec<&str>)> {
    let (program, rest) = words.split_first()?;
    if basename(program) != "git" {
        return None;
    }
    let mut rest = rest.iter().map(String::as_str);
    while let Some(word) = rest.next() {
        match word {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
            | "--super-prefix" | "--config-env" => {
                rest.next();
            }
            word if word.starts_with('-') => {}
            sub => return Some((sub, rest.collect())),
        }
    }
    None
}

// ----------------------------------------------------------- script text

pub(crate) fn is_shell_tool(name: Option<&str>) -> bool {
    name.is_some_and(|name| SHELL_TOOLS.contains(&name.to_ascii_lowercase().as_str()))
}

/// Every command a shell tool call's recorded arguments run, in order, bounded.
pub(crate) fn commands(arguments: &str) -> Vec<Command> {
    let mut commands = Vec::new();
    if arguments.chars().count() > MAX_COMMAND_CHARS {
        return commands;
    }
    for script in scripts(arguments) {
        scan_shell(&script, 0, Sep::Seq, &mut commands);
        if commands.len() >= MAX_COMMANDS {
            break;
        }
    }
    commands
}

/// The shell scripts a shell tool call's recorded arguments hold.
fn scripts(arguments: &str) -> Vec<String> {
    if arguments.contains("tools.exec_command") {
        return javascript_commands(arguments);
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value_scripts(&value),
        Err(_) => Vec::new(),
    }
}

fn value_scripts(value: &Value) -> Vec<String> {
    match value {
        Value::String(script) => vec![script.clone()],
        Value::Array(argv) => argv_script(argv).into_iter().collect(),
        Value::Object(object) => ["command", "cmd", "argv"]
            .into_iter()
            .find_map(|key| object.get(key))
            .map(value_scripts)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// An argv as one script: a shell's `-c` operand, or the words themselves.
fn argv_script(argv: &[Value]) -> Option<String> {
    let words = argv.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
    if let Some(script) = shell_operand(&words) {
        return Some(script.to_owned());
    }
    Some(
        words
            .iter()
            .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The script a shell invoked with `-c` (or `-lc` and the like) runs.
pub(crate) fn shell_operand<'a>(words: &[&'a str]) -> Option<&'a str> {
    let (program, rest) = words.split_first()?;
    if !SHELLS.contains(&basename(program)) {
        return None;
    }
    let flag = rest.iter().position(|word| {
        word.starts_with('-') && !word.starts_with("--") && word[1..].contains('c')
    })?;
    rest.get(flag + 1).copied()
}

/// Each literal `cmd`/`command` string of a Codex script's `exec_command` calls.
fn javascript_commands(text: &str) -> Vec<String> {
    let Ok(tokens) = javascript_tokens(text) else {
        return Vec::new();
    };
    tokens
        .windows(3)
        .filter_map(|window| {
            let key = match &window[0].kind {
                JsTokenKind::Identifier(key) | JsTokenKind::String(key) => key.as_str(),
                JsTokenKind::Punctuation(_) => return None,
            };
            match (&window[1].kind, &window[2].kind) {
                (JsTokenKind::Punctuation(':'), JsTokenKind::String(script))
                    if matches!(key, "cmd" | "command") =>
                {
                    Some(script.clone())
                }
                _ => None,
            }
        })
        .collect()
}

/// A tolerant split of shell text into commands. It follows quoting, comments,
/// heredoc bodies, redirections and `sh -c` operands, and treats command
/// substitutions as opaque words. It never evaluates anything, so a command it
/// cannot read exactly is still split at the separators it can see.
fn scan_shell(text: &str, depth: usize, first: Sep, out: &mut Vec<Command>) {
    let mut scanner = Scanner {
        chars: text.chars().collect(),
        at: 0,
        words: Vec::new(),
        word: String::new(),
        in_word: false,
        sep: first,
        heredocs: Vec::new(),
        depth,
        redirects: Vec::new(),
        out,
    };
    scanner.run();
}

struct Scanner<'a> {
    chars: Vec<char>,
    at: usize,
    words: Vec<String>,
    word: String,
    in_word: bool,
    sep: Sep,
    /// Heredoc delimiters whose bodies start at the next line, with whether
    /// leading tabs are stripped from the closing line.
    heredocs: Vec<(String, bool)>,
    depth: usize,
    /// Files the command being read redirects into.
    redirects: Vec<String>,
    out: &'a mut Vec<Command>,
}

impl Scanner<'_> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn run(&mut self) {
        while self.at < self.chars.len() && self.out.len() < MAX_COMMANDS {
            let c = self.chars[self.at];
            match c {
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(next) => {
                            self.word.push(next);
                            self.in_word = true;
                        }
                        None => {}
                    }
                    self.at += 2;
                }
                '\'' => self.single_quoted(),
                '"' => self.double_quoted(),
                '`' => self.opaque('`', '`'),
                '$' if self.peek(1) == Some('(') => {
                    self.at += 1;
                    self.opaque('(', ')');
                }
                '$' if self.peek(1) == Some('{') => {
                    self.at += 1;
                    self.opaque('{', '}');
                }
                '#' if !self.in_word => {
                    while self.at < self.chars.len() && self.chars[self.at] != '\n' {
                        self.at += 1;
                    }
                }
                '<' | '>' => self.redirection(),
                '&' => match self.peek(1) {
                    Some('&') => self.end(Sep::And, 2),
                    Some('>') => self.at += 1,
                    _ => self.end(Sep::Seq, 1),
                },
                '|' => match self.peek(1) {
                    Some('|') => self.end(Sep::Or, 2),
                    Some('&') => self.end(Sep::Pipe, 2),
                    _ => self.end(Sep::Pipe, 1),
                },
                ';' | '(' | ')' => self.end(Sep::Seq, 1),
                '{' | '}' if !self.in_word => self.end(Sep::Seq, 1),
                '\n' => {
                    self.end(Sep::Seq, 1);
                    self.heredoc_bodies();
                }
                c if c.is_whitespace() => {
                    self.flush_word();
                    self.at += 1;
                }
                c => {
                    self.word.push(c);
                    self.in_word = true;
                    self.at += 1;
                }
            }
        }
        self.end(Sep::Seq, 0);
    }

    fn flush_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    /// Finish the command being read; the next one follows `next`.
    fn end(&mut self, next: Sep, width: usize) {
        self.at += width;
        self.flush_word();
        if self.words.is_empty() {
            if self.sep == Sep::Start || next != Sep::Seq {
                self.sep = next;
            }
            return;
        }
        let words = std::mem::take(&mut self.words);
        self.push_command(words);
        self.sep = next;
    }

    fn push_command(&mut self, mut words: Vec<String>) {
        const PREFIXES: [&str; 12] = [
            "if", "then", "else", "elif", "do", "while", "until", "!", "time", "command", "exec",
            "nohup",
        ];
        while let Some(first) = words.first() {
            let assignment = first.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
                    && !name.starts_with(|c: char| c.is_ascii_digit())
            });
            if assignment || PREFIXES.contains(&first.as_str()) || first == "env" {
                words.remove(0);
            } else if first == "timeout" && words.len() > 2 {
                words.drain(..2);
            } else {
                break;
            }
        }
        if words.is_empty() {
            return;
        }
        let refs = words.iter().map(String::as_str).collect::<Vec<_>>();
        let operand = shell_operand(&refs).or(match refs.as_slice() {
            ["eval", script] => Some(script),
            _ => None,
        });
        if let Some(script) = operand {
            if self.depth < MAX_SHELL_DEPTH {
                let script = script.to_owned();
                let mut inner = Vec::new();
                scan_shell(&script, self.depth + 1, self.sep, &mut inner);
                self.out.extend(inner);
                return;
            }
        }
        self.out.push(Command {
            after: self.sep,
            words,
            redirects: std::mem::take(&mut self.redirects),
        });
    }

    fn single_quoted(&mut self) {
        self.in_word = true;
        self.at += 1;
        while let Some(&c) = self.chars.get(self.at) {
            self.at += 1;
            if c == '\'' {
                return;
            }
            self.word.push(c);
        }
    }

    fn double_quoted(&mut self) {
        self.in_word = true;
        self.at += 1;
        while let Some(&c) = self.chars.get(self.at) {
            match c {
                '"' => {
                    self.at += 1;
                    return;
                }
                '\\' => {
                    if let Some(&next) = self.chars.get(self.at + 1) {
                        if next != '\n' {
                            self.word.push(next);
                        }
                    }
                    self.at += 2;
                }
                '$' if self.peek(1) == Some('(') => {
                    self.at += 1;
                    self.opaque('(', ')');
                }
                c => {
                    self.word.push(c);
                    self.at += 1;
                }
            }
        }
    }

    /// Skip a balanced `open … close` group (a substitution) starting at its
    /// opening character, leaving a placeholder that is no sha, path or
    /// subcommand. A group whose two ends are one character (a backtick) closes
    /// at the second.
    fn opaque(&mut self, open: char, close: char) {
        self.in_word = true;
        self.word.push('\u{1}');
        let mut depth = 0usize;
        while let Some(&c) = self.chars.get(self.at) {
            self.at += 1;
            if c == '\\' {
                self.at += 1;
            } else if open == close {
                depth += 1;
                if depth == 2 {
                    return;
                }
            } else if c == open {
                depth += 1;
            } else if c == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return;
                }
            }
        }
    }

    fn redirection(&mut self) {
        if self.in_word && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.in_word = false;
        }
        let c = self.chars[self.at];
        if c == '<' && self.peek(1) == Some('<') && self.peek(2) != Some('<') {
            self.at += 2;
            let strip_tabs = self.peek(0) == Some('-');
            if strip_tabs {
                self.at += 1;
            }
            let tag = self.operand();
            if !tag.is_empty() {
                self.heredocs.push((tag, strip_tabs));
            }
            return;
        }
        let mut operator = String::new();
        while let Some(c @ ('<' | '>' | '&' | '|')) = self.peek(0) {
            operator.push(c);
            self.at += 1;
        }
        let target = self.operand();
        // `2>&1` duplicates a descriptor and `> /dev/null` discards; any other
        // output redirection writes a file.
        let duplicates =
            operator.contains('&') && target.chars().all(|c| c.is_ascii_digit() || c == '-');
        if operator.contains('>') && !duplicates && !target.is_empty() && target != "/dev/null" {
            self.redirects.push(target);
        }
    }

    /// Read one word after an operator, without adding it to the command.
    fn operand(&mut self) -> String {
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
            self.at += 1;
        }
        let mut word = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                '\'' | '"' => {
                    self.at += 1;
                    while let Some(inner) = self.peek(0) {
                        self.at += 1;
                        if inner == c {
                            break;
                        }
                        word.push(inner);
                    }
                }
                '\\' => {
                    self.at += 1;
                }
                c if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>') => {
                    break
                }
                c => {
                    word.push(c);
                    self.at += 1;
                }
            }
        }
        word
    }

    /// Skip the bodies of the heredocs opened on the line just ended.
    fn heredoc_bodies(&mut self) {
        for (tag, strip_tabs) in std::mem::take(&mut self.heredocs) {
            loop {
                if self.at >= self.chars.len() {
                    return;
                }
                let end = self.chars[self.at..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(self.chars.len(), |offset| self.at + offset);
                let line = self.chars[self.at..end].iter().collect::<String>();
                self.at = (end + 1).min(self.chars.len());
                let line = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == tag {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redirects(command: &str) -> Vec<Vec<String>> {
        let arguments = serde_json::json!({ "command": command }).to_string();
        commands(&arguments)
            .into_iter()
            .map(|command| command.redirects)
            .collect()
    }

    #[test]
    fn output_redirection_names_the_files_it_writes() {
        assert_eq!(
            redirects("cat > a.rs <<'E'\nbody\nE\necho x >> log.md 2>&1; ls > /dev/null"),
            [vec!["a.rs".to_owned()], vec!["log.md".to_owned()], vec![]]
        );
        assert_eq!(
            redirects("make 2>&1 | tail -3; echo done 1>&2; sort < in > out.txt"),
            [vec![], vec![], vec![], vec!["out.txt".to_owned()]]
        );
    }
}
