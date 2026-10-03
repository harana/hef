//! Holds a whole column of text in one shared buffer instead of a separate heap string per row.
//!
//! See: hef-encodings-and-compression/spec.md

use std::fmt;

/// A column of text values kept as one contiguous character buffer plus a per-row span into it, so a column of a
/// million rows costs a handful of allocations rather than one per row.
///
/// A value may be absent (the row carries no text at all). An absent value contributes no characters, and is told
/// apart from a present empty string by its own presence flag — never by its length, which is zero for both.
///
/// See: hef-encodings-and-compression/spec.md
#[derive(Clone, PartialEq, Eq)]
pub struct StringColumn {
    /// `offsets[i]..offsets[i + 1]` spans value `i` in `text`; always one longer than the value count, and always
    /// opens with a leading `0`.
    offsets: Vec<u64>,
    present: Vec<bool>,
    text: String,
}

impl StringColumn {
    /// An empty column, ready to be filled row by row.
    pub fn new() -> Self {
        Self {
            offsets: vec![0],
            present: Vec::new(),
            text: String::new(),
        }
    }

    /// An empty column with room for `values` rows totalling `text_bytes` characters, so a bulk fill reallocates
    /// nothing.
    pub fn with_capacity(values: usize, text_bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(values + 1);
        offsets.push(0);
        Self {
            offsets,
            present: Vec::with_capacity(values),
            text: String::with_capacity(text_bytes),
        }
    }

    /// Appends one row — its text, or `None` for a row that carries no value. The text is copied into the shared
    /// buffer, so the caller's own copy can be dropped straight away.
    pub fn push(&mut self, value: Option<&str>) {
        if let Some(text) = value {
            self.text.push_str(text);
        }
        self.offsets.push(self.text.len() as u64);
        self.present.push(value.is_some());
    }

    /// How many rows the column holds.
    pub fn len(&self) -> usize {
        self.present.len()
    }

    /// Whether the column holds no rows at all.
    pub fn is_empty(&self) -> bool {
        self.present.is_empty()
    }

    /// The row at `index`: `None` past the last row, `Some(None)` for a row that carries no value.
    pub fn get(&self, index: usize) -> Option<Option<&str>> {
        if !self.present.get(index).copied()? {
            return Some(None);
        }
        Some(self.span(index))
    }

    /// Every row in order, absent rows included.
    ///
    /// Walks `present` and consecutive pairs of `offsets` together instead of looking each row's index up in both
    /// (as [`get`](Self::get) does): `present.len() == offsets.len() - 1` always, so a zipped walk needs no
    /// per-row bounds check to line the two up.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<&str>> + '_ {
        self.present
            .iter()
            .zip(self.offsets.windows(2))
            .map(move |(&present, span)| {
                let [start, end] = span else { return None };
                present.then(|| self.text.get(*start as usize..*end as usize)).flatten()
            })
    }

    /// Only the rows that carry a value, in order — the dense value list every string encoding stores.
    pub fn iter_present(&self) -> impl Iterator<Item = &str> + '_ {
        self.iter().flatten()
    }

    /// How many rows carry no value.
    pub fn null_count(&self) -> usize {
        self.present.iter().filter(|present| !**present).count()
    }

    /// Total characters across every present row — the column's stored text, excluding the spans and flags.
    pub fn text_len(&self) -> usize {
        self.text.len()
    }

    /// Rows `[start, end)` as a column of their own, with `end` clamped to the row count. Copies the exact text span
    /// the rows occupy in one pass and rebases their offsets onto it, rather than replaying each row through
    /// [`push`](Self::push).
    pub fn slice(&self, start: usize, end: usize) -> Self {
        let end = end.min(self.len());
        if start >= end {
            return Self::new();
        }
        let Some(offsets) = self.offsets.get(start..=end) else {
            return Self::new();
        };
        let Some(&text_start) = offsets.first() else {
            return Self::new();
        };
        let text_end = offsets.last().copied().unwrap_or(text_start);
        let text = self
            .text
            .get(text_start as usize..text_end as usize)
            .unwrap_or("")
            .to_owned();
        let offsets = offsets.iter().map(|offset| offset - text_start).collect();
        let present = self.present.get(start..end).unwrap_or(&[]).to_vec();
        Self { offsets, present, text }
    }

    /// Appends every row of `other` after this column's own rows. Copies `other`'s text in one bulk append and
    /// rebases its offsets onto that, rather than replaying each row through [`push`](Self::push).
    pub fn append(&mut self, other: &Self) {
        if other.is_empty() {
            return;
        }
        self.offsets.reserve(other.len());
        self.present.reserve(other.len());
        self.text.reserve(other.text_len());
        let base = self.text.len() as u64;
        self.text.push_str(&other.text);
        self.offsets
            .extend(other.offsets.get(1..).unwrap_or(&[]).iter().map(|offset| offset + base));
        self.present.extend_from_slice(&other.present);
    }

    fn span(&self, index: usize) -> Option<&str> {
        let start = self.offsets.get(index).copied()? as usize;
        let end = self.offsets.get(index + 1).copied()? as usize;
        self.text.get(start..end)
    }
}

impl Default for StringColumn {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for StringColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: AsRef<str>> FromIterator<Option<T>> for StringColumn {
    fn from_iter<I: IntoIterator<Item = Option<T>>>(values: I) -> Self {
        let values = values.into_iter();
        let mut column = Self::with_capacity(values.size_hint().0, 0);
        for value in values {
            column.push(value.as_ref().map(AsRef::as_ref));
        }
        column
    }
}

impl<T: AsRef<str>> From<Vec<Option<T>>> for StringColumn {
    fn from(values: Vec<Option<T>>) -> Self {
        values.into_iter().collect()
    }
}

#[cfg(test)]
#[path = "test/string_column.rs"]
mod tests;
