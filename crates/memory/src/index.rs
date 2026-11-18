//! The memory index an agent session starts with: one line per memory
//! (id, title, and its one-line hook), bounded in lines and bytes, so the
//! agent knows what it can recall without reading every memory.

use crate::Memory;

/// Memories the index lists at most.
pub const INDEX_LINES: usize = 40;
/// The index's size at most, in bytes.
pub const INDEX_BYTES: usize = 4096;
/// One line's size at most, in characters.
const LINE_CHARS: usize = 160;

/// What the index says before its lines.
pub const INDEX_HEAD: &str = "Dreamer memory: notes kept from earlier sessions for this user, \
this host, this project, and this session (newest first: id, title, and what it is about). \
Read one in full with memory_recall, and keep a fact worth having in a later session with \
memory_remember (from a shell: `dream memory recall WORDS`, `dream memory remember`).";

fn line(m: &Memory) -> String {
    let title = m.title.trim();
    let hook = m.description.trim();
    let l = if hook.is_empty() || hook == title {
        format!("- {}: {title}", m.id)
    } else {
        format!("- {}: {title} — {hook}", m.id)
    };
    let l = l.replace(['\n', '\r'], " ");
    if l.chars().count() > LINE_CHARS {
        l.chars().take(LINE_CHARS - 1).collect::<String>() + "…"
    } else {
        l
    }
}

/// The index of `items` (in the order given, newest first), or `None` when
/// there are none. At most [`INDEX_LINES`] lines and [`INDEX_BYTES`]
/// bytes; the rest are counted in a last line.
pub fn index(items: &[Memory]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from(INDEX_HEAD);
    let mut listed = 0;
    for m in items.iter().take(INDEX_LINES) {
        let l = line(m);
        // Room is kept for the last line.
        if out.len() + 1 + l.len() > INDEX_BYTES - 80 {
            break;
        }
        out.push('\n');
        out.push_str(&l);
        listed += 1;
    }
    let rest = items.len() - listed;
    if rest > 0 {
        out.push_str(&format!(
            "\n({rest} more not listed; memory_recall finds them.)"
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Scope};

    fn mem(i: usize, title: &str, hook: &str) -> Memory {
        Memory {
            id: format!("project/m{i}"),
            scope: Scope::Project,
            kind: Kind::Project,
            title: title.into(),
            description: hook.into(),
            text: String::new(),
            links: Vec::new(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            path: None,
            score: None,
        }
    }

    #[test]
    fn the_index_is_bounded_and_counts_the_rest() {
        assert_eq!(index(&[]), None);
        let one = index(&[mem(1, "Port", "Staging listens on 8461")]).expect("index");
        assert!(
            one.ends_with("\n- project/m1: Port — Staging listens on 8461"),
            "{one}"
        );
        let same = index(&[mem(1, "Port", "Port")]).expect("index");
        assert!(same.ends_with("\n- project/m1: Port"), "{same}");

        let many: Vec<Memory> = (0..100).map(|i| mem(i, "T", "h")).collect();
        let text = index(&many).expect("index");
        assert_eq!(
            text.lines().filter(|l| l.starts_with("- ")).count(),
            INDEX_LINES
        );
        assert!(
            text.ends_with("(60 more not listed; memory_recall finds them.)"),
            "{text}"
        );

        let long: Vec<Memory> = (0..40).map(|i| mem(i, &"x".repeat(500), "h")).collect();
        let text = index(&long).expect("index");
        assert!(text.len() <= INDEX_BYTES, "{}", text.len());
        assert!(
            text.lines()
                .filter(|l| l.starts_with("- "))
                .all(|l| l.chars().count() <= LINE_CHARS)
        );
        assert!(text.contains("more not listed"));
    }
}
