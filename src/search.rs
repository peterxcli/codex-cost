use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use fst::automaton::Str;
use fst::{Automaton, IntoStreamer, Set, SetBuilder, Streamer};

use crate::models::Session;

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchIndex {
    pub(crate) doc_count: usize,
    pub(crate) terms: Option<Set<Vec<u8>>>,
    pub(crate) postings: BTreeMap<String, Vec<usize>>,
}
impl SearchIndex {
    #[cfg(test)]
    pub(crate) fn build<F>(sessions: &[Session], mut progress: F) -> Self
    where
        F: FnMut(usize, usize),
    {
        let total = sessions.len();
        let mut postings: BTreeMap<String, Vec<usize>> = BTreeMap::new();

        for (idx, session) in sessions.iter().enumerate() {
            for token in session_terms(session) {
                postings.entry(token).or_default().push(idx);
            }
            progress(idx + 1, total);
        }

        Self::from_postings(total, postings)
    }

    pub(crate) fn from_postings(doc_count: usize, postings: BTreeMap<String, Vec<usize>>) -> Self {
        let terms = build_fst_bytes(postings.keys()).ok().and_then(|bytes| {
            if bytes.is_empty() {
                None
            } else {
                Set::new(bytes).ok()
            }
        });
        Self {
            doc_count,
            terms,
            postings,
        }
    }

    pub(crate) fn from_persisted(
        doc_count: usize,
        postings: BTreeMap<String, Vec<usize>>,
        fst_bytes: Vec<u8>,
    ) -> Self {
        let terms = if fst_bytes.is_empty() {
            None
        } else {
            Set::new(fst_bytes).ok()
        };
        Self {
            doc_count,
            terms,
            postings,
        }
    }

    pub(crate) fn search(&self, query: &str) -> Vec<usize> {
        let terms = unique_search_terms(&query.trim().to_lowercase());
        if terms.is_empty() {
            return (0..self.doc_count).collect();
        }

        let mut posting_lists = Vec::with_capacity(terms.len());
        for term in &terms {
            let postings = self.postings_for_prefix(term);
            if postings.is_empty() {
                return Vec::new();
            }
            posting_lists.push(postings);
        }

        posting_lists.sort_by_key(Vec::len);
        let mut candidates = posting_lists.remove(0);
        for postings in posting_lists {
            candidates = intersect_sorted(&candidates, &postings);
            if candidates.is_empty() {
                return Vec::new();
            }
        }
        candidates
    }

    pub(crate) fn postings_for_prefix(&self, prefix: &str) -> Vec<usize> {
        let mut docs = BTreeSet::new();

        if let Some(terms) = &self.terms {
            let matcher = Str::new(prefix).starts_with();
            let mut stream = terms.search(matcher).into_stream();
            while let Some(key) = stream.next() {
                if let Ok(token) = std::str::from_utf8(key) {
                    if let Some(postings) = self.postings.get(token) {
                        docs.extend(postings.iter().copied());
                    }
                }
            }
            return docs.into_iter().collect();
        }

        for (token, postings) in self.postings.range(prefix.to_string()..) {
            if !token.starts_with(prefix) {
                break;
            }
            docs.extend(postings.iter().copied());
        }
        docs.into_iter().collect()
    }
}

pub(crate) fn build_fst_bytes<'a, I>(terms: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = &'a String>,
{
    let mut builder = SetBuilder::memory();
    for term in terms {
        builder.insert(term.as_str())?;
    }
    Ok(builder.into_inner()?)
}

pub(crate) fn indexed_text_lower(session: &Session) -> String {
    let mut text = String::with_capacity(
        session.id.len()
            + session.path.to_string_lossy().len()
            + session
                .first_user_message
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + session
                .final_assistant_message
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + session
                .search_messages
                .iter()
                .map(String::len)
                .sum::<usize>()
            + session
                .goal
                .objective
                .as_deref()
                .map(str::len)
                .unwrap_or_default()
            + 128,
    );
    push_lower_field(&mut text, &session.id);
    push_lower_field(&mut text, &session.path.display().to_string());
    push_optional_lower_field(&mut text, session.model.as_deref());
    push_optional_lower_field(&mut text, session.model_provider.as_deref());
    push_optional_lower_field(&mut text, session.cwd.as_deref());
    push_optional_lower_field(&mut text, session.first_user_message.as_deref());
    push_optional_lower_field(&mut text, session.final_assistant_message.as_deref());
    for message in &session.search_messages {
        push_lower_field(&mut text, message);
    }
    push_optional_lower_field(&mut text, session.goal.objective.as_deref());
    push_optional_lower_field(&mut text, session.goal.status.as_deref());
    for error in &session.parse_errors {
        push_lower_field(&mut text, error);
    }
    text
}

pub(crate) fn session_terms(session: &Session) -> Vec<String> {
    indexed_text_lower(session)
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn push_optional_lower_field(out: &mut String, value: Option<&str>) {
    if let Some(value) = value {
        push_lower_field(out, value);
    }
}

pub(crate) fn push_lower_field(out: &mut String, value: &str) {
    out.push(' ');
    out.push_str(&value.to_lowercase());
}

pub(crate) fn unique_search_terms(text: &str) -> Vec<String> {
    search_terms(text)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn search_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        terms.push(current);
    }

    terms
}

pub(crate) fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut left_idx = 0;
    let mut right_idx = 0;

    while left_idx < left.len() && right_idx < right.len() {
        match left[left_idx].cmp(&right[right_idx]) {
            std::cmp::Ordering::Less => left_idx += 1,
            std::cmp::Ordering::Greater => right_idx += 1,
            std::cmp::Ordering::Equal => {
                out.push(left[left_idx]);
                left_idx += 1;
                right_idx += 1;
            }
        }
    }

    out
}
