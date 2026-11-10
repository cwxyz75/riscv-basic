//! The reader: text to tokens (rules in "Reader, evaluation and errors in
//! detail" in `LOGO.md`). Lists are read whole, into values.

use crate::error::LogoError;
use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    Num(f64),
    /// `"word`: the text after the quote, case kept.
    Quoted(String),
    /// `:name`, lower-cased.
    Var(String),
    /// A procedure or primitive name, lower-cased.
    Word(String),
    List(Vec<Value>),
    LParen,
    RParen,
    Plus,
    /// Binary minus.
    Minus,
    /// Unary minus in front of something that is not a number literal.
    Neg,
    Star,
    Slash,
    Eq,
    Lt,
    Gt,
}

/// Characters that end a name.
fn ends_name(c: char) -> bool {
    c.is_whitespace() || "[]()\":;+-*/=<>".contains(c)
}

/// Characters that end a quoted word.
fn ends_quoted(c: char) -> bool {
    c.is_whitespace() || "[]()".contains(c)
}

fn starts_number(chars: &[char], i: usize) -> bool {
    match chars.get(i) {
        Some(c) if c.is_ascii_digit() => true,
        Some('.') => chars.get(i + 1).is_some_and(char::is_ascii_digit),
        _ => false,
    }
}

/// Read a number literal starting at `i`; returns it and the index after it.
fn number(chars: &[char], mut i: usize) -> (f64, usize) {
    let start = i;
    while chars.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        while chars.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
    }
    if matches!(chars.get(i), Some('e' | 'E')) {
        let mut j = i + 1;
        if matches!(chars.get(j), Some('+' | '-')) {
            j += 1;
        }
        if chars.get(j).is_some_and(char::is_ascii_digit) {
            while chars.get(j).is_some_and(char::is_ascii_digit) {
                j += 1;
            }
            i = j;
        }
    }
    let text: String = chars[start..i].iter().collect();
    (text.parse().unwrap_or(0.0), i)
}

/// Does `text` (an item inside a list) read as a number?
fn list_number(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    let (negative, from) = if chars.first() == Some(&'-') {
        (true, 1)
    } else {
        (false, 0)
    };
    if !starts_number(&chars, from) {
        return None;
    }
    let (value, end) = number(&chars, from);
    (end == chars.len()).then_some(if negative { -value } else { value })
}

/// Read the inside of a list, `start` being just after its `[`. Returns the
/// items and the index just after the closing `]`.
fn list(chars: &[char], start: usize) -> Result<(Vec<Value>, usize), LogoError> {
    let mut items = Vec::new();
    let mut i = start;
    loop {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        match chars.get(i) {
            None => return Err(LogoError::MissingBracket),
            Some(']') => return Ok((items, i + 1)),
            Some('[') => {
                let (inner, next) = list(chars, i + 1)?;
                items.push(Value::List(inner));
                i = next;
            }
            Some(_) => {
                let begin = i;
                while chars
                    .get(i)
                    .is_some_and(|c| !c.is_whitespace() && *c != '[' && *c != ']')
                {
                    i += 1;
                }
                let text: String = chars[begin..i].iter().collect();
                items.push(match list_number(&text) {
                    Some(n) => Value::Num(n),
                    None => Value::Word(text),
                });
            }
        }
    }
}

/// Read one line into tokens.
pub(crate) fn lex(line: &str) -> Result<Vec<Token>, LogoError> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            ';' => break,
            '[' => {
                let (items, next) = list(&chars, i + 1)?;
                tokens.push(Token::List(items));
                i = next;
            }
            ']' => return Err(LogoError::UnexpectedBracket),
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '"' => {
                let start = i + 1;
                i = start;
                while i < chars.len() && !ends_quoted(chars[i]) {
                    i += 1;
                }
                tokens.push(Token::Quoted(chars[start..i].iter().collect()));
            }
            ':' => {
                let start = i + 1;
                i = start;
                while i < chars.len() && !ends_name(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                tokens.push(Token::Var(name.to_lowercase()));
            }
            '+' | '*' | '/' | '=' | '<' | '>' => {
                tokens.push(match c {
                    '+' => Token::Plus,
                    '*' => Token::Star,
                    '/' => Token::Slash,
                    '=' => Token::Eq,
                    '<' => Token::Lt,
                    _ => Token::Gt,
                });
                i += 1;
            }
            '-' => {
                let after_operator = matches!(
                    tokens.last(),
                    None | Some(
                        Token::Plus
                            | Token::Minus
                            | Token::Neg
                            | Token::Star
                            | Token::Slash
                            | Token::Eq
                            | Token::Lt
                            | Token::Gt
                            | Token::LParen
                    )
                );
                let space_before = i > 0 && chars[i - 1].is_whitespace();
                let attached = chars.get(i + 1).is_some_and(|c| !c.is_whitespace());
                if !(after_operator || (space_before && attached)) {
                    tokens.push(Token::Minus);
                    i += 1;
                } else if starts_number(&chars, i + 1) {
                    let (value, next) = number(&chars, i + 1);
                    tokens.push(Token::Num(-value));
                    i = next;
                } else {
                    tokens.push(Token::Neg);
                    i += 1;
                }
            }
            _ if starts_number(&chars, i) => {
                let (value, next) = number(&chars, i);
                tokens.push(Token::Num(value));
                i = next;
            }
            _ => {
                let start = i;
                while i < chars.len() && !ends_name(chars[i]) {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                tokens.push(Token::Word(name.to_lowercase()));
            }
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn w(s: &str) -> Token {
        Token::Word(s.into())
    }

    #[test]
    fn unary_and_binary_minus() {
        assert_eq!(
            lex("3 - 2").unwrap(),
            vec![Token::Num(3.0), Token::Minus, Token::Num(2.0)]
        );
        assert_eq!(
            lex("3-2").unwrap(),
            vec![Token::Num(3.0), Token::Minus, Token::Num(2.0)]
        );
        assert_eq!(lex("fd -50").unwrap(), vec![w("fd"), Token::Num(-50.0)]);
        assert_eq!(
            lex("3 -2").unwrap(),
            vec![Token::Num(3.0), Token::Num(-2.0)]
        );
        assert_eq!(
            lex("3 + -2").unwrap(),
            vec![Token::Num(3.0), Token::Plus, Token::Num(-2.0)]
        );
        assert_eq!(
            lex("-:x").unwrap(),
            vec![Token::Neg, Token::Var("x".into())]
        );
    }

    #[test]
    fn words_numbers_quotes_and_comments() {
        assert_eq!(
            lex("PRINT \"Hello :X 1e3 ; ignored").unwrap(),
            vec![
                w("print"),
                Token::Quoted("Hello".into()),
                Token::Var("x".into()),
                Token::Num(1000.0)
            ]
        );
        assert_eq!(lex("\"").unwrap(), vec![Token::Quoted(String::new())]);
        assert_eq!(lex("fd:x+1").unwrap().len(), 4);
    }

    #[test]
    fn lists_keep_their_text() {
        assert_eq!(
            lex("[fd -50 \"hi [a 1.50]]").unwrap(),
            vec![Token::List(vec![
                Value::word("fd"),
                Value::Num(-50.0),
                Value::word("\"hi"),
                Value::List(vec![Value::word("a"), Value::Num(1.5)]),
            ])]
        );
    }

    #[test]
    fn bracket_errors() {
        assert_eq!(lex("print [a b"), Err(LogoError::MissingBracket));
        assert_eq!(lex("print a]"), Err(LogoError::UnexpectedBracket));
    }
}
