//! Ranked metadata queries with literal-only terminal screen evidence.

use crate::{
    model::{Session, Workspace, clean_text},
    provider,
    search::{ContentDocument, SearchMatch, find_matches_terms, normalize},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Any,
    Agent,
    Status,
    Workspace,
    Path,
    Title,
    Screen,
}

#[derive(Clone, Debug)]
struct Term {
    field: Field,
    value: String,
    excluded: bool,
    literal: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Clone, Debug, Default)]
pub struct RankedMatch {
    pub score: i64,
    pub reason: String,
    pub screen: SearchMatch,
}

struct MetadataField {
    field: Field,
    label: &'static str,
    text: String,
    weight: i64,
}

#[derive(Default)]
struct Token {
    text: String,
    quoted: bool,
    excluded: bool,
    field_boundary: Option<usize>,
}

impl Query {
    pub fn parse(input: &str) -> Result<Self, String> {
        let cleaned = clean_text(input, true);
        let mut tokens = Vec::new();
        let mut token = Token::default();
        let mut quoted = false;
        for ch in cleaned.chars() {
            match ch {
                '"' => {
                    quoted = !quoted;
                    token.quoted = true;
                }
                ch if ch.is_whitespace() && !quoted => {
                    if !token.text.is_empty() || token.quoted {
                        tokens.push(std::mem::take(&mut token));
                    }
                }
                _ => {
                    if !quoted && !token.quoted {
                        if ch == '!' && token.text.is_empty() {
                            token.excluded = true;
                        }
                        if ch == ':' && token.field_boundary.is_none() {
                            token.field_boundary = Some(token.text.len());
                        }
                    }
                    token.text.push(ch);
                }
            }
        }
        if quoted {
            return Err("Close the double quote to search a literal phrase.".into());
        }
        if !token.text.is_empty() || token.quoted {
            tokens.push(token);
        }
        let mut terms = Vec::new();
        for token in tokens {
            let prefix = usize::from(token.excluded);
            let text = &token.text[prefix..];
            let field_parts = token.field_boundary.map(|boundary| {
                let boundary = boundary - prefix;
                (&text[..boundary], &text[boundary + 1..])
            });
            let (field, value, unknown_field) = match field_parts {
                Some(("agent", value)) => (Field::Agent, value, false),
                Some(("status", value)) => (Field::Status, value, false),
                Some(("ws", value)) => (Field::Workspace, value, false),
                Some(("path", value)) => (Field::Path, value, false),
                Some(("title", value)) => (Field::Title, value, false),
                Some(("screen", value)) => (Field::Screen, value, false),
                Some(_) => (Field::Any, text, true),
                None => (Field::Any, text, false),
            };
            let value = normalize(value);
            if value.is_empty() {
                return Err("Search terms and field values cannot be empty.".into());
            }
            terms.push(Term {
                field,
                value,
                excluded: token.excluded,
                literal: token.quoted || token.excluded || unknown_field || field != Field::Any,
            });
        }
        Ok(Self { terms })
    }

    pub fn positive_terms(&self) -> Vec<String> {
        let mut result = Vec::new();
        for term in &self.terms {
            if !term.excluded && !result.contains(&term.value) {
                result.push(term.value.clone());
            }
        }
        result
    }

    pub fn fuzzy_terms(&self) -> Vec<String> {
        self.terms
            .iter()
            .filter(|term| !term.excluded && !term.literal && term.field == Field::Any)
            .map(|term| term.value.clone())
            .collect()
    }

    pub fn match_session(
        &self,
        session: &Session,
        workspace: &Workspace,
        tab_label: &str,
        body: Option<&ContentDocument>,
    ) -> Option<RankedMatch> {
        if self.terms.is_empty() {
            return Some(RankedMatch {
                reason: "All sessions".into(),
                ..RankedMatch::default()
            });
        }
        let mut metadata = vec![
            (Field::Title, "Title", session.title.as_str(), 100),
            (Field::Any, "Name", session.name.as_str(), 90),
            (Field::Path, "Path", session.cwd.as_str(), 60),
            (Field::Workspace, "Workspace", workspace.label.as_str(), 50),
            (Field::Any, "Tab", tab_label, 40),
            (Field::Agent, "Agent", session.kind.as_str(), 30),
            (Field::Agent, "Agent", session.display_kind.as_str(), 30),
            (Field::Status, "Status", session.status.as_str(), 20),
        ];
        metadata.extend(
            session
                .search_titles
                .iter()
                .map(|title| (Field::Title, "Title", title.as_str(), 80)),
        );
        if let Some(provider) = provider::lookup(&session.kind) {
            metadata.extend([
                (Field::Agent, "Agent", provider.kind, 30),
                (Field::Agent, "Agent", provider.label, 30),
            ]);
            metadata.extend(
                provider
                    .aliases
                    .iter()
                    .map(|alias| (Field::Agent, "Agent", *alias, 30)),
            );
        }
        self.match_fields(metadata, body)
    }

    pub fn needs_screens(&self) -> bool {
        self.terms
            .iter()
            .any(|term| matches!(term.field, Field::Any | Field::Screen))
    }

    pub(crate) fn match_group(
        &self,
        workspace: &Workspace,
        tab_label: Option<&str>,
    ) -> Option<i64> {
        if self
            .terms
            .iter()
            .any(|term| !matches!(term.field, Field::Any | Field::Workspace))
        {
            return None;
        }
        let mut metadata = vec![(Field::Workspace, "Workspace", workspace.label.as_str(), 50)];
        if let Some(label) = tab_label {
            metadata.push((Field::Any, "Tab", label, 40));
        }
        self.match_fields(metadata, None).map(|found| found.score)
    }

    fn match_fields<'a>(
        &self,
        metadata: impl IntoIterator<Item = (Field, &'static str, &'a str, i64)>,
        body: Option<&ContentDocument>,
    ) -> Option<RankedMatch> {
        let metadata: Vec<_> = metadata
            .into_iter()
            .filter(|(_, _, text, _)| !text.is_empty())
            .map(|(field, label, text, weight)| MetadataField {
                field,
                label,
                text: normalize(text),
                weight,
            })
            .collect();
        let mut result = RankedMatch::default();
        let mut reasons = Vec::new();
        let mut screen_terms = Vec::new();
        for term in &self.terms {
            let metadata_hit = metadata
                .iter()
                .filter(|item| term.field == Field::Any || term.field == item.field)
                .filter_map(|item| {
                    let fuzzy = !term.literal
                        && matches!(item.field, Field::Any | Field::Title | Field::Workspace);
                    score_metadata(&item.text, &term.value, fuzzy)
                        .map(|(score, method)| (score + item.weight, item.label, method))
                })
                .max_by_key(|(score, _, _)| *score);
            let uses_screen = matches!(term.field, Field::Any | Field::Screen);
            // Metadata matches must not hide literal evidence from the terminal screen.
            let screen_hit = uses_screen
                && body.is_some_and(|document| document.normalized.contains(&term.value));
            if term.excluded {
                if metadata_hit.is_some() || screen_hit {
                    return None;
                }
                // An unread screen cannot prove that a screen exclusion is satisfied.
                if uses_screen && body.is_none() {
                    return None;
                }
                continue;
            }
            let screen_score = if screen_hit { 800 } else { 0 };
            if metadata_hit.is_none() && !screen_hit {
                return None;
            }
            let metadata_score = metadata_hit.map_or(0, |(score, _, _)| score);
            result.score = result
                .score
                .saturating_add(metadata_score.max(screen_score));
            if screen_hit {
                screen_terms.push(term.value.clone());
            }
            let metadata_reason =
                metadata_hit.map(|(_, label, method)| format!("{label} ({method})"));
            let screen_reason = screen_hit.then(|| "Screen (literal)".to_owned());
            let sources = if screen_score > metadata_score {
                [screen_reason, metadata_reason]
            } else {
                [metadata_reason, screen_reason]
            };
            for reason in sources.into_iter().flatten() {
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
        }
        if !screen_terms.is_empty() {
            result.screen = find_matches_terms(&screen_terms, &ContentDocument::default(), body?)?;
        }
        result.reason = if reasons.is_empty() {
            "Exclusion filters".into()
        } else {
            reasons.join(", ")
        };
        Some(result)
    }
}

fn score_metadata(text: &str, term: &str, fuzzy: bool) -> Option<(i64, &'static str)> {
    if text == term {
        return Some((1400, "exact"));
    }
    if text.starts_with(term) {
        return Some((1200, "prefix"));
    }
    if text.contains(term) {
        return Some((1000, "literal"));
    }
    if !fuzzy {
        return None;
    }
    let mut wanted = term.chars().peekable();
    let mut score = 0_i64;
    let mut previous_match = false;
    let mut previous_character = None;
    for ch in text.chars() {
        if wanted.peek() == Some(&ch) {
            wanted.next();
            score += if previous_match { 4 } else { 1 };
            if previous_character.is_none_or(|previous: char| !previous.is_alphanumeric()) {
                score += 3;
            }
            if wanted.peek().is_none() {
                return Some((100 + score.min(100), "fuzzy"));
            }
            previous_match = true;
        } else {
            previous_match = false;
        }
        previous_character = Some(ch);
    }
    None
}
