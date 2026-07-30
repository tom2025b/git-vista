//! A small, deliberately dumb CSS reader (M1.12, #65).
//!
//! It exists for one reason: the accessibility tripwires in [`super::audit`] must be
//! statements about the bytes in `styles.css`, not about a summary of them that someone
//! typed out once and that has been wrong ever since. Six citations have already rotted
//! in this milestone; a parser is how a citation stops being a citation.
//!
//! **Scope, honestly.** This handles the CSS that `styles.css` actually contains today:
//! comments, `@font-face`, one level of `@media` nesting, comma-separated selector
//! lists, and `prop: value` declarations with optional `!important`. It is not a CSS
//! parser. It does not understand strings containing braces or semicolons, nested
//! at-rules beyond one level, CSS nesting (`&`), or attribute selectors that contain
//! combinator characters. If `styles.css` grows any of those, the right move is to teach
//! this module about them — not to relax the tripwires that depend on it. Every
//! behaviour it *does* claim is pinned by a fixture test at the bottom of this file, so
//! the reader is verified independently of the stylesheet it reads.

/// One `prop: value` pair from a declaration block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Lowercased property name, e.g. `border-color`.
    pub property: String,
    /// The value with any `!important` flag removed, whitespace-collapsed.
    pub value: String,
    /// Whether the declaration carried `!important`.
    pub important: bool,
}

/// One style rule: its at-rule context, its selector list, and its declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The enclosing at-rule preludes, outermost first, with the leading `@` stripped —
    /// e.g. `["media print"]`. Empty for a top-level rule.
    ///
    /// Two rules only count as twins for the focus-parity tripwire when their contexts
    /// are equal: a `:focus-visible` rule inside `@media print` does nothing for a
    /// `:hover` rule outside it.
    pub at_context: Vec<String>,
    /// Whitespace-collapsed selectors, one per comma-separated entry.
    pub selectors: Vec<String>,
    pub declarations: Vec<Declaration>,
}

impl Rule {
    /// The set of property names this rule declares, in source order.
    pub fn properties(&self) -> Vec<&str> {
        self.declarations
            .iter()
            .map(|d| d.property.as_str())
            .collect()
    }

    /// The value of the last declaration of `property`, mirroring the cascade's
    /// within-block "last one wins".
    pub fn value_of(&self, property: &str) -> Option<&Declaration> {
        self.declarations
            .iter()
            .rev()
            .find(|d| d.property == property)
    }
}

/// Remove `/* … */` comments. Unterminated comments swallow the rest of the input, which
/// is what a browser does too.
pub fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Collapse every run of whitespace to a single space and trim the ends.
///
/// This is the only normalisation applied to selectors, which is why the module docs say
/// combinator spacing matters: `.a>.b` and `.a > .b` select the same elements but are
/// different strings here. `styles.css` uses descendant combinators only.
pub fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_declarations(body: &str) -> Vec<Declaration> {
    body.split(';')
        .filter_map(|chunk| {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                return None;
            }
            let (property, value) = chunk.split_once(':')?;
            let value = normalize_whitespace(value);
            let lowered = value.to_ascii_lowercase();
            let important = lowered.ends_with("!important");
            let value = if important {
                value[..value.len() - "!important".len()].trim_end().to_string()
            } else {
                value
            };
            Some(Declaration {
                property: normalize_whitespace(property).to_ascii_lowercase(),
                value,
                important,
            })
        })
        .collect()
}

/// At-rules whose block contains further rules rather than declarations.
fn is_nesting_at_rule(prelude: &str) -> bool {
    let head = prelude
        .trim_start_matches('@')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(head.as_str(), "media" | "supports" | "container" | "layer")
}

/// Read a stylesheet into its rules, flattened, each carrying its at-rule context.
pub fn parse(css: &str) -> Vec<Rule> {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    let mut at_context: Vec<String> = Vec::new();
    let mut prelude = String::new();
    let mut chars = css.chars();

    // A `loop` + explicit `next()` rather than `while let`, because the inner
    // declaration-block scan borrows the same iterator.
    loop {
        let Some(c) = chars.next() else { break };
        match c {
            '{' => {
                let head = normalize_whitespace(&prelude);
                prelude.clear();
                if head.starts_with('@') && is_nesting_at_rule(&head) {
                    at_context.push(head.trim_start_matches('@').trim().to_string());
                    continue;
                }
                // A declaration block: consume to its matching close brace.
                let mut body = String::new();
                let mut depth = 1usize;
                for c2 in chars.by_ref() {
                    match c2 {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    body.push(c2);
                }
                rules.push(Rule {
                    at_context: at_context.clone(),
                    selectors: head
                        .split(',')
                        .map(normalize_whitespace)
                        .filter(|s| !s.is_empty())
                        .collect(),
                    declarations: parse_declarations(&body),
                });
            }
            '}' => {
                at_context.pop();
                prelude.clear();
            }
            // A statement at-rule such as `@import …;` — nothing to record.
            ';' => prelude.clear(),
            _ => prelude.push(c),
        }
    }
    rules
}

/// Every rule that lists `selector` verbatim among its selectors, in source order.
pub fn rules_for_selector<'a>(rules: &'a [Rule], selector: &str) -> Vec<&'a Rule> {
    rules
        .iter()
        .filter(|r| r.selectors.iter().any(|s| s == selector))
        .collect()
}

/// A CSS length in CSS pixels, for the two units this crate's stylesheet uses.
///
/// `px` is exact. `rem` is resolved against the browser default root font size of 16 px —
/// `styles.css` sets no `font-size` on `:root` or `html`, so that is the value in play
/// unless the *user* has changed their browser default, which is precisely a thing this
/// code cannot see. Everything else (`em`, `%`, `auto`, `calc(…)`, keywords) returns
/// `None`, because a number that depends on an ancestor or on the viewport is not a
/// guarantee about this element.
pub const ROOT_FONT_SIZE_PX: f64 = 16.0;

pub fn length_px(value: &str) -> Option<f64> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse::<f64>().ok();
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f64>().ok().map(|n| n * ROOT_FONT_SIZE_PX);
    }
    // A bare `0` is a valid length in CSS.
    if v == "0" {
        return Some(0.0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(property: &str, value: &str, important: bool) -> Declaration {
        Declaration {
            property: property.to_string(),
            value: value.to_string(),
            important,
        }
    }

    #[test]
    fn strips_comments_including_multiline_ones() {
        assert_eq!(strip_comments("a /* x */ b"), "a  b");
        assert_eq!(strip_comments("a /* x\ny */ b"), "a  b");
        assert_eq!(strip_comments("a /* unterminated"), "a ");
        assert_eq!(strip_comments("no comment"), "no comment");
    }

    #[test]
    fn a_comment_cannot_smuggle_a_rule_past_the_parser() {
        // The point of stripping first: a commented-out hover rule must not be read as
        // a real one, or the focus-parity tripwire would demand a twin for a rule that
        // does not exist.
        let rules = parse("/* .ghost:hover { color: red; } */ .real { color: blue; }");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].selectors, vec![".real".to_string()]);
    }

    #[test]
    fn parses_selector_lists_and_declarations() {
        let rules = parse(".a, .b > .c { color: red; background: blue }");
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].selectors,
            vec![".a".to_string(), ".b > .c".to_string()]
        );
        assert_eq!(
            rules[0].declarations,
            vec![decl("color", "red", false), decl("background", "blue", false)]
        );
        assert_eq!(rules[0].at_context, Vec::<String>::new());
    }

    #[test]
    fn collapses_whitespace_in_selectors_and_values() {
        let rules = parse(".a   .b\n  .c {\n  border-color:   var(--accent)  ;\n}");
        assert_eq!(rules[0].selectors, vec![".a .b .c".to_string()]);
        assert_eq!(rules[0].declarations[0].value, "var(--accent)");
    }

    #[test]
    fn records_important_separately_from_the_value() {
        let rules = parse(".a { display: none !important; color: red; }");
        assert_eq!(
            rules[0].declarations,
            vec![decl("display", "none", true), decl("color", "red", false)]
        );
    }

    #[test]
    fn media_blocks_become_at_context_and_then_unwind() {
        let rules = parse("@media print { .a { color: red; } } .b { color: blue; }");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].at_context, vec!["media print".to_string()]);
        assert_eq!(rules[0].selectors, vec![".a".to_string()]);
        assert_eq!(rules[1].at_context, Vec::<String>::new());
        assert_eq!(rules[1].selectors, vec![".b".to_string()]);
    }

    #[test]
    fn a_media_query_with_a_colon_is_context_not_a_declaration() {
        let rules = parse("@media (prefers-reduced-motion: reduce) { * { color: red; } }");
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].at_context,
            vec!["media (prefers-reduced-motion: reduce)".to_string()]
        );
        assert_eq!(rules[0].selectors, vec!["*".to_string()]);
    }

    #[test]
    fn font_face_is_a_rule_not_a_nesting_context() {
        let rules = parse("@font-face { font-family: \"X\"; } .a { color: red; }");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].selectors, vec!["@font-face".to_string()]);
        assert_eq!(rules[1].at_context, Vec::<String>::new());
    }

    #[test]
    fn rules_for_selector_matches_verbatim_only() {
        let rules = parse(".a { color: red; } .a.b { color: blue; } .c, .a { color: green; }");
        let found = rules_for_selector(&rules, ".a");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].value_of("color").unwrap().value, "red");
        assert_eq!(found[1].value_of("color").unwrap().value, "green");
        assert!(rules_for_selector(&rules, ".missing").is_empty());
    }

    #[test]
    fn value_of_takes_the_last_declaration_like_the_cascade() {
        let rules = parse(".a { height: 10px; height: 20px; }");
        assert_eq!(rules[0].value_of("height").unwrap().value, "20px");
        assert!(rules[0].value_of("width").is_none());
    }

    #[test]
    fn properties_lists_declared_property_names_in_order() {
        let rules = parse(".a { opacity: 1; border-color: red; }");
        assert_eq!(rules[0].properties(), vec!["opacity", "border-color"]);
    }

    #[test]
    fn length_px_understands_px_rem_and_zero_only() {
        assert_eq!(length_px("44px"), Some(44.0));
        assert_eq!(length_px(" 0.4rem "), Some(6.4));
        assert_eq!(length_px("2.75rem"), Some(44.0));
        assert_eq!(length_px("0"), Some(0.0));

        // Relative to something this code cannot see, so: not a guarantee.
        assert_eq!(length_px("100%"), None);
        assert_eq!(length_px("2em"), None);
        assert_eq!(length_px("auto"), None);
        assert_eq!(length_px("calc(100% - 4px)"), None);
        assert_eq!(length_px("inherit"), None);
    }
}
