use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Directive {
    pub name: String,
    pub args: Vec<String>,
    pub children: Option<Vec<Directive>>,
    pub source: String,
}

#[derive(Clone)]
struct Token {
    text: String,
    punctuation: bool,
    line: usize,
    column: usize,
}

pub fn load(path: &Path) -> Result<Vec<Directive>> {
    load_inner(path, &mut Vec::new())
}

fn load_inner(path: &Path, stack: &mut Vec<PathBuf>) -> Result<Vec<Directive>> {
    let path = path
        .canonicalize()
        .with_context(|| format!("read {}", path.display()))?;
    if stack.contains(&path) || stack.len() >= 32 {
        bail!("{}: include cycle or depth exceeds 32", path.display());
    }
    stack.push(path.clone());
    let bytes = fs::read(&path)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("{}: configuration exceeds 4 MiB", path.display());
    }
    let input = String::from_utf8(bytes)?;
    let tokens = tokenize(&input).with_context(|| path.display().to_string())?;
    let mut index = 0;
    let mut nodes = parse(&tokens, &mut index, &path, 0)?;
    let prefix = stack[0].parent().unwrap().to_path_buf();
    expand(&mut nodes, &prefix, stack)?;
    stack.pop();
    Ok(nodes)
}

fn expand(nodes: &mut Vec<Directive>, base: &Path, stack: &mut Vec<PathBuf>) -> Result<()> {
    let mut out = Vec::new();
    for mut node in nodes.drain(..) {
        if node.name == "include" {
            if node.children.is_some() || node.args.len() != 1 {
                bail!("{}: include expects one path", node.source);
            }
            let pattern = base.join(&node.args[0]);
            let mut paths: Vec<_> = glob::glob(pattern.to_str().context("non-UTF8 include path")?)?
                .collect::<Result<_, _>>()?;
            paths.sort();
            if paths.is_empty() && !node.args[0].contains(['*', '?', '[']) {
                bail!("{}: include not found", node.source);
            }
            for path in paths {
                out.extend(load_inner(&path, stack)?);
            }
        } else {
            if let Some(children) = &mut node.children {
                expand(children, base, stack)?;
            }
            out.push(node);
        }
    }
    *nodes = out;
    Ok(())
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<_> = input.chars().collect();
    let (mut i, mut line, mut column) = (0, 1, 1);
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
            i += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
                column += 1;
            }
            continue;
        }
        let (start_line, start_column) = (line, column);
        if "{};".contains(c) {
            tokens.push(Token {
                text: c.to_string(),
                punctuation: true,
                line,
                column,
            });
            i += 1;
            column += 1;
            continue;
        }
        let mut value = String::new();
        let mut quote = None;
        while i < chars.len() {
            let c = chars[i];
            if quote.is_none() && (c.is_whitespace() || "{};#".contains(c)) {
                break;
            }
            if c == '\\' {
                i += 1;
                column += 1;
                if i == chars.len() {
                    bail!("{line}:{column}: incomplete escape");
                }
                value.push(match chars[i] {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    c => c,
                });
            } else if quote == Some(c) {
                quote = None;
            } else if quote.is_none() && (c == '\'' || c == '"') {
                quote = Some(c);
            } else {
                value.push(c);
            }
            if c == '\n' {
                line += 1;
                column = 0;
            }
            i += 1;
            column += 1;
        }
        if quote.is_some() {
            bail!("{start_line}:{start_column}: unterminated string");
        }
        tokens.push(Token {
            text: value,
            punctuation: false,
            line: start_line,
            column: start_column,
        });
    }
    Ok(tokens)
}

fn parse(tokens: &[Token], index: &mut usize, path: &Path, depth: usize) -> Result<Vec<Directive>> {
    if depth > 32 {
        bail!("{}: block nesting exceeds 32", path.display());
    }
    let mut nodes = Vec::new();
    while let Some(t) = tokens.get(*index) {
        if t.punctuation && t.text == "}" {
            if depth == 0 {
                bail!(
                    "{}:{}:{}: unexpected closing brace",
                    path.display(),
                    t.line,
                    t.column
                );
            }
            *index += 1;
            return Ok(nodes);
        }
        let source = format!("{}:{}:{}", path.display(), t.line, t.column);
        if t.punctuation {
            bail!("{source}: expected directive");
        }
        let mut node = Directive {
            name: t.text.clone(),
            args: vec![],
            children: None,
            source,
        };
        *index += 1;
        loop {
            let token = tokens
                .get(*index)
                .with_context(|| format!("{}: missing semicolon or block", node.source))?;
            *index += 1;
            if token.punctuation {
                match token.text.as_str() {
                    ";" => break,
                    "{" => {
                        node.children = Some(parse(tokens, index, path, depth + 1)?);
                        break;
                    }
                    _ => bail!("{}: missing semicolon", node.source),
                }
            }
            node.args.push(token.text.clone());
        }
        nodes.push(node);
    }
    if depth > 0 {
        bail!("{}: unclosed block", path.display());
    }
    Ok(nodes)
}
