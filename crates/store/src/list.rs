//! S3 ListObjects semantics on top of [`MetadataStore::list_objects`].

use crate::{
    error::Result,
    meta::{MetadataStore, ObjectRecord},
};

/// Appended to a common prefix to form a cursor that sorts after every key under that prefix
/// (U+10FFFF is the largest code point, so its UTF-8 encoding sorts last).
pub(crate) const PREFIX_SKIP: char = '\u{10FFFF}';
const BATCH: u32 = 1000;
pub const MAX_KEYS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListStart {
    Beginning,
    /// A key or common prefix; listing starts strictly after it (V1 `marker`, V2 `start-after`).
    Marker(String),
    /// A resume point previously returned as [`ListPage::next_cursor`] (V2 continuation token).
    Cursor(String),
}

#[derive(Debug, Clone)]
pub struct ListParams {
    pub bucket: String,
    pub prefix: String,
    /// Empty delimiters are treated as absent.
    pub delimiter: Option<String>,
    pub start: ListStart,
    /// Capped at [`MAX_KEYS`].
    pub max_keys: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub objects: Vec<ObjectRecord>,
    pub common_prefixes: Vec<String>,
    pub truncated: bool,
    /// Last key or common prefix returned (V1 `NextMarker`).
    pub next_marker: Option<String>,
    /// Where the next page starts; set only when truncated (V2 continuation token).
    pub next_cursor: Option<String>,
}

/// The common prefix `key` rolls up into, if any.
fn common_prefix<'a>(key: &'a str, prefix: &str, delimiter: Option<&str>) -> Option<&'a str> {
    let d = delimiter?;
    let rest = key.strip_prefix(prefix)?;
    let pos = rest.find(d)?;
    Some(&key[..prefix.len() + pos + d.len()])
}

pub(crate) async fn list(meta: &dyn MetadataStore, p: &ListParams) -> Result<ListPage> {
    let delimiter = p.delimiter.as_deref().filter(|d| !d.is_empty());
    let max_keys = p.max_keys.min(MAX_KEYS);
    let mut page = ListPage::default();
    if max_keys == 0 {
        return Ok(page);
    }

    let mut cursor = match &p.start {
        ListStart::Beginning => String::new(),
        ListStart::Cursor(c) => c.clone(),
        ListStart::Marker(m) => match common_prefix(m, &p.prefix, delimiter) {
            // The marker is itself a common prefix: skip everything under it.
            Some(cp) if cp.len() == m.len() => format!("{m}{PREFIX_SKIP}"),
            _ => m.clone(),
        },
    };

    'fetch: loop {
        let batch = meta
            .list_objects(&p.bucket, &p.prefix, &cursor, BATCH)
            .await?;
        let exhausted = batch.len() < BATCH as usize;
        for obj in batch {
            if !obj.key.starts_with(&p.prefix) {
                // Keys are sorted, so nothing after this matches the prefix either.
                break 'fetch;
            }
            let entries = page.objects.len() + page.common_prefixes.len();
            if entries == max_keys {
                page.truncated = true;
                page.next_cursor = Some(cursor);
                break 'fetch;
            }
            if let Some(cp) = common_prefix(&obj.key, &p.prefix, delimiter) {
                let cp = cp.to_owned();
                cursor = format!("{cp}{PREFIX_SKIP}");
                page.next_marker = Some(cp.clone());
                page.common_prefixes.push(cp);
                // The rest of this batch may be under the same prefix; refetch past it.
                continue 'fetch;
            }
            cursor.clone_from(&obj.key);
            page.next_marker = Some(obj.key.clone());
            page.objects.push(obj);
        }
        if exhausted {
            break;
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::common_prefix;

    #[test]
    fn rolls_up_on_first_delimiter_after_prefix() {
        assert_eq!(common_prefix("a/b/c", "", Some("/")), Some("a/"));
        assert_eq!(common_prefix("a/b/c", "a/", Some("/")), Some("a/b/"));
        assert_eq!(common_prefix("a/b", "a/", Some("/")), None);
        assert_eq!(common_prefix("a/b", "", None), None);
        assert_eq!(common_prefix("x--y--z", "x", Some("--")), Some("x--"));
        assert_eq!(common_prefix("日本/語", "", Some("/")), Some("日本/"));
    }
}
