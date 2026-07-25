//! The XPath subset epubcheck's Schematron assertions are written in.
//!
//! Every `<rule context>`, `<assert test>`, `<report test>`, `<let value>` and
//! `<value-of select>` in the vendored `.sch` files is an XPath 2.0 expression,
//! evaluated by Saxon in epubcheck. This evaluates the same expressions against
//! [`Document`] — the constructs those 3500 lines actually use, and no more.
//!
//! What that comes to: the forward and reverse axes, name and kind tests,
//! predicates, the comparison and boolean operators, `some`/`every … satisfies`,
//! variables, and about two dozen functions. Anything outside it is a
//! [`XPathError`] at parse time rather than a wrong answer at evaluation time —
//! an assertion that cannot be evaluated must produce no finding at all, since a
//! guessed verdict on a book is worse than a missing one.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;

use crate::validate::source::epub::xml::tree::{Document, NodeId, NodeKind};

/// A malformed or unsupported expression. Never a statement about a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPathError(pub String);

impl std::fmt::Display for XPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A node in the XPath data model. Attributes are nodes too — `@id` and
/// `$e/@id` are ordinary path steps — so they need an identity of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeRef {
    pub node: NodeId,
    /// The index of an attribute within `node`, or `None` for the node itself.
    pub attr: Option<usize>,
}

impl NodeRef {
    pub fn element(node: NodeId) -> NodeRef {
        NodeRef { node, attr: None }
    }
}

/// One value in a sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Node(NodeRef),
    Str(String),
    Num(f64),
    Bool(bool),
}

pub type Sequence = Vec<Item>;

/// The variables in scope, sharing their values. See [`Context::vars`].
pub type Bindings = HashMap<String, Rc<Sequence>>;

// ---------------------------------------------------------------- syntax

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
    DescendantOrSelf,
    Parent,
    Ancestor,
    AncestorOrSelf,
    FollowingSibling,
    PrecedingSibling,
    Following,
    Preceding,
    Self_,
    Attribute,
}

impl Axis {
    fn parse(name: &str) -> Option<Axis> {
        Some(match name {
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "descendant-or-self" => Axis::DescendantOrSelf,
            "parent" => Axis::Parent,
            "ancestor" => Axis::Ancestor,
            "ancestor-or-self" => Axis::AncestorOrSelf,
            "following-sibling" => Axis::FollowingSibling,
            "preceding-sibling" => Axis::PrecedingSibling,
            "following" => Axis::Following,
            "preceding" => Axis::Preceding,
            "self" => Axis::Self_,
            "attribute" => Axis::Attribute,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeTest {
    /// `prefix:local`, with the prefix already resolved to a URI.
    Name {
        ns: Option<String>,
        local: String,
    },
    /// `*` or `prefix:*`.
    Any(Option<String>),
    Text,
    Node,
}

#[derive(Debug, Clone)]
enum StepKind {
    Axis(Axis, NodeTest),
    /// `…/(a | b)` — a parenthesized expression evaluated once per input node,
    /// which the EduPub schemas use to select a union of element names.
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
struct Step {
    kind: StepKind,
    predicates: Vec<Expr>,
}

impl Step {
    fn axis(axis: Axis, test: NodeTest) -> Step {
        Step {
            kind: StepKind::Axis(axis, test),
            predicates: Vec::new(),
        }
    }

    /// The `//` abbreviation, which is `/descendant-or-self::node()/`.
    fn any_descendant() -> Step {
        Step::axis(Axis::DescendantOrSelf, NodeTest::Node)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompOp {
    /// The general comparisons, which are existential over both sequences.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// The value comparisons, which require singletons.
    ValueEq,
    ValueNe,
    ValueLt,
    ValueLe,
    ValueGt,
    ValueGe,
    /// `is` — node identity, not value.
    Is,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Compare(CompOp, Box<Expr>, Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    /// `|` / `union` / `intersect` / `except` — set operations on nodes.
    SetOp(SetOp, Box<Expr>, Box<Expr>),
    /// `if (…) then … else …`.
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    /// A path: an optional root, an optional starting expression, then steps.
    Path {
        absolute: bool,
        start: Option<Box<Expr>>,
        steps: Vec<Step>,
    },
    /// `(a, b, c)` — a sequence constructor, distinct from mere grouping.
    Sequence(Vec<Expr>),
    Literal(String),
    Number(f64),
    Var(String),
    Call(String, Vec<Expr>),
    Quantified {
        every: bool,
        var: String,
        source: Box<Expr>,
        satisfies: Box<Expr>,
    },
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Name(String),
    /// `prefix:local`.
    QName(String, String),
    /// `prefix:*`.
    NsWildcard(String),
    Number(f64),
    Str(String),
    Var(String),
    Op(&'static str),
    Eof,
}

fn lex(source: &str) -> Result<Vec<Tok>, XPathError> {
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    let err = |m: &str| XPathError(format!("{m} in {source:?}"));

    while i < source.len() {
        let c = source[i..].chars().next().expect("in bounds");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if c == '"' || c == '\'' {
            let rest = &source[i + 1..];
            let end = rest.find(c).ok_or_else(|| err("unterminated string"))?;
            out.push(Tok::Str(rest[..end].to_string()));
            i += 1 + end + 1;
            continue;
        }
        if c.is_ascii_digit()
            || (c == '.'
                && source[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            while source[i..]
                .chars()
                .next()
                .is_some_and(|d| d.is_ascii_digit() || d == '.')
            {
                i += 1;
            }
            out.push(Tok::Number(
                source[start..i]
                    .parse()
                    .map_err(|_| err("malformed number"))?,
            ));
            continue;
        }
        if c == '$' {
            i += 1;
            let start = i;
            while source[i..].chars().next().is_some_and(is_name_char) {
                i += source[i..].chars().next().expect("in bounds").len_utf8();
            }
            if start == i {
                return Err(err("`$` with no name"));
            }
            out.push(Tok::Var(source[start..i].to_string()));
            continue;
        }
        if is_name_start(c) {
            let start = i;
            while source[i..].chars().next().is_some_and(is_name_char) {
                i += source[i..].chars().next().expect("in bounds").len_utf8();
            }
            let name = &source[start..i];
            // `prefix:local` and `prefix:*`, but not the `::` of an axis.
            if source[i..].starts_with(':') && !source[i..].starts_with("::") {
                if source[i + 1..].starts_with('*') {
                    i += 2;
                    out.push(Tok::NsWildcard(name.to_string()));
                    continue;
                }
                if source[i + 1..].chars().next().is_some_and(is_name_start) {
                    i += 1;
                    let local_start = i;
                    while source[i..].chars().next().is_some_and(is_name_char) {
                        i += source[i..].chars().next().expect("in bounds").len_utf8();
                    }
                    out.push(Tok::QName(
                        name.to_string(),
                        source[local_start..i].to_string(),
                    ));
                    continue;
                }
            }
            out.push(Tok::Name(name.to_string()));
            continue;
        }
        // Operators, longest first.
        const OPS: &[&str] = &[
            "//", "!=", "<=", ">=", "::", "..", "(", ")", "[", "]", "/", "@", ",", ".", "|", "+",
            "-", "*", "=", "<", ">",
        ];
        let op = OPS
            .iter()
            .find(|o| source[i..].starts_with(**o))
            .ok_or_else(|| err(&format!("unexpected character {c:?}")))?;
        out.push(Tok::Op(op));
        i += op.len();
    }
    out.push(Tok::Eof);
    Ok(out)
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_')
}

// ---------------------------------------------------------------- parser

/// A parsed expression, ready to evaluate against any document.
#[derive(Debug, Clone)]
pub struct XPath {
    expr: Expr,
    source: String,
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    /// Prefix → namespace URI, from the Schematron `<ns>` declarations.
    namespaces: &'a HashMap<String, String>,
    source: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        &self.toks[(self.pos + ahead).min(self.toks.len() - 1)]
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        self.pos = (self.pos + 1).min(self.toks.len() - 1);
        t
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Tok::Op(o) if *o == op) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_name(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Tok::Name(n) if n == word) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), XPathError> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {op:?}, found {:?}", self.peek())))
        }
    }

    fn err(&self, message: &str) -> XPathError {
        XPathError(format!("{message} in {:?}", self.source))
    }

    fn resolve_prefix(&self, prefix: &str) -> Result<Option<String>, XPathError> {
        match prefix {
            "xml" => Ok(Some(
                crate::validate::source::epub::xml::tree::XML_NS.to_string(),
            )),
            _ => self
                .namespaces
                .get(prefix)
                .map(|u| Some(u.clone()))
                .ok_or_else(|| self.err(&format!("undeclared prefix {prefix:?}"))),
        }
    }

    fn expr(&mut self) -> Result<Expr, XPathError> {
        // `if (…) then … else …` — the only place a name is followed by `(`
        // without being a function call.
        if matches!(self.peek(), Tok::Name(n) if n == "if")
            && matches!(self.peek_at(1), Tok::Op("("))
        {
            self.bump();
            self.bump();
            let condition = self.expr()?;
            self.expect_op(")")?;
            if !self.eat_name("then") {
                return Err(self.err("expected `then`"));
            }
            let then = self.expr()?;
            if !self.eat_name("else") {
                return Err(self.err("expected `else`"));
            }
            let otherwise = self.expr()?;
            return Ok(Expr::If(
                Box::new(condition),
                Box::new(then),
                Box::new(otherwise),
            ));
        }
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.and_expr()?;
        while self.eat_name("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.comparison()?;
        while self.eat_name("and") {
            left = Expr::And(Box::new(left), Box::new(self.comparison()?));
        }
        Ok(left)
    }

    fn comparison(&mut self) -> Result<Expr, XPathError> {
        let left = self.additive()?;
        let op = match self.peek() {
            Tok::Op("=") => CompOp::Eq,
            Tok::Op("!=") => CompOp::Ne,
            Tok::Op("<") => CompOp::Lt,
            Tok::Op("<=") => CompOp::Le,
            Tok::Op(">") => CompOp::Gt,
            Tok::Op(">=") => CompOp::Ge,
            Tok::Name(n) => match n.as_str() {
                "eq" => CompOp::ValueEq,
                "ne" => CompOp::ValueNe,
                "lt" => CompOp::ValueLt,
                "le" => CompOp::ValueLe,
                "gt" => CompOp::ValueGt,
                "ge" => CompOp::ValueGe,
                "is" => CompOp::Is,
                _ => return Ok(left),
            },
            _ => return Ok(left),
        };
        self.bump();
        let right = self.additive()?;
        Ok(Expr::Compare(op, Box::new(left), Box::new(right)))
    }

    fn additive(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Op("+") => ArithOp::Add,
                // A `-` directly after a name is part of that name, and the
                // lexer has already taken it; anything left here is subtraction.
                Tok::Op("-") => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.bump();
            left = Expr::Arith(op, Box::new(left), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.union()?;
        loop {
            let op = match self.peek() {
                Tok::Op("*") => ArithOp::Mul,
                Tok::Name(n) if n == "div" => ArithOp::Div,
                Tok::Name(n) if n == "mod" => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.bump();
            left = Expr::Arith(op, Box::new(left), Box::new(self.union()?));
        }
    }

    fn union(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.intersect_except()?;
        loop {
            if !self.eat_op("|") && !self.eat_name("union") {
                return Ok(left);
            }
            left = Expr::SetOp(
                SetOp::Union,
                Box::new(left),
                Box::new(self.intersect_except()?),
            );
        }
    }

    fn intersect_except(&mut self) -> Result<Expr, XPathError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Name(n) if n == "intersect" => SetOp::Intersect,
                Tok::Name(n) if n == "except" => SetOp::Except,
                _ => return Ok(left),
            };
            self.bump();
            left = Expr::SetOp(op, Box::new(left), Box::new(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr, XPathError> {
        if self.eat_op("-") {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.path()
    }

    /// A path expression, or one of the primaries that can start one.
    fn path(&mut self) -> Result<Expr, XPathError> {
        // `some $v in … satisfies …` / `every $v in … satisfies …`
        if let Tok::Name(word) = self.peek()
            && (word == "some" || word == "every")
            && matches!(self.peek_at(1), Tok::Var(_))
        {
            let every = word == "every";
            self.bump();
            let Tok::Var(var) = self.bump() else {
                unreachable!("checked above")
            };
            if !self.eat_name("in") {
                return Err(self.err("expected `in` after a quantified variable"));
            }
            let source = self.expr()?;
            if !self.eat_name("satisfies") {
                return Err(self.err("expected `satisfies`"));
            }
            let satisfies = self.expr()?;
            return Ok(Expr::Quantified {
                every,
                var,
                source: Box::new(source),
                satisfies: Box::new(satisfies),
            });
        }

        let absolute = matches!(self.peek(), Tok::Op("/") | Tok::Op("//"));
        let mut steps = Vec::new();
        let mut start = None;
        if absolute {
            if self.eat_op("//") {
                steps.push(Step::any_descendant());
            } else {
                self.bump();
            }
            // `/` alone is the document node.
            if self.at_path_end() {
                return Ok(Expr::Path {
                    absolute,
                    start: None,
                    steps,
                });
            }
        } else if self.starts_primary() {
            start = Some(Box::new(self.primary()?));
        }

        if start.is_none() || matches!(self.peek(), Tok::Op("/") | Tok::Op("//")) {
            if start.is_some() {
                if self.eat_op("//") {
                    steps.push(Step::any_descendant());
                } else {
                    self.bump();
                }
            }
            steps.push(self.step()?);
            loop {
                if self.eat_op("//") {
                    steps.push(Step::any_descendant());
                } else if !self.eat_op("/") {
                    break;
                }
                steps.push(self.step()?);
            }
        }
        Ok(Expr::Path {
            absolute,
            start,
            steps,
        })
    }

    fn at_path_end(&self) -> bool {
        matches!(
            self.peek(),
            Tok::Eof | Tok::Op(")") | Tok::Op("]") | Tok::Op(",") | Tok::Op("|")
        )
    }

    /// Whether the next token starts a primary expression rather than a step.
    /// A name followed by `(` is a function call unless it is a kind test.
    fn starts_primary(&self) -> bool {
        match self.peek() {
            Tok::Number(_) | Tok::Str(_) | Tok::Var(_) | Tok::Op("(") => true,
            Tok::Name(n) => matches!(self.peek_at(1), Tok::Op("(")) && n != "text" && n != "node",
            _ => false,
        }
    }

    fn primary(&mut self) -> Result<Expr, XPathError> {
        let mut base = match self.bump() {
            Tok::Number(n) => Expr::Number(n),
            Tok::Str(s) => Expr::Literal(s),
            Tok::Var(v) => Expr::Var(v),
            // Parentheses group, but `(a, b)` is a sequence constructor.
            Tok::Op("(") => {
                if self.eat_op(")") {
                    Expr::Sequence(Vec::new())
                } else {
                    let first = self.expr()?;
                    if self.eat_op(")") {
                        first
                    } else {
                        let mut items = vec![first];
                        while self.eat_op(",") {
                            items.push(self.expr()?);
                        }
                        self.expect_op(")")?;
                        Expr::Sequence(items)
                    }
                }
            }
            Tok::Name(name) => {
                self.expect_op("(")?;
                let mut args = Vec::new();
                if !self.eat_op(")") {
                    loop {
                        args.push(self.expr()?);
                        if !self.eat_op(",") {
                            self.expect_op(")")?;
                            break;
                        }
                    }
                }
                Expr::Call(name, args)
            }
            other => return Err(self.err(&format!("expected a primary, found {other:?}"))),
        };
        // A filter's predicates apply to the primary itself.
        while self.eat_op("[") {
            let predicate = self.expr()?;
            self.expect_op("]")?;
            base = Expr::Path {
                absolute: false,
                start: Some(Box::new(base)),
                steps: vec![Step {
                    kind: StepKind::Axis(Axis::Self_, NodeTest::Node),
                    predicates: vec![predicate],
                }],
            };
        }
        Ok(base)
    }

    fn step(&mut self) -> Result<Step, XPathError> {
        let kind = if self.eat_op(".") {
            StepKind::Axis(Axis::Self_, NodeTest::Node)
        } else if self.eat_op("..") {
            StepKind::Axis(Axis::Parent, NodeTest::Node)
        } else if self.eat_op("@") {
            StepKind::Axis(Axis::Attribute, self.node_test()?)
        } else if self.starts_primary() {
            // XPath 2.0 lets a step be any expression, which these schemas use
            // to map a function over a node set: `./normalize-space(@id)`
            // applies to each selected node in turn.
            StepKind::Expr(Box::new(self.primary()?))
        } else if let Tok::Name(name) = self.peek().clone()
            && matches!(self.peek_at(1), Tok::Op("::"))
        {
            let axis =
                Axis::parse(&name).ok_or_else(|| self.err(&format!("unknown axis {name:?}")))?;
            self.bump();
            self.bump();
            StepKind::Axis(axis, self.node_test()?)
        } else {
            StepKind::Axis(Axis::Child, self.node_test()?)
        };
        let mut predicates = Vec::new();
        while self.eat_op("[") {
            predicates.push(self.expr()?);
            self.expect_op("]")?;
        }
        Ok(Step { kind, predicates })
    }

    fn node_test(&mut self) -> Result<NodeTest, XPathError> {
        match self.bump() {
            Tok::Op("*") => Ok(NodeTest::Any(None)),
            Tok::NsWildcard(prefix) => Ok(NodeTest::Any(self.resolve_prefix(&prefix)?)),
            Tok::QName(prefix, local) => Ok(NodeTest::Name {
                ns: self.resolve_prefix(&prefix)?,
                local,
            }),
            Tok::Name(name) if name == "text" && matches!(self.peek(), Tok::Op("(")) => {
                self.bump();
                self.expect_op(")")?;
                Ok(NodeTest::Text)
            }
            Tok::Name(name) if name == "node" && matches!(self.peek(), Tok::Op("(")) => {
                self.bump();
                self.expect_op(")")?;
                Ok(NodeTest::Node)
            }
            // An unprefixed name in a path selects the no-namespace name — which
            // for an attribute is what every schema here means, and for an
            // element only ever appears in `@`-steps in these files.
            Tok::Name(name) => Ok(NodeTest::Name {
                ns: None,
                local: name,
            }),
            other => Err(self.err(&format!("expected a node test, found {other:?}"))),
        }
    }
}

impl XPath {
    /// Parse `source`, resolving prefixes through the Schematron `<ns>` map.
    pub fn parse(source: &str, namespaces: &HashMap<String, String>) -> Result<XPath, XPathError> {
        let mut parser = Parser {
            toks: lex(source)?,
            pos: 0,
            namespaces,
            source,
        };
        let expr = parser.expr()?;
        if !matches!(parser.peek(), Tok::Eof) {
            return Err(parser.err(&format!("trailing {:?}", parser.peek())));
        }
        Ok(XPath {
            expr,
            source: source.to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Reinterpret the expression as an XSLT **match pattern**, which is what a
    /// Schematron `<rule context>` is.
    ///
    /// A pattern says which nodes a rule applies to, not where to look from:
    /// `h:a` matches every `<a>` in the document, and `opf:package/opf:metadata`
    /// every `<metadata>` whose parent is a `<package>`. Evaluated as an
    /// ordinary path from the document node, the first would select only a root
    /// `<a>` and the second nothing at all. Rooting each relative branch at
    /// `descendant-or-self::node()` turns one into the other.
    pub fn into_match_pattern(mut self) -> XPath {
        fn root(expr: &mut Expr) {
            match expr {
                Expr::SetOp(SetOp::Union, a, b) => {
                    root(a);
                    root(b);
                }
                Expr::Path {
                    absolute,
                    start,
                    steps,
                } if !*absolute && start.is_none() => {
                    *absolute = true;
                    steps.insert(0, Step::any_descendant());
                }
                _ => {}
            }
        }
        root(&mut self.expr);
        self
    }
}

// ---------------------------------------------------------------- evaluation

/// What an expression is evaluated against.
pub struct Context<'a> {
    pub doc: &'a Document,
    pub item: Item,
    pub position: usize,
    pub size: usize,
    /// `<let>` bindings and quantified variables.
    ///
    /// Each value is shared, not owned: a context is copied for every item a
    /// step visits, and `epub-xhtml-30.sch` binds `$id-set` to `//*[@id]` —
    /// every id in the document. Copying *that* per visited node is how one
    /// chapter came to take seven minutes to validate. A binding is only
    /// materialized where an expression actually reads it.
    pub vars: Bindings,
    /// Schematron's `current()` — the node the enclosing rule matched, which a
    /// predicate's `.` no longer refers to.
    pub current: Option<NodeRef>,
}

impl<'a> Context<'a> {
    pub fn new(doc: &'a Document, node: NodeRef) -> Context<'a> {
        Context {
            doc,
            item: Item::Node(node),
            position: 1,
            size: 1,
            vars: HashMap::new(),
            current: Some(node),
        }
    }

    fn with_item(&self, item: Item, position: usize, size: usize) -> Context<'_> {
        Context {
            doc: self.doc,
            item,
            position,
            size,
            vars: self.vars.clone(),
            current: self.current,
        }
    }
}

impl XPath {
    pub fn eval(&self, ctx: &Context) -> Result<Sequence, XPathError> {
        eval(&self.expr, ctx)
    }

    /// The expression's effective boolean value — what an `<assert>` wants.
    pub fn eval_bool(&self, ctx: &Context) -> Result<bool, XPathError> {
        effective_boolean(&self.eval(ctx)?, ctx.doc)
    }

    /// The expression's string value — what a `<value-of>` wants.
    pub fn eval_string(&self, ctx: &Context) -> Result<String, XPathError> {
        let seq = self.eval(ctx)?;
        Ok(seq
            .iter()
            .map(|i| string_value(i, ctx.doc))
            .collect::<Vec<_>>()
            .join(" "))
    }
}

fn eval(expr: &Expr, ctx: &Context) -> Result<Sequence, XPathError> {
    match expr {
        Expr::Sequence(items) => {
            let mut out = Sequence::new();
            for item in items {
                out.extend(eval(item, ctx)?);
            }
            Ok(out)
        }
        Expr::Literal(s) => Ok(vec![Item::Str(s.clone())]),
        Expr::Number(n) => Ok(vec![Item::Num(*n)]),
        Expr::Var(name) => ctx
            .vars
            .get(name)
            .map(|value| (**value).clone())
            .ok_or_else(|| XPathError(format!("undefined variable ${name}"))),
        Expr::Or(a, b) => Ok(vec![Item::Bool(
            effective_boolean(&eval(a, ctx)?, ctx.doc)?
                || effective_boolean(&eval(b, ctx)?, ctx.doc)?,
        )]),
        Expr::And(a, b) => Ok(vec![Item::Bool(
            effective_boolean(&eval(a, ctx)?, ctx.doc)?
                && effective_boolean(&eval(b, ctx)?, ctx.doc)?,
        )]),
        Expr::Negate(a) => Ok(vec![Item::Num(-number_of(&eval(a, ctx)?, ctx.doc))]),
        Expr::If(condition, then, otherwise) => {
            match effective_boolean(&eval(condition, ctx)?, ctx.doc)? {
                true => eval(then, ctx),
                false => eval(otherwise, ctx),
            }
        }
        Expr::SetOp(op, a, b) => {
            let (left, right) = (to_nodes(&eval(a, ctx)?)?, to_nodes(&eval(b, ctx)?)?);
            let mut nodes: Vec<NodeRef> = match op {
                SetOp::Union => left.into_iter().chain(right).collect(),
                SetOp::Intersect => left.into_iter().filter(|n| right.contains(n)).collect(),
                SetOp::Except => left.into_iter().filter(|n| !right.contains(n)).collect(),
            };
            // Set operations return their result in document order, which for
            // this tree is node-id order because nodes are numbered as parsed.
            nodes.sort_unstable();
            nodes.dedup();
            Ok(nodes.into_iter().map(Item::Node).collect())
        }
        Expr::Arith(op, a, b) => {
            let (x, y) = (
                number_of(&eval(a, ctx)?, ctx.doc),
                number_of(&eval(b, ctx)?, ctx.doc),
            );
            Ok(vec![Item::Num(match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => x / y,
                ArithOp::Mod => x % y,
            })])
        }
        Expr::Compare(op, a, b) => {
            let (left, right) = (eval(a, ctx)?, eval(b, ctx)?);
            Ok(vec![Item::Bool(compare(*op, &left, &right, ctx.doc)?)])
        }
        Expr::Quantified {
            every,
            var,
            source,
            satisfies,
        } => {
            let items = eval(source, ctx)?;
            let mut result = *every;
            for item in items {
                let mut inner = ctx.with_item(ctx.item.clone(), ctx.position, ctx.size);
                inner.vars.insert(var.clone(), Rc::new(vec![item]));
                let holds = effective_boolean(&eval(satisfies, &inner)?, ctx.doc)?;
                if *every && !holds {
                    result = false;
                    break;
                }
                if !*every && holds {
                    result = true;
                    break;
                }
            }
            Ok(vec![Item::Bool(result)])
        }
        Expr::Call(name, args) => call(name, args, ctx),
        Expr::Path {
            absolute,
            start,
            steps,
        } => {
            let mut current: Sequence = if *absolute {
                vec![Item::Node(NodeRef::element(ctx.doc.root()))]
            } else if let Some(start) = start {
                eval(start, ctx)?
            } else {
                vec![ctx.item.clone()]
            };
            for step in steps {
                current = apply_step(step, &current, ctx)?;
            }
            Ok(current)
        }
    }
}

fn apply_step(step: &Step, input: &Sequence, ctx: &Context) -> Result<Sequence, XPathError> {
    let mut out: Sequence = Vec::new();
    for item in input {
        let candidates: Sequence = match &step.kind {
            StepKind::Axis(axis, test) => {
                let Item::Node(node) = item else {
                    return Err(XPathError("an axis step needs a node".into()));
                };
                // `axis_nodes` already returns a reverse axis counted outwards
                // from the context node, which is the order a positional
                // predicate on such an axis is numbered in.
                let mut nodes = axis_nodes(*axis, *node, ctx.doc);
                nodes.retain(|n| test_matches(test, *n, ctx.doc));
                nodes.into_iter().map(Item::Node).collect()
            }
            StepKind::Expr(expr) => {
                let inner = ctx.with_item(item.clone(), 1, 1);
                eval(expr, &inner)?
            }
        };
        // Predicates apply per input item, with positions numbered within that
        // item's own results.
        let size = candidates.len();
        for (index, candidate) in candidates.into_iter().enumerate() {
            let position = index + 1;
            let mut keep = true;
            for predicate in &step.predicates {
                let inner = ctx.with_item(candidate.clone(), position, size);
                let value = eval(predicate, &inner)?;
                keep = match value.as_slice() {
                    [Item::Num(n)] => (*n - position as f64).abs() < f64::EPSILON,
                    other => effective_boolean(other, ctx.doc)?,
                };
                if !keep {
                    break;
                }
            }
            if keep {
                out.push(candidate);
            }
        }
    }
    // Consecutive duplicates only: a node reached from two different inputs is
    // adjacent here, and the general dedup a node set wants belongs to the set
    // operators, which sort first.
    out.dedup();
    Ok(out)
}

/// The nodes on `axis` from `node`, in document order (reverse axes reversed).
fn axis_nodes(axis: Axis, node: NodeRef, doc: &Document) -> Vec<NodeRef> {
    // An attribute node has no children and no siblings; only `self` and the
    // upward axes reach anywhere from one.
    if node.attr.is_some() {
        return match axis {
            Axis::Self_ => vec![node],
            Axis::Parent => vec![NodeRef::element(node.node)],
            Axis::Ancestor | Axis::AncestorOrSelf => {
                let mut out = if axis == Axis::AncestorOrSelf {
                    vec![node]
                } else {
                    Vec::new()
                };
                out.push(NodeRef::element(node.node));
                out.extend(ancestors(node.node, doc));
                out
            }
            _ => Vec::new(),
        };
    }
    let id = node.node;
    match axis {
        Axis::Self_ => vec![node],
        Axis::Child => doc.children(id).map(NodeRef::element).collect(),
        Axis::Attribute => match doc.element(id) {
            Some(e) => (0..e.attrs.len())
                .map(|i| NodeRef {
                    node: id,
                    attr: Some(i),
                })
                .collect(),
            None => Vec::new(),
        },
        Axis::Descendant => doc
            .descendants(id)
            .into_iter()
            .skip(1)
            .map(NodeRef::element)
            .collect(),
        Axis::DescendantOrSelf => doc
            .descendants(id)
            .into_iter()
            .map(NodeRef::element)
            .collect(),
        Axis::Parent => doc
            .node(id)
            .parent
            .into_iter()
            .map(NodeRef::element)
            .collect(),
        Axis::Ancestor => ancestors(id, doc),
        Axis::AncestorOrSelf => {
            let mut out = vec![node];
            out.extend(ancestors(id, doc));
            out
        }
        Axis::FollowingSibling | Axis::PrecedingSibling => {
            let Some(parent) = doc.node(id).parent else {
                return Vec::new();
            };
            let siblings: Vec<NodeId> = doc.children(parent).collect();
            let Some(index) = siblings.iter().position(|s| *s == id) else {
                return Vec::new();
            };
            match axis {
                Axis::FollowingSibling => siblings[index + 1..]
                    .iter()
                    .copied()
                    .map(NodeRef::element)
                    .collect(),
                _ => siblings[..index]
                    .iter()
                    .rev()
                    .copied()
                    .map(NodeRef::element)
                    .collect(),
            }
        }
        Axis::Following | Axis::Preceding => {
            let all = doc.descendants(doc.root());
            let Some(index) = all.iter().position(|n| *n == id) else {
                return Vec::new();
            };
            let subtree = doc.descendants(id);
            match axis {
                Axis::Following => all[index..]
                    .iter()
                    .filter(|n| !subtree.contains(n))
                    .copied()
                    .map(NodeRef::element)
                    .collect(),
                _ => {
                    let ancestors: Vec<NodeId> =
                        ancestors(id, doc).into_iter().map(|a| a.node).collect();
                    all[..index]
                        .iter()
                        .filter(|n| !ancestors.contains(n))
                        .rev()
                        .copied()
                        .map(NodeRef::element)
                        .collect()
                }
            }
        }
    }
}

fn ancestors(id: NodeId, doc: &Document) -> Vec<NodeRef> {
    let mut out = Vec::new();
    let mut cur = doc.node(id).parent;
    while let Some(parent) = cur {
        out.push(NodeRef::element(parent));
        cur = doc.node(parent).parent;
    }
    out
}

fn test_matches(test: &NodeTest, node: NodeRef, doc: &Document) -> bool {
    let name = match node.attr {
        Some(i) => doc.element(node.node).map(|e| &e.attrs[i].name),
        None => doc.element(node.node).map(|e| &e.name),
    };
    match test {
        NodeTest::Node => true,
        NodeTest::Text => {
            node.attr.is_none() && matches!(doc.node(node.node).kind, NodeKind::Text(_))
        }
        NodeTest::Any(ns) => match (name, ns) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(n), Some(uri)) => doc.expanded(n).0 == Some(uri.as_str()),
        },
        NodeTest::Name { ns, local } => match name {
            None => false,
            Some(n) => {
                let (node_ns, node_local) = doc.expanded(n);
                node_local == local && node_ns == ns.as_deref()
            }
        },
    }
}

// ---------------------------------------------------------------- values

fn string_value(item: &Item, doc: &Document) -> String {
    match item {
        Item::Str(s) => s.clone(),
        Item::Bool(b) => b.to_string(),
        Item::Num(n) => format_number(*n),
        Item::Node(node) => match node.attr {
            Some(i) => doc
                .element(node.node)
                .map(|e| e.attrs[i].value.clone())
                .unwrap_or_default(),
            None => doc.string_value(node.node),
        },
    }
}

/// XPath's number formatting: integers without a fractional part.
fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == n.trunc() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

fn number_of(seq: &[Item], doc: &Document) -> f64 {
    match seq.first() {
        None => f64::NAN,
        Some(Item::Num(n)) => *n,
        Some(Item::Bool(b)) => *b as u8 as f64,
        Some(other) => string_value(other, doc).trim().parse().unwrap_or(f64::NAN),
    }
}

/// XPath 2.0's effective boolean value.
fn effective_boolean(seq: &[Item], doc: &Document) -> Result<bool, XPathError> {
    Ok(match seq {
        [] => false,
        [Item::Bool(b)] => *b,
        [Item::Str(s)] => !s.is_empty(),
        [Item::Num(n)] => *n != 0.0 && !n.is_nan(),
        [Item::Node(_)] => true,
        // A sequence of more than one item is true iff it starts with a node;
        // anything else is a type error in XPath, but here it can only come
        // from a schema doing something this port does not model, and a `false`
        // verdict is the one that cannot invent a finding.
        [first, ..] => matches!(first, Item::Node(_)) || !string_value(first, doc).is_empty(),
    })
}

fn to_nodes(seq: &[Item]) -> Result<Vec<NodeRef>, XPathError> {
    seq.iter()
        .map(|i| match i {
            Item::Node(n) => Ok(*n),
            _ => Err(XPathError("expected nodes".into())),
        })
        .collect()
}

fn compare(op: CompOp, left: &[Item], right: &[Item], doc: &Document) -> Result<bool, XPathError> {
    if op == CompOp::Is {
        // Identity, not value: `. is $current` asks whether two expressions
        // reached the same node, which no string comparison can decide.
        return Ok(match (left, right) {
            ([Item::Node(a)], [Item::Node(b)]) => a == b,
            _ => false,
        });
    }
    if matches!(
        op,
        CompOp::ValueEq
            | CompOp::ValueNe
            | CompOp::ValueLt
            | CompOp::ValueLe
            | CompOp::ValueGt
            | CompOp::ValueGe
    ) {
        // A value comparison with an empty operand is empty, hence false.
        let (Some(a), Some(b)) = (left.first(), right.first()) else {
            return Ok(false);
        };
        return Ok(match op {
            CompOp::ValueEq => atom_equal(a, b, doc),
            CompOp::ValueNe => !atom_equal(a, b, doc),
            _ => {
                let (x, y) = (
                    number_of(std::slice::from_ref(a), doc),
                    number_of(std::slice::from_ref(b), doc),
                );
                match op {
                    CompOp::ValueLt => x < y,
                    CompOp::ValueLe => x <= y,
                    CompOp::ValueGt => x > y,
                    CompOp::ValueGe => x >= y,
                    _ => unreachable!("handled above"),
                }
            }
        });
    }
    // The general comparisons are existential over both sides.
    for a in left {
        for b in right {
            let holds = match op {
                CompOp::Eq => atom_equal(a, b, doc),
                CompOp::Ne => !atom_equal(a, b, doc),
                _ => {
                    let (x, y) = (
                        number_of(std::slice::from_ref(a), doc),
                        number_of(std::slice::from_ref(b), doc),
                    );
                    match op {
                        CompOp::Lt => x < y,
                        CompOp::Le => x <= y,
                        CompOp::Gt => x > y,
                        CompOp::Ge => x >= y,
                        _ => unreachable!("handled above"),
                    }
                }
            };
            if holds {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn atom_equal(a: &Item, b: &Item, doc: &Document) -> bool {
    match (a, b) {
        (Item::Num(_), _) | (_, Item::Num(_)) => {
            let (x, y) = (
                number_of(std::slice::from_ref(a), doc),
                number_of(std::slice::from_ref(b), doc),
            );
            x == y
        }
        (Item::Bool(x), _) => {
            *x == effective_boolean(std::slice::from_ref(b), doc).unwrap_or(false)
        }
        (_, Item::Bool(y)) => {
            effective_boolean(std::slice::from_ref(a), doc).unwrap_or(false) == *y
        }
        _ => string_value(a, doc) == string_value(b, doc),
    }
}

// ---------------------------------------------------------------- functions

/// Compile and cache a regular expression. The same handful of patterns are
/// evaluated once per matching element in a book, so compiling each time would
/// dominate the cost of validating a large content document.
///
/// `flags` is XPath 2.0's optional flags argument (`i` case-insensitive, `m`
/// multi-line, `s` dot-matches-all, `x` ignore whitespace), spelled as the
/// inline group the regex crate takes. Dropping it would make
/// `matches(…, 'text/html;\s*charset=utf-8', 'i')` — a real rule in the EPUB 3
/// content-document assertions — reject the uppercase spelling every second
/// book uses.
fn regex_for(pattern: &str, flags: &str) -> Result<&'static Regex, XPathError> {
    use std::sync::Mutex;
    static CACHE: OnceLock<Mutex<HashMap<String, &'static Regex>>> = OnceLock::new();
    // `q` (literal) has no inline spelling, so it is applied by escaping the
    // pattern instead. A flag outside the five is an XPath error; ignoring it
    // would silently change what the rule means, so it is left to fail below.
    let body = match flags.contains('q') {
        true => regex::escape(pattern),
        false => pattern.to_string(),
    };
    let inline: String = flags.chars().filter(|c| "imsx".contains(*c)).collect();
    let source = match inline.is_empty() {
        true => body,
        false => format!("(?{inline}:{body})"),
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().expect("regex cache");
    if let Some(re) = cache.get(&source) {
        return Ok(re);
    }
    let compiled: &'static Regex =
        Box::leak(Box::new(Regex::new(&source).map_err(|e| {
            XPathError(format!("bad pattern {pattern:?}: {e}"))
        })?));
    cache.insert(source, compiled);
    Ok(compiled)
}

fn call(name: &str, args: &[Expr], ctx: &Context) -> Result<Sequence, XPathError> {
    let evaluated: Result<Vec<Sequence>, XPathError> = args.iter().map(|a| eval(a, ctx)).collect();
    let args = evaluated?;
    let str_arg = |i: usize| -> String {
        match args.get(i) {
            None => string_value(&ctx.item, ctx.doc),
            Some(seq) => seq
                .iter()
                .map(|it| string_value(it, ctx.doc))
                .collect::<Vec<_>>()
                .join(""),
        }
    };
    // `str_arg` falls back to the context item, which is what the one-argument
    // forms of `string`/`normalize-space`/`local-name` mean. A genuinely
    // optional argument means *nothing* when absent, not the context item.
    let opt_str_arg = |i: usize| -> String {
        match args.get(i) {
            None => String::new(),
            Some(_) => str_arg(i),
        }
    };
    let num_arg = |i: usize| -> f64 {
        args.get(i)
            .map(|s| number_of(s, ctx.doc))
            .unwrap_or(f64::NAN)
    };
    let one = |b: bool| Ok(vec![Item::Bool(b)]);

    match name {
        "true" => one(true),
        "false" => one(false),
        "not" => one(!effective_boolean(
            args.first().unwrap_or(&vec![]),
            ctx.doc,
        )?),
        "boolean" => one(effective_boolean(args.first().unwrap_or(&vec![]), ctx.doc)?),
        "exists" => one(!args.first().map(|s| s.is_empty()).unwrap_or(true)),
        "empty" => one(args.first().map(|s| s.is_empty()).unwrap_or(true)),
        "count" => Ok(vec![Item::Num(
            args.first().map(|s| s.len()).unwrap_or(0) as f64
        )]),
        "position" => Ok(vec![Item::Num(ctx.position as f64)]),
        "last" => Ok(vec![Item::Num(ctx.size as f64)]),
        "current" => match ctx.current {
            Some(node) => Ok(vec![Item::Node(node)]),
            None => Ok(vec![]),
        },
        "string" => Ok(vec![Item::Str(str_arg(0))]),
        "number" => Ok(vec![Item::Num(match args.first() {
            None => number_of(std::slice::from_ref(&ctx.item), ctx.doc),
            Some(seq) => number_of(seq, ctx.doc),
        })]),
        "normalize-space" => Ok(vec![Item::Str(
            str_arg(0).split_whitespace().collect::<Vec<_>>().join(" "),
        )]),
        "string-length" => Ok(vec![Item::Num(str_arg(0).chars().count() as f64)]),
        "lower-case" => Ok(vec![Item::Str(str_arg(0).to_lowercase())]),
        "upper-case" => Ok(vec![Item::Str(str_arg(0).to_uppercase())]),
        "concat" => Ok(vec![Item::Str(
            (0..args.len()).map(str_arg).collect::<Vec<_>>().concat(),
        )]),
        "starts-with" => one(str_arg(0).starts_with(&str_arg(1))),
        "ends-with" => one(str_arg(0).ends_with(&str_arg(1))),
        "contains" => one(str_arg(0).contains(&str_arg(1))),
        "substring-before" => Ok(vec![Item::Str(
            str_arg(0)
                .split_once(&str_arg(1))
                .map(|(a, _)| a.to_string())
                .unwrap_or_default(),
        )]),
        "substring-after" => Ok(vec![Item::Str(
            str_arg(0)
                .split_once(&str_arg(1))
                .map(|(_, b)| b.to_string())
                .unwrap_or_default(),
        )]),
        // XPath's `substring` is 1-based and rounds its arguments.
        "substring" => {
            let chars: Vec<char> = str_arg(0).chars().collect();
            let start = num_arg(1).round();
            let end = match args.len() {
                3 => start + num_arg(2).round(),
                _ => f64::INFINITY,
            };
            Ok(vec![Item::Str(
                chars
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        let pos = *i as f64 + 1.0;
                        pos >= start && pos < end
                    })
                    .map(|(_, c)| *c)
                    .collect(),
            )])
        }
        "translate" => {
            let (from, to): (Vec<char>, Vec<char>) =
                (str_arg(1).chars().collect(), str_arg(2).chars().collect());
            Ok(vec![Item::Str(
                str_arg(0)
                    .chars()
                    .filter_map(|c| match from.iter().position(|f| *f == c) {
                        None => Some(c),
                        Some(i) => to.get(i).copied(),
                    })
                    .collect(),
            )])
        }
        "string-join" => {
            let separator = if args.len() > 1 {
                str_arg(1)
            } else {
                String::new()
            };
            Ok(vec![Item::Str(
                args.first()
                    .map(|s| {
                        s.iter()
                            .map(|i| string_value(i, ctx.doc))
                            .collect::<Vec<_>>()
                            .join(&separator)
                    })
                    .unwrap_or_default(),
            )])
        }
        // The trailing `flags` argument is optional in all three (XPath 2.0
        // §7.6); absent means no flags.
        "matches" => one(regex_for(&str_arg(1), &opt_str_arg(2))?.is_match(&str_arg(0))),
        "replace" => Ok(vec![Item::Str(
            regex_for(&str_arg(1), &opt_str_arg(3))?
                .replace_all(&str_arg(0), str_arg(2).as_str())
                .into_owned(),
        )]),
        "tokenize" => Ok(regex_for(&str_arg(1), &opt_str_arg(2))?
            .split(&str_arg(0))
            .filter(|t| !t.is_empty())
            .map(|t| Item::Str(t.to_string()))
            .collect()),
        "local-name" => Ok(vec![Item::Str(match args.first() {
            None => node_name(&ctx.item, ctx.doc)
                .map(|(_, l)| l)
                .unwrap_or_default(),
            Some(seq) => seq
                .first()
                .and_then(|i| node_name(i, ctx.doc))
                .map(|(_, l)| l)
                .unwrap_or_default(),
        })]),
        "name" => Ok(vec![Item::Str(match args.first() {
            None => qualified_name(&ctx.item, ctx.doc),
            Some(seq) => seq
                .first()
                .map(|i| qualified_name(i, ctx.doc))
                .unwrap_or_default(),
        })]),
        "namespace-uri" => Ok(vec![Item::Str(
            node_name(
                args.first().and_then(|s| s.first()).unwrap_or(&ctx.item),
                ctx.doc,
            )
            .and_then(|(ns, _)| ns)
            .unwrap_or_default(),
        )]),
        // There is no base URI here: every document a schema resolves against is
        // the one being validated, so the only thing `resolve-uri` can do that
        // matters is collapse dot segments before two references are compared.
        "resolve-uri" => Ok(vec![Item::Str(collapse_dot_segments(&str_arg(0)))]),
        other => Err(XPathError(format!("unsupported function {other}()"))),
    }
}

fn node_name(item: &Item, doc: &Document) -> Option<(Option<String>, String)> {
    let Item::Node(node) = item else { return None };
    let element = doc.element(node.node)?;
    let name = match node.attr {
        Some(i) => &element.attrs[i].name,
        None => &element.name,
    };
    let (ns, local) = doc.expanded(name);
    Some((ns.map(str::to_string), local.to_string()))
}

/// `name()` returns the name as written, prefix and all.
fn qualified_name(item: &Item, doc: &Document) -> String {
    let Item::Node(node) = item else {
        return String::new();
    };
    let Some(element) = doc.element(node.node) else {
        return String::new();
    };
    let local = match node.attr {
        Some(i) => &element.attrs[i].name.local,
        None => &element.name.local,
    };
    match (&element.prefix, node.attr) {
        (Some(prefix), None) => format!("{prefix}:{local}"),
        _ => local.clone(),
    }
}

fn collapse_dot_segments(url: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in url.split('/') {
        match segment {
            "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> HashMap<String, String> {
        HashMap::from([
            ("h".to_string(), "http://www.w3.org/1999/xhtml".to_string()),
            (
                "opf".to_string(),
                "http://www.idpf.org/2007/opf".to_string(),
            ),
        ])
    }

    const DOC: &str = r##"<html xmlns="http://www.w3.org/1999/xhtml">
      <head><title>Title</title></head>
      <body>
        <p id="a" class="x">one</p>
        <p id="b">two <a href="#a">link</a></p>
        <ul><li>i1</li><li>i2</li><li>i3</li></ul>
      </body></html>"##;

    fn eval_on(xml: &str, expr: &str) -> Sequence {
        let doc = Box::leak(Box::new(Document::parse(xml).expect("well-formed")));
        let root = doc.root_element().expect("root");
        let ctx = Context::new(doc, NodeRef::element(root));
        XPath::parse(expr, &ns())
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
            .eval(&ctx)
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    fn truth(expr: &str) -> bool {
        let doc = Box::leak(Box::new(Document::parse(DOC).expect("well-formed")));
        let root = doc.root_element().expect("root");
        let ctx = Context::new(doc, NodeRef::element(root));
        XPath::parse(expr, &ns())
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
            .eval_bool(&ctx)
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    fn text(expr: &str) -> String {
        let doc = Box::leak(Box::new(Document::parse(DOC).expect("well-formed")));
        let root = doc.root_element().expect("root");
        let ctx = Context::new(doc, NodeRef::element(root));
        XPath::parse(expr, &ns())
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
            .eval_string(&ctx)
            .unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    #[test]
    fn paths_axes_and_predicates() {
        assert_eq!(eval_on(DOC, "//h:p").len(), 2);
        assert_eq!(eval_on(DOC, "h:body/h:p").len(), 2);
        assert_eq!(eval_on(DOC, "//h:li[2]").len(), 1);
        assert_eq!(text("//h:li[2]"), "i2");
        assert_eq!(text(r##"//h:p[@id='b']/h:a/@href"##), "#a");
        assert_eq!(eval_on(DOC, "//h:a/ancestor::h:p").len(), 1);
        assert_eq!(eval_on(DOC, "//h:li[1]/following-sibling::h:li").len(), 2);
        assert_eq!(eval_on(DOC, "//h:li[3]/preceding-sibling::h:li").len(), 2);
        assert_eq!(eval_on(DOC, "//h:p/@*").len(), 3, "three attributes in all");
        assert!(truth("//h:p[@class]"), "a predicate on existence");
        assert!(!truth("//h:p[@nosuch]"));
        assert_eq!(text("//h:title/text()"), "Title");
    }

    #[test]
    fn comparisons_are_existential_but_value_comparisons_are_not() {
        assert!(truth("//h:p/@id = 'b'"), "any attribute equals");
        assert!(!truth("//h:p/@id = 'z'"));
        assert!(truth("count(//h:li) = 3"));
        assert!(truth("count(//h:li) > 2 and count(//h:li) < 4"));
        assert!(truth("//h:p[@id='a']/@id eq 'a'"));
        assert!(
            !truth("//h:nosuch/@id eq 'a'"),
            "a value comparison with an empty side is false, not an error"
        );
        assert!(truth("//h:p[1]/@id != //h:p[2]/@id"));
    }

    #[test]
    fn the_functions_the_schemas_use() {
        assert_eq!(text("normalize-space('  a   b ')"), "a b");
        assert_eq!(text("substring('abcdef', 2, 3)"), "bcd");
        assert_eq!(text("substring-after('a#b', '#')"), "b");
        assert_eq!(text("translate('a-b-c', '-', '_')"), "a_b_c");
        assert_eq!(text("lower-case('ABC')"), "abc");
        assert_eq!(text("concat('a', 'b', 'c')"), "abc");
        assert_eq!(text("string-length('abcd')"), "4");
        assert_eq!(text("string-join(('a','b'), '-')"), "a-b");
        assert!(truth("starts-with('abc', 'ab')"));
        assert!(truth("contains('abc', 'b')"));
        assert!(truth("matches('2020-01-01T00:00:00Z', '^[0-9]{4}-')"));
        assert_eq!(text("replace('a1b2', '[0-9]', '')"), "ab");
        assert_eq!(eval_on(DOC, "tokenize('a  b c', '\\s+')").len(), 3);
        assert!(truth("empty(//h:nosuch)"));
        assert!(truth("exists(//h:p)"));
        assert!(truth("not(exists(//h:nosuch))"));
        assert_eq!(text("local-name(//h:p[1])"), "p");
        assert_eq!(text("number('3') + 1"), "4");
    }

    /// The optional flags argument of the three regular-expression functions.
    /// Dropping it turns the EPUB 3 content-document rule
    /// `matches(…,'text/html;\s*charset=utf-8','i')` into a case-sensitive
    /// test, which rejects the uppercase `charset=UTF-8` half the world writes.
    #[test]
    fn regular_expression_flags_are_honoured() {
        assert!(truth(
            r#"matches(normalize-space('text/html; charset=UTF-8'),'text/html;\s*charset=utf-8','i')"#
        ));
        assert!(
            !truth(
                r#"matches(normalize-space('text/html; charset=UTF-8'),'text/html;\s*charset=utf-8')"#
            ),
            "without the flag the same value must not match"
        );
        // The flags argument must not be read as the context item when absent.
        assert!(truth("matches('ABC', 'abc', 'i')"));
        assert!(!truth("matches('ABC', 'abc')"));
        assert_eq!(text("replace('A1b2', '[a-z]', 'x', 'i')"), "x1x2");
        assert_eq!(eval_on(DOC, "tokenize('aXbxc', 'x', 'i')").len(), 3);
        // `q` takes the pattern literally rather than as a regular expression.
        assert!(truth("matches('a.c', 'a.c', 'q')"));
        assert!(!truth("matches('abc', 'a.c', 'q')"));
    }

    #[test]
    fn quantified_expressions() {
        assert!(truth("some $p in //h:p satisfies $p/@id = 'b'"));
        assert!(!truth("some $p in //h:p satisfies $p/@id = 'z'"));
        assert!(truth("every $p in //h:p satisfies $p/@id"));
        assert!(!truth("every $p in //h:p satisfies $p/@class"));
        assert!(
            truth(
                "every $t in tokenize('a b', '\\s+') satisfies (some $p in //h:p satisfies $p/@id)"
            ),
            "nested quantifiers"
        );
    }

    #[test]
    fn current_is_the_rule_node_not_the_predicate_node() {
        // Schematron's `current()` is what makes cross-references expressible:
        // inside `//x[@id = current()/@ref]`, `.` is the `x` being tested while
        // `current()` is still the element the rule matched.
        let doc = Box::leak(Box::new(
            Document::parse(
                r#"<r xmlns="http://www.w3.org/1999/xhtml"><a id="1"/><b ref="1"/></r>"#,
            )
            .expect("well-formed"),
        ));
        let b = doc
            .descendants(doc.root())
            .into_iter()
            .find(|n| doc.element(*n).is_some_and(|e| e.name.local == "b"))
            .expect("b");
        let ctx = Context::new(doc, NodeRef::element(b));
        let found = XPath::parse("//h:a[@id = current()/@ref]", &ns())
            .expect("parses")
            .eval(&ctx)
            .expect("evaluates");
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_unsupported_construct_is_an_error_not_a_verdict() {
        assert!(XPath::parse("nosuchfunction(.)", &ns()).is_ok(), "parses");
        let doc = Box::leak(Box::new(Document::parse(DOC).expect("well-formed")));
        let ctx = Context::new(doc, NodeRef::element(doc.root_element().expect("root")));
        assert!(
            XPath::parse("nosuchfunction(.)", &ns())
                .expect("parses")
                .eval(&ctx)
                .is_err(),
            "and fails loudly when evaluated"
        );
        assert!(
            XPath::parse("h:p[", &ns()).is_err(),
            "a malformed expression is rejected"
        );
        assert!(
            XPath::parse("undeclared:p", &ns()).is_err(),
            "so is an undeclared prefix"
        );
    }
}
