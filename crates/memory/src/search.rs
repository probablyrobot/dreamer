//! Full-text ranking without a model: the same query over the same
//! memories always gives the same order.
//!
//! Text is split into words (runs of letters and digits, lowercased). A
//! query word hits a memory word that equals it (2 points) or starts with
//! it (1 point), weighted by where the word is: title 4, description 2,
//! text and links 1. A memory that hits more distinct query words always
//! ranks above one that hits fewer; then more points; then the newer;
//! then the id.

use crate::Memory;

/// Lowercased words of `text`.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The score of `m` for the query words, or 0 when none hits.
pub fn score(terms: &[String], m: &Memory) -> u32 {
    let fields: [(u32, Vec<String>); 4] = [
        (4, words(&m.title)),
        (2, words(&m.description)),
        (1, words(&m.text)),
        (1, words(&m.links.join(" "))),
    ];
    let mut hit = 0u32;
    let mut points = 0u32;
    for t in terms {
        let mut p = 0u32;
        for (weight, ws) in &fields {
            for w in ws {
                if w == t {
                    p += 2 * weight;
                } else if w.starts_with(t.as_str()) {
                    p += weight;
                }
            }
        }
        if p > 0 {
            hit += 1;
            points += p;
        }
    }
    if hit == 0 {
        0
    } else {
        hit * 1000 + points.min(999)
    }
}

/// The best `limit` of `items` for `query`, each with its score. An empty
/// query keeps every item, newest first, without scores.
pub fn rank(query: &str, items: Vec<Memory>, limit: usize) -> Vec<Memory> {
    let mut terms = words(query);
    terms.sort();
    terms.dedup();
    let mut out: Vec<Memory> = if terms.is_empty() {
        items
    } else {
        items
            .into_iter()
            .filter_map(|mut m| {
                let s = score(&terms, &m);
                (s > 0).then(|| {
                    m.score = Some(s);
                    m
                })
            })
            .collect()
    };
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.updated_unix_ms.cmp(&a.updated_unix_ms))
            .then(a.id.cmp(&b.id))
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Scope};

    fn m(id: &str, title: &str, text: &str, updated: u64) -> Memory {
        Memory {
            id: id.into(),
            scope: Scope::User,
            kind: Kind::User,
            title: title.into(),
            description: String::new(),
            text: text.into(),
            links: vec![],
            created_unix_ms: 0,
            updated_unix_ms: updated,
            path: None,
            score: None,
        }
    }

    #[test]
    fn more_words_beat_more_points_and_ties_go_to_the_newer() {
        let items = vec![
            m("user/a", "tabs tabs tabs", "", 1),
            m("user/b", "", "tabs and spaces", 1),
            m("user/c", "", "tabs and spaces", 2),
            m("user/d", "", "nothing here", 3),
        ];
        let ids: Vec<String> = rank("Spaces TABS", items.clone(), 10)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["user/c", "user/b", "user/a"]);
        // A prefix hits too; an empty query lists the newest first.
        assert_eq!(rank("spa", items.clone(), 1)[0].id, "user/c");
        let all = rank("  ", items, 2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "user/d");
        assert_eq!(all[0].score, None);
    }
}
