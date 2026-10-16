use super::*;
use super::merges::{MAX_MERGE_EDITS_PER_SHEET, MergeIndex};

pub(super) const HYPERLINK_PREFIX: &str = "hyperlink:";
pub(super) type HyperlinkEdits = BTreeMap<String, Option<Hyperlink>>;

pub(super) fn insert_hyperlink_edit(
    edits: &mut HyperlinkEdits,
    key: &str,
    value: &Out,
) -> Result<(), String> {
    if edits.len() >= MAX_MERGE_EDITS_PER_SHEET {
        return Err("too many sheet hyperlink edits".to_string());
    }
    let range_key = key
        .strip_prefix(HYPERLINK_PREFIX)
        .ok_or_else(|| "invalid hyperlink edit prefix".to_string())?;
    if key.len() > 64 {
        return Err("hyperlink edit key is too long".to_string());
    }
    let range =
        CellRange::parse_a1(range_key).map_err(|_| format!("invalid hyperlink edit key {key}"))?;
    if range.to_a1() != range_key
        || range_key.contains('$')
        || range.start.row >= MAX_ROWS
        || range.end.row >= MAX_ROWS
        || range.start.col >= MAX_COLS
        || range.end.col >= MAX_COLS
    {
        return Err(format!("noncanonical or out of bounds hyperlink edit {key}"));
    }
    let link = match value {
        Out::Any(Any::Bool(false)) => None,
        Out::Any(Any::String(json)) => {
            let mut parsed = decode_hyperlinks(&format!("[{json}]"))?;
            if parsed.len() != 1 {
                return Err(format!("hyperlink edit {key} is not one link"));
            }
            let link = parsed.pop().unwrap();
            if link.range != range || link.range.to_a1() != range_key {
                return Err(format!("hyperlink edit {key} has a different range"));
            }
            Some(link)
        }
        _ => return Err(format!("hyperlink edit {key} is not a link or removal")),
    };
    edits.insert(range_key.to_string(), link);
    Ok(())
}

pub(super) fn clear_hyperlink_edits(sheet: &MapRef, txn: &mut TransactionMut<'_>) {
    clear_sheet_edits(sheet, txn, HYPERLINK_PREFIX);
}

pub(super) fn sync_hyperlinks(
    sheet: &MapRef,
    txn: &mut TransactionMut<'_>,
    model: &Sheet,
    collaborative: bool,
) -> Result<(), String> {
    if !collaborative {
        let json = serde_json::to_string(&model.hyperlinks)
            .map_err(|error| format!("cannot encode sheet hyperlinks: {error}"))?;
        sheet.try_update(txn, HYPERLINKS, json);
        clear_hyperlink_edits(sheet, txn);
        return Ok(());
    }
    let Some(Out::Any(Any::String(json))) = sheet.get(txn, HYPERLINKS) else {
        return Err("sheet is missing hyperlinks".to_string());
    };
    let baseline = decode_hyperlinks(&json)?
        .into_iter()
        .map(|link| (link.range.to_a1(), link))
        .collect::<BTreeMap<_, _>>();
    let current = model
        .hyperlinks
        .iter()
        .map(|link| (link.range.to_a1(), link))
        .collect::<BTreeMap<_, _>>();
    let mut keys = sheet
        .keys(txn)
        .filter_map(|key| key.strip_prefix(HYPERLINK_PREFIX).map(str::to_string))
        .collect::<BTreeSet<_>>();
    keys.extend(baseline.keys().cloned());
    keys.extend(current.keys().cloned());
    for key in keys {
        let edit_key = format!("{HYPERLINK_PREFIX}{key}");
        match (baseline.get(&key), current.get(&key).copied()) {
            (Some(original), Some(link)) if original == link => {
                sheet.remove(txn, &edit_key);
            }
            (_, Some(link)) => {
                let json = serde_json::to_string(link)
                    .map_err(|error| format!("cannot encode hyperlink: {error}"))?;
                sheet.try_update(txn, edit_key, json);
            }
            (Some(_), None) => {
                sheet.try_update(txn, edit_key, false);
            }
            (None, None) => {
                sheet.remove(txn, &edit_key);
            }
        }
    }
    Ok(())
}

pub(super) fn project_hyperlinks<T: ReadTxn>(
    baseline: Vec<Hyperlink>,
    sheet: &MapRef,
    txn: &T,
) -> Result<Vec<Hyperlink>, String> {
    let mut edits = HyperlinkEdits::new();
    for (key, value) in sheet.iter(txn) {
        if key.starts_with(HYPERLINK_PREFIX) {
            insert_hyperlink_edit(&mut edits, key, &value)?;
        }
    }
    let mut projection = HyperlinkProjection::new(edits);
    let mut links = Vec::new();
    for link in baseline {
        links.extend(projection.baseline(link)?);
    }
    while let Some(link) = projection.next_edit() {
        links.extend(link);
    }
    Ok(links)
}

pub(super) struct HyperlinkProjection {
    edits: HyperlinkEdits,
    changed: bool,
    occupied: MergeIndex,
    baseline_occupied: MergeIndex,
}

impl HyperlinkProjection {
    pub(super) fn new(edits: HyperlinkEdits) -> Self {
        Self {
            changed: !edits.is_empty(),
            edits,
            occupied: MergeIndex::default(),
            baseline_occupied: MergeIndex::default(),
        }
    }

    pub(super) fn baseline(&mut self, link: Hyperlink) -> Result<Option<Hyperlink>, String> {
        if !self.changed {
            return Ok(Some(link));
        }
        if self.baseline_occupied.overlaps(1, 0, MAX_COLS - 1, &link.range) {
            return Err("cannot edit overlapping imported hyperlinks".to_string());
        }
        self.baseline_occupied.insert(1, 0, MAX_COLS - 1, &link.range);
        let link = self.edits.remove(&link.range.to_a1()).unwrap_or(Some(link));
        Ok(link.and_then(|link| self.choose(link)))
    }

    pub(super) fn next_edit(&mut self) -> Option<Option<Hyperlink>> {
        let (_, link) = self.edits.pop_first()?;
        Some(link.and_then(|link| self.choose(link)))
    }

    fn choose(&mut self, link: Hyperlink) -> Option<Hyperlink> {
        if self.occupied.overlaps(1, 0, MAX_COLS - 1, &link.range) {
            return None;
        }
        self.occupied.insert(1, 0, MAX_COLS - 1, &link.range);
        Some(link)
    }
}

#[derive(Default)]
pub(super) struct HyperlinkBaseline {
    offset: usize,
    count: usize,
    done: bool,
}

impl HyperlinkBaseline {
    pub(super) fn next(&mut self, json: &str) -> Result<Option<Hyperlink>, String> {
        use crate::sheet_json::{MAX_HYPERLINKS_PER_SHEET, MAX_HYPERLINK_STATE_BYTES};

        let invalid = || "sheet hyperlinks are invalid".to_string();
        if self.done {
            return Ok(None);
        }
        if json.len() > MAX_HYPERLINK_STATE_BYTES {
            return Err("sheet hyperlink state exceeds its size limit".to_string());
        }
        let mut remaining = json[self.offset..].trim_start_matches([' ', '\n', '\r', '\t']);
        if self.offset == 0 {
            remaining = remaining
                .strip_prefix('[')
                .ok_or_else(invalid)?
                .trim_start_matches([' ', '\n', '\r', '\t']);
        } else if !remaining.starts_with(']') {
            remaining = remaining
                .strip_prefix(',')
                .ok_or_else(invalid)?
                .trim_start_matches([' ', '\n', '\r', '\t']);
            if remaining.starts_with(']') {
                return Err(invalid());
            }
        }
        if let Some(tail) = remaining.strip_prefix(']') {
            if !tail.trim_matches([' ', '\n', '\r', '\t']).is_empty() {
                return Err(invalid());
            }
            self.done = true;
            return Ok(None);
        }
        if self.count >= MAX_HYPERLINKS_PER_SHEET {
            return Err("sheet has too many hyperlinks".to_string());
        }
        let deserializer = serde_json::Deserializer::from_str(remaining);
        let mut stream = deserializer.into_iter::<serde::de::IgnoredAny>();
        stream.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
        let end = stream.byte_offset();
        let mut links = decode_hyperlinks(&format!("[{}]", &remaining[..end]))?;
        if links.len() != 1 {
            return Err(invalid());
        }
        self.offset = json.len() - remaining.len() + end;
        self.count += 1;
        Ok(links.pop())
    }
}
