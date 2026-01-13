//! The replacement script: an ordered list of search/replace rules.

use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// Text drawn on pages (including form XObjects and annotation appearances).
    Text,
    /// URI actions (hyperlink destinations).
    Links,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub search: String,
    pub replace: String,
    /// Treat `search` as a regular expression; `replace` may then use `$1`, `${name}`.
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// Defaults to both `text` and `links`.
    #[serde(default)]
    pub targets: Option<Vec<Target>>,
    /// Restrict text replacement to these pages, e.g. "1-3,7". Defaults to all pages.
    #[serde(default)]
    pub pages: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScriptFile {
    Rules { rules: Vec<RuleSpec> },
    List(Vec<RuleSpec>),
    Map { replacements: BTreeMap<String, String> },
}

pub struct Rule {
    pub spec: RuleSpec,
    /// Pattern for page text: whitespace in a literal search matches any run of
    /// whitespace, because spaces and line breaks in a PDF are often just gaps.
    pub text_re: Regex,
    pub link_re: Regex,
    pub text: bool,
    pub links: bool,
    pages: Option<Vec<(usize, usize)>>,
}

impl Rule {
    pub fn compile(spec: RuleSpec) -> Result<Rule> {
        if spec.search.is_empty() {
            bail!("a rule has an empty \"search\" string");
        }
        let build = |pattern: String| -> Result<Regex> {
            let pattern = if spec.whole_word { format!(r"\b(?:{pattern})\b") } else { pattern };
            RegexBuilder::new(&pattern)
                .case_insensitive(spec.ignore_case)
                .build()
                .with_context(|| format!("invalid search pattern {:?}", spec.search))
        };
        let (text_re, link_re) = if spec.regex {
            (build(spec.search.clone())?, build(spec.search.clone())?)
        } else {
            let words: Vec<String> = spec.search.split_whitespace().map(regex::escape).collect();
            let mut relaxed = words.join(r"\s+");
            if spec.search.starts_with(char::is_whitespace) {
                relaxed.insert_str(0, r"\s+");
            }
            if spec.search.ends_with(char::is_whitespace) && !words.is_empty() {
                relaxed.push_str(r"\s+");
            }
            (build(relaxed)?, build(regex::escape(&spec.search))?)
        };
        let targets = spec.targets.clone().unwrap_or_else(|| vec![Target::Text, Target::Links]);
        let pages = spec.pages.as_deref().map(parse_pages).transpose()?;
        Ok(Rule {
            text: targets.contains(&Target::Text),
            links: targets.contains(&Target::Links),
            text_re,
            link_re,
            pages,
            spec,
        })
    }

    pub fn applies_to_page(&self, page: usize) -> bool {
        self.pages.as_ref().is_none_or(|ranges| ranges.iter().any(|&(a, b)| a <= page && page <= b))
    }

    /// Expands the replacement for one match.
    pub fn replacement(&self, caps: &regex::Captures) -> String {
        if self.spec.regex {
            let mut out = String::new();
            caps.expand(&self.spec.replace, &mut out);
            out
        } else {
            self.spec.replace.clone()
        }
    }
}

fn parse_pages(spec: &str) -> Result<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let parse = |s: &str| s.trim().parse::<usize>().with_context(|| format!("invalid page range {spec:?}"));
        out.push(match part.split_once('-') {
            Some((a, b)) if b.trim().is_empty() => (parse(a)?, usize::MAX),
            Some((a, b)) => (parse(a)?, parse(b)?),
            None => (parse(part)?, parse(part)?),
        });
    }
    if out.is_empty() {
        bail!("empty page range");
    }
    Ok(out)
}

pub fn parse_script(json: &str) -> Result<Vec<RuleSpec>> {
    let file: ScriptFile = serde_json::from_str(json).context(
        "script must be {\"rules\": [{\"search\": ..., \"replace\": ...}, ...]}, a bare list of rules, \
         or {\"replacements\": {\"old\": \"new\"}}",
    )?;
    Ok(match file {
        ScriptFile::Rules { rules } | ScriptFile::List(rules) => rules,
        // A plain map has no order; apply longer searches first so that
        // overlapping keys behave predictably.
        ScriptFile::Map { replacements } => {
            let mut pairs: Vec<(String, String)> = replacements.into_iter().collect();
            pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
            pairs.into_iter().map(|(search, replace)| literal(search, replace)).collect()
        }
    })
}

pub fn literal(search: String, replace: String) -> RuleSpec {
    RuleSpec { search, replace, regex: false, ignore_case: false, whole_word: false, targets: None, pages: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_forms() {
        assert_eq!(parse_script(r#"{"rules":[{"search":"a","replace":"b","regex":true}]}"#).unwrap().len(), 1);
        assert_eq!(parse_script(r#"[{"search":"a","replace":"b"}]"#).unwrap().len(), 1);
        let map = parse_script(r#"{"replacements":{"ab":"x","abc":"y"}}"#).unwrap();
        assert_eq!(map[0].search, "abc");
        assert!(parse_script(r#"{"rules":[{"search":"a","replace":"b","bogus":1}]}"#).is_err());
    }

    #[test]
    fn literal_whitespace_is_relaxed_for_text_only() {
        let rule = Rule::compile(literal("Acme  Corp.".into(), "X".into())).unwrap();
        assert!(rule.text_re.is_match("Acme\nCorp."));
        assert!(!rule.text_re.is_match("Acme Corpx"));
        assert!(!rule.link_re.is_match("Acme Corp."));
    }

    #[test]
    fn pages() {
        let mut spec = literal("a".into(), "b".into());
        spec.pages = Some("1-2, 5, 9-".into());
        let rule = Rule::compile(spec).unwrap();
        assert!(rule.applies_to_page(2) && rule.applies_to_page(5) && rule.applies_to_page(40));
        assert!(!rule.applies_to_page(3));
    }
}
