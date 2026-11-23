use std::cmp::Ordering;
use std::ops::Range;

use unicode_normalization::char::{canonical_combining_class, decompose_compatible};

use super::catalog::{NameCatalog, NameTarget};
use crate::resolve::normalize_key;

const MIN_EXACT_NAME_CHARS: usize = 2;
const MIN_TYPO_NAME_CHARS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionMatch {
    Exact,
    Typo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameMention {
    pub(crate) surface: String,
    pub(crate) key: String,
    pub(crate) matched: MentionMatch,
    pub(crate) targets: Vec<NameTarget>,
    span: Range<usize>,
}

impl NameMention {
    pub(crate) fn single_target(&self) -> Option<&NameTarget> {
        match self.targets.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

struct NormalizedText {
    chars: Vec<char>,
    sources: Vec<Range<usize>>,
    separated: Vec<bool>,
}

struct Occurrence {
    entry: usize,
    span: Range<usize>,
    matched: MentionMatch,
}

pub(crate) fn name_chars(text: &str) -> Vec<char> {
    NormalizedText::new(text).chars
}

pub(crate) fn name_key(text: &str) -> String {
    name_chars(text).into_iter().collect()
}

pub(crate) fn find_mentions(text: &str, catalog: &NameCatalog) -> Vec<NameMention> {
    let normalized = NormalizedText::new(text);
    let occurrences = find_occurrences(&normalized, catalog);
    let exact = longest_exact_occurrences(&occurrences);
    let typos: Vec<&Occurrence> = occurrences
        .iter()
        .filter(|occurrence| occurrence.matched == MentionMatch::Typo)
        .filter(|occurrence| {
            !exact
                .iter()
                .any(|found| overlaps(&found.span, &occurrence.span))
        })
        .collect();
    let mut mentions = exact_mentions(text, &normalized, catalog, &exact);
    mentions.extend(typo_mentions(text, &normalized, catalog, typos));
    mentions.sort_by_key(|mention| (mention.span.start, mention.span.end));
    mentions
}

impl NormalizedText {
    fn new(text: &str) -> Self {
        let mut normalized = Self {
            chars: Vec::new(),
            sources: Vec::new(),
            separated: Vec::new(),
        };
        let mut separated = true;
        for cluster in clusters(text) {
            let key = normalize_key(&text[cluster.clone()]);
            if key.is_empty() {
                separated = true;
                continue;
            }
            for (offset, ch) in key.chars().enumerate() {
                normalized.chars.push(ch);
                normalized.sources.push(cluster.clone());
                normalized.separated.push(offset == 0 && separated);
            }
            separated = false;
        }
        normalized
    }

    fn starts(&self, len: usize) -> impl Iterator<Item = usize> {
        self.chars
            .len()
            .checked_sub(len)
            .map(|last| 0..=last)
            .into_iter()
            .flatten()
    }

    fn is_whole_word_span(&self, span: &Range<usize>) -> bool {
        !self.splits_ascii_word_at(span.start) && !self.splits_ascii_word_at(span.end)
    }

    fn splits_ascii_word_at(&self, at: usize) -> bool {
        at > 0
            && at < self.chars.len()
            && !self.separated[at]
            && self.chars[at - 1].is_ascii_alphanumeric()
            && self.chars[at].is_ascii_alphanumeric()
    }

    fn original<'a>(&self, text: &'a str, span: &Range<usize>) -> &'a str {
        &text[self.sources[span.start].start..self.sources[span.end - 1].end]
    }
}

fn clusters(text: &str) -> Vec<Range<usize>> {
    let mut clusters: Vec<Range<usize>> = Vec::new();
    for (start, ch) in text.char_indices() {
        let end = start + ch.len_utf8();
        match clusters.last_mut() {
            Some(last) if continues_cluster(ch) => last.end = end,
            _ => clusters.push(start..end),
        }
    }
    clusters
}

fn continues_cluster(ch: char) -> bool {
    let mut leading = None;
    decompose_compatible(ch, |part| {
        if leading.is_none() {
            leading = Some(part);
        }
    });
    leading.is_some_and(|part| canonical_combining_class(part) != 0)
}

fn find_occurrences(text: &NormalizedText, catalog: &NameCatalog) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for (entry, catalog_entry) in catalog.entries().iter().enumerate() {
        let name = catalog_entry.key.as_slice();
        if name.len() >= MIN_EXACT_NAME_CHARS {
            for start in text.starts(name.len()) {
                let span = start..start + name.len();
                if text.chars[span.clone()] == *name && text.is_whole_word_span(&span) {
                    found.push(Occurrence {
                        entry,
                        span,
                        matched: MentionMatch::Exact,
                    });
                }
            }
        }
        if name.len() >= MIN_TYPO_NAME_CHARS {
            for len in name.len() - 1..=name.len() + 1 {
                for start in text.starts(len) {
                    let span = start..start + len;
                    if text.is_whole_word_span(&span)
                        && is_single_typo(&text.chars[span.clone()], name)
                    {
                        found.push(Occurrence {
                            entry,
                            span,
                            matched: MentionMatch::Typo,
                        });
                    }
                }
            }
        }
    }
    found
}

fn is_single_typo(written: &[char], name: &[char]) -> bool {
    let common = written
        .iter()
        .zip(name)
        .take_while(|(written, name)| written == name)
        .count();
    match written.len().cmp(&name.len()) {
        Ordering::Equal => {
            common < written.len()
                && (written[common + 1..] == name[common + 1..]
                    || is_adjacent_swap(written, name, common))
        }
        Ordering::Greater => {
            written.len() == name.len() + 1 && written[common + 1..] == name[common..]
        }
        Ordering::Less => {
            written.len() + 1 == name.len() && written[common..] == name[common + 1..]
        }
    }
}

fn is_adjacent_swap(written: &[char], name: &[char], at: usize) -> bool {
    at + 1 < written.len()
        && written[at] == name[at + 1]
        && written[at + 1] == name[at]
        && written[at + 2..] == name[at + 2..]
}

fn longest_exact_occurrences(occurrences: &[Occurrence]) -> Vec<&Occurrence> {
    let exact: Vec<&Occurrence> = occurrences
        .iter()
        .filter(|occurrence| occurrence.matched == MentionMatch::Exact)
        .collect();
    exact
        .iter()
        .copied()
        .filter(|occurrence| {
            !exact
                .iter()
                .any(|other| contains_longer(&other.span, &occurrence.span))
        })
        .collect()
}

fn contains_longer(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && inner.end <= outer.end && outer.len() > inner.len()
}

fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn exact_mentions(
    text: &str,
    normalized: &NormalizedText,
    catalog: &NameCatalog,
    exact: &[&Occurrence],
) -> Vec<NameMention> {
    let mut mentions: Vec<NameMention> = Vec::new();
    for occurrence in exact {
        let target = &catalog.entries()[occurrence.entry].target;
        match mentions
            .iter_mut()
            .find(|mention| mention.span == occurrence.span)
        {
            Some(mention) => add_target(mention, target),
            None => mentions.push(mention_at(
                text,
                normalized,
                occurrence.span.clone(),
                MentionMatch::Exact,
                target,
            )),
        }
    }
    mentions
}

fn typo_mentions(
    text: &str,
    normalized: &NormalizedText,
    catalog: &NameCatalog,
    mut typos: Vec<&Occurrence>,
) -> Vec<NameMention> {
    typos.sort_by_key(|occurrence| (occurrence.span.start, occurrence.span.end));
    let mut groups: Vec<(Range<usize>, usize, Vec<&Occurrence>)> = Vec::new();
    for occurrence in typos {
        match groups.last_mut() {
            Some((shortest, group_end, members)) if occurrence.span.start < *group_end => {
                if occurrence.span.len() < shortest.len() {
                    *shortest = occurrence.span.clone();
                }
                *group_end = (*group_end).max(occurrence.span.end);
                members.push(occurrence);
            }
            _ => groups.push((
                occurrence.span.clone(),
                occurrence.span.end,
                vec![occurrence],
            )),
        }
    }
    groups
        .into_iter()
        .map(|(shortest, _, members)| {
            let mut mention = NameMention {
                surface: normalized.original(text, &shortest).to_string(),
                key: normalized.chars[shortest.clone()].iter().collect(),
                matched: MentionMatch::Typo,
                targets: Vec::new(),
                span: shortest,
            };
            for member in members {
                add_target(&mut mention, &catalog.entries()[member.entry].target);
            }
            mention
        })
        .collect()
}

fn mention_at(
    text: &str,
    normalized: &NormalizedText,
    span: Range<usize>,
    matched: MentionMatch,
    target: &NameTarget,
) -> NameMention {
    NameMention {
        surface: normalized.original(text, &span).to_string(),
        key: normalized.chars[span.clone()].iter().collect(),
        matched,
        targets: vec![target.clone()],
        span,
    }
}

fn add_target(mention: &mut NameMention, target: &NameTarget) {
    let known = mention
        .targets
        .iter()
        .any(|existing| existing.kind == target.kind && existing.id == target.id);
    if !known {
        mention.targets.push(target.clone());
    }
}

#[cfg(test)]
mod tests;
