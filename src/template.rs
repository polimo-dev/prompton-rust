//! The prompt template engine: the Liquid subset PromptOn allows.
//!
//! Tags: `for` (with `else`, `break`, `continue` and `forloop.*`), `if`/`elsif`/`else`, `unless`,
//! `assign`. Filters: `size`, `join`, `default`. Everything else — `include`, `capture`, `case`,
//! `raw`, `comment`, `cycle`, `render`, `tablerow`, `increment`, `liquid` — is a parse error, and
//! nothing is HTML-escaped.
//!
//! A variable is *missing* when its key is absent from the variables map; a key present with a
//! `null` value is not missing (it renders as the empty string and `default` replaces it). A
//! missing variable is an error at output positions (`{{ … }}`), in a `for` enumerable, in an
//! `unless` condition and as an `assign` source. It is **not** an error inside an `if`/`elsif`
//! condition, and a branch that does not execute is never looked at.
//!
//! ```
//! use prompton::template::render;
//! use prompton::{Engine, Vars};
//!
//! let out = render("Hello {{ name }}!", &Vars::from(serde_json::json!({"name": "Ada"})), Engine::Liquid)?;
//! assert_eq!(out, "Hello Ada!");
//! # Ok::<(), prompton::TemplateError>(())
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which engine a prompt version was committed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// The Liquid subset described in this module.
    #[default]
    Liquid,
    /// No parsing at all: the source is returned verbatim. For prompts whose text contains
    /// `{{` or `{%`.
    Raw,
}

impl Engine {
    /// Parses the snapshot's `engine` string; anything unknown falls back to Liquid.
    pub fn from_wire(value: Option<&str>) -> Engine {
        match value {
            Some("raw") => Engine::Raw,
            _ => Engine::Liquid,
        }
    }

    /// The wire value.
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Liquid => "liquid",
            Engine::Raw => "raw",
        }
    }
}

/// Why a render failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// A variable the template reads is absent from the variables map.
    #[error("missing variable: {0}")]
    MissingVariable(String),
    /// The template uses a construct outside the allowed set, or is malformed.
    #[error("template parse error: {0}")]
    Parse(String),
    /// The template parsed but rendering failed for another reason.
    #[error("template render error: {0}")]
    Render(String),
}

impl TemplateError {
    /// The conformance-suite category of this error: `missing_variable`, `parse_error` or
    /// `render_error`.
    pub fn category(&self) -> &'static str {
        match self {
            TemplateError::MissingVariable(_) => "missing_variable",
            TemplateError::Parse(_) => "parse_error",
            TemplateError::Render(_) => "render_error",
        }
    }
}

/// A set of template variables: the JSON object a prompt is rendered with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vars(Map<String, Value>);

impl Vars {
    /// An empty set.
    pub fn new() -> Vars {
        Vars(Map::new())
    }

    /// Adds one variable, builder style.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Vars {
        self.0.insert(key.into(), value.into());
        self
    }

    /// Adds one variable.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.0.insert(key.into(), value.into());
    }

    /// Whether a key is present (a `null` value counts as present).
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// The underlying JSON object.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// The variables as a JSON value (always an object).
    pub fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }

    /// Whether no variable is set.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Map<String, Value>> for Vars {
    fn from(map: Map<String, Value>) -> Vars {
        Vars(map)
    }
}

impl From<Value> for Vars {
    /// A JSON object becomes the variable set; anything else becomes the empty set.
    fn from(value: Value) -> Vars {
        match value {
            Value::Object(map) => Vars(map),
            _ => Vars::new(),
        }
    }
}

impl From<&Value> for Vars {
    fn from(value: &Value) -> Vars {
        Vars::from(value.clone())
    }
}

impl From<()> for Vars {
    fn from(_: ()) -> Vars {
        Vars::new()
    }
}

/// Renders `source` with `vars`.
pub fn render(source: &str, vars: &Vars, engine: Engine) -> Result<String, TemplateError> {
    if engine == Engine::Raw {
        return Ok(source.to_string());
    }
    let nodes = parse(source)?;
    let mut renderer = Renderer::new(vars);
    let mut out = String::with_capacity(source.len());
    match renderer.run(&nodes, &mut out)? {
        Flow::Normal => Ok(out),
        // `break`/`continue` outside a loop are ignored, like Liquid.
        _ => Ok(out),
    }
}

/// Renders the `content` of each message, leaving `role` and `name` untouched.
pub fn render_messages(
    messages: &[crate::snapshot::Message],
    vars: &Vars,
    engine: Engine,
) -> Result<Vec<crate::snapshot::Message>, TemplateError> {
    let mut out = Vec::new();
    for message in messages {
        if message.message_type.as_deref() == Some("slot") {
            return Err(TemplateError::Render(
                "Message slots are not supported; compose conversation history in app code."
                    .to_string(),
            ));
        } else if let Some(Value::String(content)) = &message.content_value {
            let mut rendered = message.clone();
            rendered.content = render(content, vars, engine)?;
            rendered.content_value = Some(Value::String(rendered.content.clone()));
            out.push(rendered);
        } else if !message.content_present && !message.content.is_empty() {
            let mut rendered = message.clone();
            rendered.content = render(&message.content, vars, engine)?;
            rendered.content_value = Some(Value::String(rendered.content.clone()));
            rendered.content_present = true;
            out.push(rendered);
        } else {
            out.push(message.clone());
        }
    }
    Ok(out)
}

/// Why a template failed the static whitelist check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintReason {
    /// Whitespace control (`{%-`, `-%}`, `{{-`, `-}}`) is not allowed.
    WhitespaceControl(String),
    /// A tag outside the allowed set.
    DisallowedTag(String),
    /// A filter outside `size`, `join`, `default`.
    DisallowedFilter(String),
    /// The template does not parse at all.
    Parse(String),
}

impl LintReason {
    /// The reason kind as the conformance suite spells it.
    pub fn kind(&self) -> &'static str {
        match self {
            LintReason::WhitespaceControl(_) => "whitespace_control",
            LintReason::DisallowedTag(_) => "disallowed_tag",
            LintReason::DisallowedFilter(_) => "disallowed_filter",
            LintReason::Parse(_) => "parse",
        }
    }

    /// The offending value (a marker, a tag name, a filter name, or the parser message).
    pub fn value(&self) -> &str {
        match self {
            LintReason::WhitespaceControl(v)
            | LintReason::DisallowedTag(v)
            | LintReason::DisallowedFilter(v)
            | LintReason::Parse(v) => v,
        }
    }
}

impl fmt::Display for LintReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind(), self.value())
    }
}

const ALLOWED_FILTERS: [&str; 3] = ["size", "join", "default"];
const ALLOWED_TAGS: [&str; 11] = [
    "for",
    "endfor",
    "if",
    "endif",
    "elsif",
    "else",
    "unless",
    "endunless",
    "assign",
    "break",
    "continue",
];

/// The static whitelist check the server applies when a prompt version is committed.
///
/// Rendering does not enforce the filter whitelist — a template that fails lint can never reach a
/// snapshot — so this is the check to run before committing a prompt.
pub fn lint(source: &str) -> Result<(), Vec<LintReason>> {
    let mut markers: Vec<(usize, LintReason)> = Vec::new();
    for marker in ["{{-", "{%-", "-}}", "-%}"] {
        if let Some(at) = source.find(marker) {
            markers.push((at, LintReason::WhitespaceControl(marker.to_string())));
        }
    }
    markers.sort_by_key(|(at, _)| *at);
    let mut reasons: Vec<LintReason> = markers.into_iter().map(|(_, reason)| reason).collect();

    let tokens = match lex(source) {
        Ok(tokens) => tokens,
        Err(err) => {
            reasons.push(LintReason::Parse(message_of(&err)));
            return Err(reasons);
        }
    };

    let mut bad_tags = Vec::new();
    for token in &tokens {
        if let Token::Tag { inner, .. } = token {
            let name = inner.split_whitespace().next().unwrap_or("");
            if !name.is_empty() && !ALLOWED_TAGS.contains(&name) {
                let name = name.to_string();
                if !bad_tags.contains(&name) {
                    bad_tags.push(name);
                }
            }
        }
    }
    if !bad_tags.is_empty() {
        reasons.extend(bad_tags.into_iter().map(LintReason::DisallowedTag));
        return Err(reasons);
    }

    match parse(source) {
        Ok(nodes) => {
            let mut filters = Vec::new();
            collect_filters(&nodes, &mut filters);
            for name in filters {
                if !ALLOWED_FILTERS.contains(&name.as_str()) {
                    let reason = LintReason::DisallowedFilter(name);
                    if !reasons.contains(&reason) {
                        reasons.push(reason);
                    }
                }
            }
        }
        Err(err) => reasons.push(LintReason::Parse(message_of(&err))),
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

fn message_of(err: &TemplateError) -> String {
    match err {
        TemplateError::Parse(message) => message.clone(),
        other => other.to_string(),
    }
}

/// The top-level input variables a template reads, sorted and deduplicated.
///
/// `for` loop variables, `assign` targets and `forloop` are excluded, which makes this the list an
/// app should be able to supply (the server records the same list as `detected_variables`).
pub fn variables(source: &str) -> Vec<String> {
    let nodes = match parse(source) {
        Ok(nodes) => nodes,
        Err(_) => return Vec::new(),
    };
    let mut referenced = Vec::new();
    let mut bound = vec!["forloop".to_string()];
    collect_variables(&nodes, &mut referenced, &mut bound);
    let mut names: Vec<String> = referenced
        .into_iter()
        .filter(|name| !bound.contains(name))
        .collect();
    names.sort();
    names.dedup();
    names
}

// ---------------------------------------------------------------------------
// lexing

#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Output { inner: String },
    Tag { inner: String },
}

fn lex(source: &str) -> Result<Vec<Token>, TemplateError> {
    let bytes = source.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut text = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() && (bytes[i + 1] == b'{' || bytes[i + 1] == b'%')
        {
            let is_output = bytes[i + 1] == b'{';
            let close = if is_output { "}}" } else { "%}" };
            let start = i + 2;
            let end = match source[start..].find(close) {
                Some(at) => start + at,
                None => {
                    return Err(TemplateError::Parse(format!(
                        "Expected '{close}' to close the {} opened at byte {i}",
                        if is_output { "output" } else { "tag" }
                    )))
                }
            };
            let mut inner = &source[start..end];
            let mut trim_left = false;
            let mut trim_right = false;
            if let Some(rest) = inner.strip_prefix('-') {
                trim_left = true;
                inner = rest;
            }
            if let Some(rest) = inner.strip_suffix('-') {
                trim_right = true;
                inner = rest;
            }

            if trim_left {
                let trimmed = text.trim_end().to_string();
                text = trimmed;
            }
            if !text.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut text)));
            }
            let inner = inner.trim().to_string();
            tokens.push(if is_output {
                Token::Output { inner }
            } else {
                Token::Tag { inner }
            });

            i = end + close.len();
            if trim_right {
                while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                    i += 1;
                }
            }
            continue;
        }

        let ch_len = utf8_len(bytes[i]);
        text.push_str(&source[i..i + ch_len]);
        i += ch_len;
    }

    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
    Ok(tokens)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// AST

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Output(Expr),
    If {
        branches: Vec<(Cond, Vec<Node>)>,
        otherwise: Option<Vec<Node>>,
    },
    Unless {
        cond: Cond,
        body: Vec<Node>,
        otherwise: Option<Vec<Node>>,
    },
    For {
        var: String,
        source: Expr,
        body: Vec<Node>,
        otherwise: Option<Vec<Node>>,
    },
    Assign {
        target: String,
        value: Expr,
    },
    Break,
    Continue,
}

#[derive(Debug, Clone)]
struct Expr {
    term: Term,
    filters: Vec<FilterCall>,
}

#[derive(Debug, Clone)]
enum Term {
    Literal(Value),
    Path(Path),
}

#[derive(Debug, Clone)]
struct Path {
    root: String,
    segments: Vec<Segment>,
    display: String,
}

#[derive(Debug, Clone)]
enum Segment {
    Field(String),
    Index(usize),
}

#[derive(Debug, Clone)]
struct FilterCall {
    name: String,
    args: Vec<Term>,
}

#[derive(Debug, Clone)]
enum Cond {
    Or(Box<Cond>, Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Compare(Expr, CompareOp, Expr),
    Truthy(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CompareOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Contains,
}

// ---------------------------------------------------------------------------
// parsing

fn parse(source: &str) -> Result<Vec<Node>, TemplateError> {
    let tokens = lex(source)?;
    let mut cursor = 0usize;
    let (nodes, terminator) = parse_block(&tokens, &mut cursor, &[])?;
    if let Some(name) = terminator {
        return Err(TemplateError::Parse(format!("Unexpected tag '{name}'")));
    }
    Ok(nodes)
}

/// Parses until one of `stop` (a closing tag name) or the end of input. Returns the nodes and the
/// tag name that stopped it.
fn parse_block(
    tokens: &[Token],
    cursor: &mut usize,
    stop: &[&str],
) -> Result<(Vec<Node>, Option<String>), TemplateError> {
    let mut nodes = Vec::new();

    while *cursor < tokens.len() {
        match &tokens[*cursor] {
            Token::Text(text) => {
                nodes.push(Node::Text(text.clone()));
                *cursor += 1;
            }
            Token::Output { inner } => {
                nodes.push(Node::Output(parse_expr(inner)?));
                *cursor += 1;
            }
            Token::Tag { inner } => {
                let (name, rest) = split_tag(inner);
                if stop.contains(&name.as_str()) {
                    *cursor += 1;
                    return Ok((nodes, Some(name)));
                }
                *cursor += 1;
                match name.as_str() {
                    "if" => nodes.push(parse_if(tokens, cursor, &rest)?),
                    "unless" => nodes.push(parse_unless(tokens, cursor, &rest)?),
                    "for" => nodes.push(parse_for(tokens, cursor, &rest)?),
                    "assign" => nodes.push(parse_assign(&rest)?),
                    "break" => nodes.push(Node::Break),
                    "continue" => nodes.push(Node::Continue),
                    other => {
                        return Err(TemplateError::Parse(format!("Unexpected tag '{other}'")));
                    }
                }
            }
        }
    }

    Ok((nodes, None))
}

/// Liquid's "blank block" rule, as the reference implementation applies it: when a tag body
/// consists only of whitespace text (and tags that produce nothing, such as `assign`), the text is
/// dropped, so `{% unless forloop.last %} {% endunless %}` renders nothing at all.
fn remove_blank_text_if_blank_body(nodes: Vec<Node>) -> Vec<Node> {
    if nodes.iter().all(is_blank) {
        nodes
            .into_iter()
            .filter(|node| !matches!(node, Node::Text(_)))
            .collect()
    } else {
        nodes
    }
}

fn is_blank(node: &Node) -> bool {
    match node {
        Node::Text(text) => text.trim().is_empty(),
        Node::Assign { .. } => true,
        _ => false,
    }
}

fn split_tag(inner: &str) -> (String, String) {
    let trimmed = inner.trim();
    match trimmed.find(char::is_whitespace) {
        Some(at) => (trimmed[..at].to_string(), trimmed[at..].trim().to_string()),
        None => (trimmed.to_string(), String::new()),
    }
}

fn parse_if(tokens: &[Token], cursor: &mut usize, rest: &str) -> Result<Node, TemplateError> {
    let mut branches = Vec::new();
    let mut otherwise = None;
    let mut condition = parse_cond(rest)?;

    loop {
        let (body, terminator) = parse_block(tokens, cursor, &["elsif", "else", "endif"])?;
        match terminator.as_deref() {
            Some("elsif") => {
                branches.push((condition, remove_blank_text_if_blank_body(body)));
                let inner = previous_tag_rest(tokens, *cursor);
                condition = parse_cond(&inner)?;
            }
            Some("else") => {
                branches.push((condition, remove_blank_text_if_blank_body(body)));
                let (else_body, terminator) = parse_block(tokens, cursor, &["endif"])?;
                if terminator.is_none() {
                    return Err(TemplateError::Parse("Expected 'endif'".to_string()));
                }
                otherwise = Some(remove_blank_text_if_blank_body(else_body));
                break;
            }
            Some("endif") => {
                branches.push((condition, remove_blank_text_if_blank_body(body)));
                break;
            }
            _ => return Err(TemplateError::Parse("Expected 'endif'".to_string())),
        }
    }

    Ok(Node::If {
        branches,
        otherwise,
    })
}

fn parse_unless(tokens: &[Token], cursor: &mut usize, rest: &str) -> Result<Node, TemplateError> {
    let cond = parse_cond(rest)?;
    let (body, terminator) = parse_block(tokens, cursor, &["else", "endunless"])?;
    match terminator.as_deref() {
        Some("endunless") => Ok(Node::Unless {
            cond,
            body: remove_blank_text_if_blank_body(body),
            otherwise: None,
        }),
        Some("else") => {
            let (else_body, terminator) = parse_block(tokens, cursor, &["endunless"])?;
            if terminator.is_none() {
                return Err(TemplateError::Parse("Expected 'endunless'".to_string()));
            }
            Ok(Node::Unless {
                cond,
                body: remove_blank_text_if_blank_body(body),
                otherwise: Some(remove_blank_text_if_blank_body(else_body)),
            })
        }
        _ => Err(TemplateError::Parse("Expected 'endunless'".to_string())),
    }
}

fn parse_for(tokens: &[Token], cursor: &mut usize, rest: &str) -> Result<Node, TemplateError> {
    let (var, remainder) = split_tag(rest);
    let (keyword, source) = split_tag(&remainder);
    if var.is_empty() || keyword != "in" || source.is_empty() {
        return Err(TemplateError::Parse(format!(
            "Expected 'for <var> in <collection>', got 'for {rest}'"
        )));
    }
    let source = parse_expr(&source)?;

    let (body, terminator) = parse_block(tokens, cursor, &["else", "endfor"])?;
    match terminator.as_deref() {
        Some("endfor") => Ok(Node::For {
            var,
            source,
            body: remove_blank_text_if_blank_body(body),
            otherwise: None,
        }),
        Some("else") => {
            let (else_body, terminator) = parse_block(tokens, cursor, &["endfor"])?;
            if terminator.is_none() {
                return Err(TemplateError::Parse("Expected 'endfor'".to_string()));
            }
            Ok(Node::For {
                var,
                source,
                body: remove_blank_text_if_blank_body(body),
                otherwise: Some(remove_blank_text_if_blank_body(else_body)),
            })
        }
        _ => Err(TemplateError::Parse("Expected 'endfor'".to_string())),
    }
}

fn parse_assign(rest: &str) -> Result<Node, TemplateError> {
    let (target, value) = match rest.split_once('=') {
        Some((target, value)) => (target.trim(), value.trim()),
        None => {
            return Err(TemplateError::Parse(format!(
                "Expected 'assign <name> = <expression>', got 'assign {rest}'"
            )))
        }
    };
    if target.is_empty() || value.is_empty() {
        return Err(TemplateError::Parse(format!(
            "Expected 'assign <name> = <expression>', got 'assign {rest}'"
        )));
    }
    Ok(Node::Assign {
        target: target.to_string(),
        value: parse_expr(value)?,
    })
}

/// The `elsif` condition text: `parse_block` has already consumed the tag, so look back one.
fn previous_tag_rest(tokens: &[Token], cursor: usize) -> String {
    match tokens.get(cursor - 1) {
        Some(Token::Tag { inner }) => split_tag(inner).1,
        _ => String::new(),
    }
}

fn parse_expr(source: &str) -> Result<Expr, TemplateError> {
    let parts = split_top_level(source, '|');
    let mut iter = parts.into_iter();
    let head = iter.next().unwrap_or_default();
    let term = parse_term(head.trim())?;
    let mut filters = Vec::new();
    for part in iter {
        let part = part.trim();
        if part.is_empty() {
            return Err(TemplateError::Parse(format!(
                "Expected a filter name in '{source}'"
            )));
        }
        let (name, args) = match part.split_once(':') {
            Some((name, args)) => (
                name.trim().to_string(),
                split_top_level(args, ',')
                    .into_iter()
                    .map(|arg| parse_term(arg.trim()))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            None => (part.to_string(), Vec::new()),
        };
        filters.push(FilterCall { name, args });
    }
    Ok(Expr { term, filters })
}

/// Splits on `sep`, ignoring separators inside quoted strings or brackets.
fn split_top_level(source: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;

    for ch in source.chars() {
        match quote {
            Some(q) => {
                current.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '"' | '\'' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                '[' => {
                    depth += 1;
                    current.push(ch);
                }
                ']' => {
                    depth = depth.saturating_sub(1);
                    current.push(ch);
                }
                c if c == sep && depth == 0 => {
                    parts.push(std::mem::take(&mut current));
                }
                _ => current.push(ch),
            },
        }
    }
    parts.push(current);
    parts
}

fn parse_term(source: &str) -> Result<Term, TemplateError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(TemplateError::Parse("Expected an expression".to_string()));
    }
    if (source.starts_with('"') && source.ends_with('"') && source.len() >= 2)
        || (source.starts_with('\'') && source.ends_with('\'') && source.len() >= 2)
    {
        return Ok(Term::Literal(Value::String(
            source[1..source.len() - 1].to_string(),
        )));
    }
    match source {
        "true" => return Ok(Term::Literal(Value::Bool(true))),
        "false" => return Ok(Term::Literal(Value::Bool(false))),
        "nil" | "null" | "empty" | "blank" => return Ok(Term::Literal(Value::Null)),
        _ => {}
    }
    if let Ok(int) = source.parse::<i64>() {
        return Ok(Term::Literal(Value::Number(int.into())));
    }
    if let Ok(float) = source.parse::<f64>() {
        if let Some(number) = serde_json::Number::from_f64(float) {
            return Ok(Term::Literal(Value::Number(number)));
        }
    }
    Ok(Term::Path(parse_path(source)?))
}

fn parse_path(source: &str) -> Result<Path, TemplateError> {
    let mut root = String::new();
    let mut segments = Vec::new();
    let mut chars = source.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch == '.' || ch == '[' {
            break;
        }
        root.push(ch);
        chars.next();
    }
    if root.is_empty() {
        return Err(TemplateError::Parse(format!(
            "Expected a variable name in '{source}'"
        )));
    }

    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                let mut field = String::new();
                while let Some(&next) = chars.peek() {
                    if next == '.' || next == '[' {
                        break;
                    }
                    field.push(next);
                    chars.next();
                }
                if field.is_empty() {
                    return Err(TemplateError::Parse(format!(
                        "Expected a field name in '{source}'"
                    )));
                }
                segments.push(Segment::Field(field));
            }
            '[' => {
                let mut inner = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    inner.push(next);
                }
                let inner = inner.trim();
                let unquoted = inner
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                match unquoted {
                    Some(field) => segments.push(Segment::Field(field.to_string())),
                    None => match inner.parse::<usize>() {
                        Ok(index) => segments.push(Segment::Index(index)),
                        Err(_) => {
                            return Err(TemplateError::Parse(format!(
                                "Expected a literal index in '{source}'"
                            )))
                        }
                    },
                }
            }
            other => {
                return Err(TemplateError::Parse(format!(
                    "Unexpected character '{other}' in '{source}'"
                )))
            }
        }
    }

    Ok(Path {
        root,
        segments,
        display: source.to_string(),
    })
}

fn parse_cond(source: &str) -> Result<Cond, TemplateError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(TemplateError::Parse("Expected a condition".to_string()));
    }
    // Liquid has no operator precedence and no parentheses: operators bind right to left.
    if let Some((left, rest, is_and)) = split_boolean(source) {
        let left = parse_cond(&left)?;
        let right = parse_cond(&rest)?;
        return Ok(if is_and {
            Cond::And(Box::new(left), Box::new(right))
        } else {
            Cond::Or(Box::new(left), Box::new(right))
        });
    }
    for (token, op) in [
        ("==", CompareOp::Eq),
        ("!=", CompareOp::Ne),
        (">=", CompareOp::Ge),
        ("<=", CompareOp::Le),
        (">", CompareOp::Gt),
        ("<", CompareOp::Lt),
        (" contains ", CompareOp::Contains),
    ] {
        if let Some(at) = find_outside_quotes(source, token) {
            let left = parse_expr(source[..at].trim())?;
            let right = parse_expr(source[at + token.len()..].trim())?;
            return Ok(Cond::Compare(left, op, right));
        }
    }
    Ok(Cond::Truthy(parse_expr(source)?))
}

fn split_boolean(source: &str) -> Option<(String, String, bool)> {
    let and = find_outside_quotes(source, " and ");
    let or = find_outside_quotes(source, " or ");
    match (and, or) {
        (Some(a), Some(o)) if a < o => {
            Some((source[..a].to_string(), source[a + 5..].to_string(), true))
        }
        (Some(_), Some(o)) => Some((source[..o].to_string(), source[o + 4..].to_string(), false)),
        (Some(a), None) => Some((source[..a].to_string(), source[a + 5..].to_string(), true)),
        (None, Some(o)) => Some((source[..o].to_string(), source[o + 4..].to_string(), false)),
        (None, None) => None,
    }
}

fn find_outside_quotes(source: &str, needle: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        match quote {
            Some(q) => {
                if byte == q {
                    quote = None;
                }
            }
            None => {
                if byte == b'"' || byte == b'\'' {
                    quote = Some(byte);
                } else if source[i..].starts_with(needle) {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// rendering

enum Flow {
    Normal,
    Break,
    Continue,
}

struct Renderer<'a> {
    vars: &'a Vars,
    assigns: Map<String, Value>,
    scopes: Vec<BTreeMap<String, Value>>,
}

impl<'a> Renderer<'a> {
    fn new(vars: &'a Vars) -> Renderer<'a> {
        Renderer {
            vars,
            assigns: Map::new(),
            scopes: Vec::new(),
        }
    }

    fn run(&mut self, nodes: &[Node], out: &mut String) -> Result<Flow, TemplateError> {
        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Output(expr) => {
                    let value = self.eval(expr, true)?;
                    out.push_str(&to_display(&value));
                }
                Node::Assign { target, value } => {
                    let value = self.eval(value, true)?;
                    self.assigns.insert(target.clone(), value);
                }
                Node::Break => return Ok(Flow::Break),
                Node::Continue => return Ok(Flow::Continue),
                Node::If {
                    branches,
                    otherwise,
                } => {
                    let mut taken = false;
                    for (cond, body) in branches {
                        // Undefined variables in an if/elsif condition are not an error: the
                        // reference implementation discards them, and only output positions,
                        // `for` enumerables, `unless` conditions and `assign` sources are checked.
                        if self.eval_cond(cond, false)? {
                            taken = true;
                            match self.run(body, out)? {
                                Flow::Normal => {}
                                flow => return Ok(flow),
                            }
                            break;
                        }
                    }
                    if !taken {
                        if let Some(body) = otherwise {
                            match self.run(body, out)? {
                                Flow::Normal => {}
                                flow => return Ok(flow),
                            }
                        }
                    }
                }
                Node::Unless {
                    cond,
                    body,
                    otherwise,
                } => {
                    if self.eval_cond(cond, true)? {
                        if let Some(body) = otherwise {
                            match self.run(body, out)? {
                                Flow::Normal => {}
                                flow => return Ok(flow),
                            }
                        }
                    } else {
                        match self.run(body, out)? {
                            Flow::Normal => {}
                            flow => return Ok(flow),
                        }
                    }
                }
                Node::For {
                    var,
                    source,
                    body,
                    otherwise,
                } => {
                    let items = match self.eval(source, true)? {
                        Value::Array(items) => items,
                        Value::Null => Vec::new(),
                        Value::Object(map) => map
                            .into_iter()
                            .map(|(k, v)| Value::Array(vec![Value::String(k), v]))
                            .collect(),
                        other => vec![other],
                    };

                    if items.is_empty() {
                        if let Some(body) = otherwise {
                            match self.run(body, out)? {
                                Flow::Normal => {}
                                flow => return Ok(flow),
                            }
                        }
                        continue;
                    }

                    let length = items.len();
                    for (index, item) in items.into_iter().enumerate() {
                        let mut scope = BTreeMap::new();
                        scope.insert(var.clone(), item);
                        scope.insert("forloop".to_string(), forloop(index, length));
                        self.scopes.push(scope);
                        let flow = self.run(body, out);
                        self.scopes.pop();
                        match flow? {
                            Flow::Normal | Flow::Continue => {}
                            Flow::Break => break,
                        }
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }

    fn eval(&self, expr: &Expr, strict: bool) -> Result<Value, TemplateError> {
        let mut value = match &expr.term {
            Term::Literal(literal) => literal.clone(),
            Term::Path(path) => self.lookup(path, strict)?,
        };
        for filter in &expr.filters {
            let mut args = Vec::with_capacity(filter.args.len());
            for arg in &filter.args {
                args.push(match arg {
                    Term::Literal(literal) => literal.clone(),
                    Term::Path(path) => self.lookup(path, strict)?,
                });
            }
            value = apply_filter(&filter.name, value, &args)?;
        }
        Ok(value)
    }

    fn eval_cond(&self, cond: &Cond, strict: bool) -> Result<bool, TemplateError> {
        match cond {
            Cond::Or(left, right) => {
                Ok(self.eval_cond(left, strict)? || self.eval_cond(right, strict)?)
            }
            Cond::And(left, right) => {
                Ok(self.eval_cond(left, strict)? && self.eval_cond(right, strict)?)
            }
            Cond::Truthy(expr) => Ok(truthy(&self.eval(expr, strict)?)),
            Cond::Compare(left, op, right) => {
                let left = self.eval(left, strict)?;
                let right = self.eval(right, strict)?;
                Ok(compare(&left, *op, &right))
            }
        }
    }

    fn lookup(&self, path: &Path, strict: bool) -> Result<Value, TemplateError> {
        let mut current: Option<&Value> = None;
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.get(&path.root) {
                current = Some(value);
                break;
            }
        }
        if current.is_none() {
            current = self.assigns.get(&path.root);
        }
        if current.is_none() {
            current = self.vars.as_map().get(&path.root);
        }

        let mut value = match current {
            Some(value) => value.clone(),
            None => {
                return if strict {
                    Err(TemplateError::MissingVariable(path.display.clone()))
                } else {
                    Ok(Value::Null)
                }
            }
        };

        for segment in &path.segments {
            value = match (&value, segment) {
                (Value::Object(map), Segment::Field(field)) => match map.get(field) {
                    Some(next) => next.clone(),
                    None => {
                        return if strict {
                            Err(TemplateError::MissingVariable(path.display.clone()))
                        } else {
                            Ok(Value::Null)
                        }
                    }
                },
                (Value::Array(items), Segment::Index(index)) => {
                    items.get(*index).cloned().unwrap_or(Value::Null)
                }
                (Value::Array(items), Segment::Field(field)) if field == "size" => {
                    Value::Number(items.len().into())
                }
                (Value::String(string), Segment::Field(field)) if field == "size" => {
                    Value::Number(string.chars().count().into())
                }
                _ => Value::Null,
            };
        }

        Ok(value)
    }
}

fn forloop(index: usize, length: usize) -> Value {
    let mut map = Map::new();
    map.insert("index".to_string(), Value::Number((index + 1).into()));
    map.insert("index0".to_string(), Value::Number(index.into()));
    map.insert("rindex".to_string(), Value::Number((length - index).into()));
    map.insert(
        "rindex0".to_string(),
        Value::Number((length - index - 1).into()),
    );
    map.insert("first".to_string(), Value::Bool(index == 0));
    map.insert("last".to_string(), Value::Bool(index + 1 == length));
    map.insert("length".to_string(), Value::Number(length.into()));
    Value::Object(map)
}

fn apply_filter(name: &str, value: Value, args: &[Value]) -> Result<Value, TemplateError> {
    match name {
        "size" => Ok(Value::Number(
            match &value {
                Value::String(string) => string.chars().count(),
                Value::Array(items) => items.len(),
                Value::Object(map) => map.len(),
                _ => 0,
            }
            .into(),
        )),
        "join" => {
            let separator = match args.first() {
                Some(Value::String(separator)) => separator.clone(),
                Some(other) => to_display(other),
                None => " ".to_string(),
            };
            match value {
                Value::Array(items) => Ok(Value::String(
                    items
                        .iter()
                        .map(to_display)
                        .collect::<Vec<_>>()
                        .join(&separator),
                )),
                other => Ok(Value::String(to_display(&other))),
            }
        }
        "default" => {
            let fallback = args.first().cloned().unwrap_or(Value::Null);
            Ok(if blank(&value) { fallback } else { value })
        }
        other => Err(TemplateError::Render(format!(
            "unknown filter '{other}' (allowed: size, join, default)"
        ))),
    }
}

fn blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(string) => string.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn compare(left: &Value, op: CompareOp, right: &Value) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Contains => match (left, right) {
            (Value::String(haystack), Value::String(needle)) => haystack.contains(needle),
            (Value::Array(items), needle) => items.contains(needle),
            _ => false,
        },
        _ => {
            let ordering = match (left, right) {
                (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
                    (Some(a), Some(b)) => a.partial_cmp(&b),
                    _ => None,
                },
                (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
                _ => None,
            };
            match ordering {
                None => false,
                Some(ordering) => match op {
                    CompareOp::Gt => ordering.is_gt(),
                    CompareOp::Lt => ordering.is_lt(),
                    CompareOp::Ge => ordering.is_ge(),
                    CompareOp::Le => ordering.is_le(),
                    _ => false,
                },
            }
        }
    }
}

/// Liquid's value-to-string rules: no escaping, `null` as the empty string, an integral float
/// keeps its decimal point, and a list is its elements concatenated with no separator.
pub(crate) fn to_display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(number) => number_to_string(number),
        Value::String(string) => string.clone(),
        Value::Array(items) => items.iter().map(to_display).collect(),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn number_to_string(number: &serde_json::Number) -> String {
    if let Some(int) = number.as_i64() {
        return int.to_string();
    }
    if let Some(uint) = number.as_u64() {
        return uint.to_string();
    }
    match number.as_f64() {
        Some(float) if float.fract() == 0.0 && float.is_finite() => format!("{float:.1}"),
        Some(float) => format!("{float}"),
        None => number.to_string(),
    }
}

// ---------------------------------------------------------------------------
// static analysis helpers

fn collect_filters(nodes: &[Node], out: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Output(expr) => filters_of(expr, out),
            Node::Assign { value, .. } => filters_of(value, out),
            Node::If {
                branches,
                otherwise,
            } => {
                for (cond, body) in branches {
                    cond_filters(cond, out);
                    collect_filters(body, out);
                }
                if let Some(body) = otherwise {
                    collect_filters(body, out);
                }
            }
            Node::Unless {
                cond,
                body,
                otherwise,
            } => {
                cond_filters(cond, out);
                collect_filters(body, out);
                if let Some(body) = otherwise {
                    collect_filters(body, out);
                }
            }
            Node::For {
                source,
                body,
                otherwise,
                ..
            } => {
                filters_of(source, out);
                collect_filters(body, out);
                if let Some(body) = otherwise {
                    collect_filters(body, out);
                }
            }
            Node::Text(_) | Node::Break | Node::Continue => {}
        }
    }
}

fn filters_of(expr: &Expr, out: &mut Vec<String>) {
    for filter in &expr.filters {
        out.push(filter.name.clone());
    }
}

fn cond_filters(cond: &Cond, out: &mut Vec<String>) {
    match cond {
        Cond::Or(left, right) | Cond::And(left, right) => {
            cond_filters(left, out);
            cond_filters(right, out);
        }
        Cond::Truthy(expr) => filters_of(expr, out),
        Cond::Compare(left, _, right) => {
            filters_of(left, out);
            filters_of(right, out);
        }
    }
}

fn collect_variables(nodes: &[Node], referenced: &mut Vec<String>, bound: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Output(expr) => expr_variables(expr, referenced),
            Node::Assign { target, value } => {
                expr_variables(value, referenced);
                bound.push(target.clone());
            }
            Node::If {
                branches,
                otherwise,
            } => {
                for (cond, body) in branches {
                    cond_variables(cond, referenced);
                    collect_variables(body, referenced, bound);
                }
                if let Some(body) = otherwise {
                    collect_variables(body, referenced, bound);
                }
            }
            Node::Unless {
                cond,
                body,
                otherwise,
            } => {
                cond_variables(cond, referenced);
                collect_variables(body, referenced, bound);
                if let Some(body) = otherwise {
                    collect_variables(body, referenced, bound);
                }
            }
            Node::For {
                var,
                source,
                body,
                otherwise,
            } => {
                expr_variables(source, referenced);
                bound.push(var.clone());
                collect_variables(body, referenced, bound);
                if let Some(body) = otherwise {
                    collect_variables(body, referenced, bound);
                }
            }
            Node::Text(_) | Node::Break | Node::Continue => {}
        }
    }
}

fn expr_variables(expr: &Expr, out: &mut Vec<String>) {
    if let Term::Path(path) = &expr.term {
        out.push(path.root.clone());
    }
    for filter in &expr.filters {
        for arg in &filter.args {
            if let Term::Path(path) = arg {
                out.push(path.root.clone());
            }
        }
    }
}

fn cond_variables(cond: &Cond, out: &mut Vec<String>) {
    match cond {
        Cond::Or(left, right) | Cond::And(left, right) => {
            cond_variables(left, out);
            cond_variables(right, out);
        }
        Cond::Truthy(expr) => expr_variables(expr, out),
        Cond::Compare(left, _, right) => {
            expr_variables(left, out);
            expr_variables(right, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> Vars {
        Vars::from(value)
    }

    #[test]
    fn renders_output_and_loops() {
        let out = render(
            "{% for i in items %}{{ forloop.index }}:{{ i }};{% endfor %}",
            &vars(json!({"items": ["a", "b"]})),
            Engine::Liquid,
        )
        .unwrap();
        assert_eq!(out, "1:a;2:b;");
    }

    #[test]
    fn missing_variable_reports_the_dotted_path() {
        let err = render(
            "{{ user.name }}",
            &vars(json!({"user": {}})),
            Engine::Liquid,
        )
        .unwrap_err();
        assert_eq!(err, TemplateError::MissingVariable("user.name".to_string()));
    }

    #[test]
    fn raw_engine_is_a_passthrough() {
        let source = "{% include \"x\" %} {{ a";
        assert_eq!(
            render(source, &Vars::new(), Engine::Raw).unwrap(),
            source.to_string()
        );
    }

    #[test]
    fn lint_flags_the_whitelist() {
        assert!(lint("Hello {{ name }}").is_ok());
        let reasons = lint("{{ s | upcase }}").unwrap_err();
        assert_eq!(reasons, vec![LintReason::DisallowedFilter("upcase".into())]);
        let reasons = lint("{% capture x %}y{% endcapture %}").unwrap_err();
        assert_eq!(
            reasons,
            vec![
                LintReason::DisallowedTag("capture".into()),
                LintReason::DisallowedTag("endcapture".into())
            ]
        );
    }

    #[test]
    fn message_slots_are_rejected_for_raw_and_liquid_engines() {
        let messages = vec![crate::snapshot::Message {
            role: String::new(),
            message_type: Some("slot".to_string()),
            content: String::new(),
            content_value: None,
            content_present: false,
            name: Some("history".to_string()),
            tool_call_id: None,
            tool_calls: Vec::new(),
            extra: serde_json::Map::new(),
        }];
        for engine in [Engine::Liquid, Engine::Raw] {
            assert_eq!(
                render_messages(
                    &messages,
                    &Vars::from(json!({"history":[{"role":"user","content":"before"}]})),
                    engine
                )
                .unwrap_err(),
                TemplateError::Render(
                    "Message slots are not supported; compose conversation history in app code."
                        .to_string()
                )
            );
        }
    }

    #[test]
    fn ordinary_history_variables_and_native_messages_still_render() {
        let messages = vec![
            serde_json::from_value::<crate::snapshot::Message>(
                json!({"role":"system","content":"Hi {{ name }} {{ history }}"}),
            )
            .unwrap(),
            serde_json::from_value::<crate::snapshot::Message>(json!({
                "role":"assistant",
                "content": null,
                "tool_calls": [{"id":"call_1","type":"function","function":{"name":"search","arguments":"{}"}}],
                "reasoning": {"kept": true}
            }))
            .unwrap(),
            serde_json::from_value::<crate::snapshot::Message>(json!({
                "role":"tool",
                "tool_call_id":"call_1",
                "content":[{"type":"text","text":"found"}]
            }))
            .unwrap(),
        ];

        let rendered = render_messages(
            &messages,
            &Vars::from(json!({"name":"Ada","history":"summary"})),
            Engine::Liquid,
        )
        .unwrap();

        assert_eq!(rendered[0].content, "Hi Ada summary");
        assert_eq!(rendered[1].content_json(), Value::Null);
        assert_eq!(rendered[1].tool_calls.len(), 1);
        assert!(rendered[1].extra.contains_key("reasoning"));
        assert!(rendered[2].content_json().is_array());
    }

    #[test]
    fn detects_input_variables() {
        assert_eq!(variables("{{ a }} {{ b.c }}"), vec!["a", "b"]);
        assert_eq!(
            variables("{% for item in items %}{{ item }}{% endfor %}"),
            vec!["items"]
        );
        assert_eq!(variables("{% assign x = y %}{{ x }}"), vec!["y"]);
    }
}
