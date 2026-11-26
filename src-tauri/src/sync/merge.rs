//! Pure merge rules. See `docs/sync.md` ("Synchronization", step 4).

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

use super::model::{Entry, EntryKind, EntryValue, VersionVector, file_key, is_manifest_key};

const MAX_COPY_ROUNDS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VvOrder {
    Equal,
    Dominates,
    DominatedBy,
    Concurrent,
}

pub(crate) fn compare(left: &VersionVector, right: &VersionVector) -> VvOrder {
    let mut greater = false;
    let mut less = false;
    for writer in left.keys().chain(right.keys()) {
        let a = left.get(writer).copied().unwrap_or(0);
        let b = right.get(writer).copied().unwrap_or(0);
        match a.cmp(&b) {
            Ordering::Greater => greater = true,
            Ordering::Less => less = true,
            Ordering::Equal => {}
        }
    }
    match (greater, less) {
        (false, false) => VvOrder::Equal,
        (true, false) => VvOrder::Dominates,
        (false, true) => VvOrder::DominatedBy,
        (true, true) => VvOrder::Concurrent,
    }
}

pub(crate) fn pointwise_max(left: &VersionVector, right: &VersionVector) -> VersionVector {
    let mut merged = left.clone();
    for (writer, counter) in right {
        let slot = merged.entry(writer.clone()).or_insert(0);
        *slot = (*slot).max(*counter);
    }
    merged
}

/// `<stem>.sync-conflict-<first 8 hex of the object id>.<ext>` beside the original.
pub(crate) fn conflict_copy_path(path: &str, object_id: &str) -> String {
    let tag = &object_id[..object_id.len().min(8)];
    let (directory, name) = match path.rsplit_once('/') {
        Some((directory, name)) => (Some(directory), name),
        None => (None, path),
    };
    let renamed = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.sync-conflict-{tag}.{extension}"),
        _ => format!("{name}.sync-conflict-{tag}"),
    };
    match directory {
        Some(directory) => format!("{directory}/{renamed}"),
        None => renamed,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct KeyMerge {
    /// `None` keeps the key absent.
    pub entry: Option<Entry>,
    pub conflict_copies: Vec<(String, Entry)>,
    /// Remote values kept aside for an explicit user choice.
    pub pending: Vec<Entry>,
    pub pause_vault: bool,
}

struct Candidate<'a> {
    entry: &'a Entry,
    local: bool,
}

fn later(left: &Entry, right: &Entry) -> Ordering {
    left.modified_at
        .cmp(&right.modified_at)
        .then_with(|| left.value.order_key().cmp(&right.value.order_key()))
        .then_with(|| left.origin.cmp(&right.origin))
}

/// Merges one key. `local_unconfirmed` makes the local entry concurrent with every
/// remote entry (restored, cloned, or newly mapped state).
pub(crate) fn merge_key(key: &str, local: Option<&Entry>, local_unconfirmed: bool, remotes: &[&Entry]) -> KeyMerge {
    let mut candidates: Vec<Candidate<'_>> = local.map(|entry| Candidate { entry, local: true }).into_iter().collect();
    candidates.extend(remotes.iter().map(|entry| Candidate { entry, local: false }));
    if candidates.is_empty() {
        return KeyMerge::default();
    }
    let merged_vv = candidates
        .iter()
        .fold(VersionVector::new(), |vv, candidate| pointwise_max(&vv, &candidate.entry.vv));

    let exempt = |candidate: &Candidate<'_>| candidate.local && local_unconfirmed;
    let maximal: Vec<&Candidate<'_>> = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                !exempt(candidate)
                    && !exempt(other)
                    && compare(&other.entry.vv, &candidate.entry.vv) == VvOrder::Dominates
            })
        })
        .collect();

    // One representative per distinct value, chosen deterministically.
    let mut distinct: Vec<(&Entry, bool)> = Vec::new();
    for candidate in &maximal {
        match distinct.iter_mut().find(|(entry, _)| entry.value == candidate.entry.value) {
            Some((entry, local)) => {
                *local |= candidate.local;
                if later(candidate.entry, entry) == Ordering::Greater {
                    *entry = candidate.entry;
                }
            }
            None => distinct.push((candidate.entry, candidate.local)),
        }
    }
    if distinct.iter().any(|(entry, _)| !entry.value.is_tombstone()) {
        distinct.retain(|(entry, _)| !entry.value.is_tombstone());
    }
    let with_vv = |entry: &Entry| Entry { vv: merged_vv.clone(), ..entry.clone() };
    if distinct.len() == 1 {
        return KeyMerge {
            entry: Some(with_vv(distinct[0].0)),
            ..KeyMerge::default()
        };
    }
    distinct.sort_by(|left, right| later(right.0, left.0));
    let kind = EntryKind::parse(key);

    if is_manifest_key(key) {
        let remote_choice = distinct.iter().find(|(_, local)| !local).map(|(entry, _)| (*entry).clone());
        return KeyMerge {
            entry: local.cloned(),
            pending: remote_choice.into_iter().collect(),
            pause_vault: true,
            ..KeyMerge::default()
        };
    }
    if matches!(kind, EntryKind::PluginSetting { .. } | EntryKind::PluginState { .. }) {
        let kept = distinct
            .iter()
            .find(|(_, local)| *local)
            .map(|(entry, _)| *entry)
            .unwrap_or(distinct[0].0);
        return KeyMerge {
            entry: Some(with_vv(kept)),
            pending: distinct
                .iter()
                .filter(|(entry, _)| entry.value != kept.value)
                .map(|(entry, _)| (*entry).clone())
                .collect(),
            ..KeyMerge::default()
        };
    }

    let winner = distinct[0].0;
    let conflict_copies = match kind {
        EntryKind::VaultFile { vault, path } => distinct[1..]
            .iter()
            .filter_map(|(entry, _)| {
                entry
                    .value
                    .object_id()
                    .map(|id| (file_key(vault, &conflict_copy_path(path, id)), (*entry).clone()))
            })
            .collect(),
        _ => Vec::new(),
    };
    KeyMerge {
        entry: Some(with_vv(winner)),
        conflict_copies,
        ..KeyMerge::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StateMerge {
    pub entries: BTreeMap<String, Entry>,
    pub pending: Vec<(String, Entry)>,
    pub paused_vaults: BTreeSet<String>,
}

/// Merges the local index with every remote snapshot for the keys `include` admits.
/// Keys outside `include` keep their local entry. The result is independent of the
/// order of `remotes`.
pub(crate) fn merge_state(
    local: &BTreeMap<String, Entry>,
    unconfirmed: &dyn Fn(&str) -> bool,
    remotes: &[&BTreeMap<String, Entry>],
    include: &dyn Fn(&str) -> bool,
) -> StateMerge {
    let mut keys: BTreeSet<&String> = local.keys().collect();
    for remote in remotes {
        keys.extend(remote.keys().filter(|key| include(key)));
    }
    let mut result = StateMerge::default();
    let mut copies: Vec<(String, Entry)> = Vec::new();
    for key in keys {
        let local_entry = local.get(key);
        if !include(key) {
            if let Some(entry) = local_entry {
                result.entries.insert(key.clone(), entry.clone());
            }
            continue;
        }
        let remote_entries: Vec<&Entry> = remotes.iter().filter_map(|remote| remote.get(key)).collect();
        let merged = merge_key(key, local_entry, unconfirmed(key), &remote_entries);
        if merged.pause_vault
            && let Some(vault) = EntryKind::parse(key).vault()
        {
            result.paused_vaults.insert(vault.to_string());
        }
        result
            .pending
            .extend(merged.pending.into_iter().map(|entry| (key.clone(), entry)));
        copies.extend(merged.conflict_copies);
        if let Some(entry) = merged.entry {
            result.entries.insert(key.clone(), entry);
        }
    }

    for _ in 0..MAX_COPY_ROUNDS {
        if copies.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for (key, copy) in copies {
            let existing = result.entries.get(&key).cloned();
            let merged = merge_key(&key, existing.as_ref(), false, &[&copy]);
            next.extend(merged.conflict_copies);
            if let Some(entry) = merged.entry {
                result.entries.insert(key, entry);
            }
        }
        copies = next;
    }

    if !result.paused_vaults.is_empty() {
        let paused = |key: &str| {
            EntryKind::parse(key)
                .vault()
                .is_some_and(|vault| result.paused_vaults.contains(vault))
        };
        result.entries.retain(|key, _| !paused(key) || local.contains_key(key));
        for (key, entry) in local.iter().filter(|(key, _)| paused(key)) {
            result.entries.insert(key.clone(), entry.clone());
        }
        let paused_vaults = result.paused_vaults.clone();
        result.pending.retain(|(key, _)| {
            !EntryKind::parse(key).vault().is_some_and(|vault| paused_vaults.contains(vault)) || is_manifest_key(key)
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const V: &str = "0123456789abcdef0123456789abcdef";
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccc";

    fn vv(pairs: &[(&str, u64)]) -> VersionVector {
        pairs.iter().map(|(writer, counter)| (writer.to_string(), *counter)).collect()
    }

    fn object(id_byte: char, modified_at: i64, origin: &str, pairs: &[(&str, u64)]) -> Entry {
        Entry {
            value: EntryValue::Object { id: id_byte.to_string().repeat(64), size: 1, mode: 0o644 },
            modified_at,
            origin: origin.into(),
            vv: vv(pairs),
        }
    }

    fn tombstone(modified_at: i64, origin: &str, pairs: &[(&str, u64)]) -> Entry {
        Entry { value: EntryValue::Tombstone, modified_at, origin: origin.into(), vv: vv(pairs) }
    }

    fn inline(value: serde_json::Value, modified_at: i64, origin: &str, pairs: &[(&str, u64)]) -> Entry {
        Entry { value: EntryValue::Inline { json: value }, modified_at, origin: origin.into(), vv: vv(pairs) }
    }

    fn file(path: &str) -> String {
        file_key(V, path)
    }

    #[test]
    fn version_vectors_compare_and_merge_pointwise() {
        assert_eq!(compare(&vv(&[(A, 1)]), &vv(&[(A, 1)])), VvOrder::Equal);
        assert_eq!(compare(&vv(&[(A, 2)]), &vv(&[(A, 1)])), VvOrder::Dominates);
        assert_eq!(compare(&vv(&[(A, 1)]), &vv(&[(A, 1), (B, 1)])), VvOrder::DominatedBy);
        assert_eq!(compare(&vv(&[(A, 2)]), &vv(&[(A, 1), (B, 1)])), VvOrder::Concurrent);
        assert_eq!(pointwise_max(&vv(&[(A, 2), (C, 1)]), &vv(&[(A, 1), (B, 4)])), vv(&[(A, 2), (B, 4), (C, 1)]));
    }

    #[test]
    fn conflict_copy_names_are_deterministic() {
        let id = "1234abcd".repeat(8);
        assert_eq!(conflict_copy_path("notes/a.md", &id), "notes/a.sync-conflict-1234abcd.md");
        assert_eq!(conflict_copy_path("README", &id), "README.sync-conflict-1234abcd");
        assert_eq!(conflict_copy_path(".env", &id), ".env.sync-conflict-1234abcd");
        assert_eq!(conflict_copy_path("a.tar.gz", &id), "a.tar.sync-conflict-1234abcd.gz");
        assert_eq!(
            conflict_copy_path(".denote/note-comments.json", &id),
            ".denote/note-comments.sync-conflict-1234abcd.json"
        );
    }

    #[test]
    fn dominating_and_equal_entries_merge_without_copies() {
        let key = file("a.md");
        let old = object('1', 10, A, &[(A, 1)]);
        let new = object('2', 5, B, &[(A, 1), (B, 1)]);
        let merged = merge_key(&key, Some(&old), false, &[&new]);
        assert_eq!(merged.entry.unwrap(), new);
        assert!(merged.conflict_copies.is_empty());

        // Dominance wins even when the dominated side is newer by clock.
        let merged = merge_key(&key, Some(&new), false, &[&old]);
        assert_eq!(merged.entry.unwrap(), new);

        // Equal values with concurrent vectors converge on the maximum without a local increment.
        let same_a = object('1', 10, A, &[(A, 2)]);
        let same_b = object('1', 12, B, &[(B, 3)]);
        let merged = merge_key(&key, Some(&same_a), false, &[&same_b]).entry.unwrap();
        assert_eq!(merged.vv, vv(&[(A, 2), (B, 3)]));
        assert_eq!(merged.origin, B);
        assert!(merge_key(&key, None, false, &[]).entry.is_none());
    }

    #[test]
    fn concurrent_file_versions_keep_both_with_a_deterministic_winner() {
        let key = file("dir/a.md");
        let local = object('1', 10, A, &[(A, 2)]);
        let remote = object('2', 20, B, &[(A, 1), (B, 1)]);
        let left = merge_key(&key, Some(&local), false, &[&remote]);
        let right = merge_key(&key, Some(&remote), false, &[&local]);
        assert_eq!(left, right);
        let winner = left.entry.unwrap();
        assert_eq!(winner.value, remote.value);
        assert_eq!(winner.vv, vv(&[(A, 2), (B, 1)]));
        assert_eq!(left.conflict_copies.len(), 1);
        let (copy_key, copy) = &left.conflict_copies[0];
        assert_eq!(copy_key, &file(&format!("dir/a.sync-conflict-{}.md", "1".repeat(8))));
        assert_eq!(copy.value, local.value);

        // Same timestamp: the higher object id wins.
        let tie_a = object('3', 10, A, &[(A, 1)]);
        let tie_b = object('4', 10, B, &[(B, 1)]);
        assert_eq!(merge_key(&key, Some(&tie_a), false, &[&tie_b]).entry.unwrap().value, tie_b.value);
    }

    #[test]
    fn concurrent_edit_beats_tombstone() {
        let key = file("a.md");
        let deleted = tombstone(50, A, &[(A, 2)]);
        let edited = object('2', 10, B, &[(A, 1), (B, 1)]);
        for merged in [
            merge_key(&key, Some(&deleted), false, &[&edited]),
            merge_key(&key, Some(&edited), false, &[&deleted]),
        ] {
            let entry = merged.entry.unwrap();
            assert_eq!(entry.value, edited.value);
            assert_eq!(entry.vv, vv(&[(A, 2), (B, 1)]));
            assert!(merged.conflict_copies.is_empty());
        }
        // A dominating tombstone still deletes.
        let later_delete = tombstone(1, A, &[(A, 2), (B, 1)]);
        assert!(merge_key(&key, Some(&edited), false, &[&later_delete]).entry.unwrap().value.is_tombstone());
    }

    #[test]
    fn concurrent_encryption_manifests_pause_the_vault() {
        let key = file(".denote/encryption.json");
        let local = object('1', 10, A, &[(A, 1)]);
        let remote = object('2', 20, B, &[(B, 1)]);
        let merged = merge_key(&key, Some(&local), false, &[&remote]);
        assert!(merged.pause_vault);
        assert_eq!(merged.entry.unwrap(), local);
        assert_eq!(merged.pending, vec![remote.clone()]);

        let mut state = BTreeMap::new();
        state.insert(key.clone(), local.clone());
        state.insert(file("a.md"), object('5', 1, A, &[(A, 1)]));
        let mut remote_state = BTreeMap::new();
        remote_state.insert(key.clone(), remote.clone());
        remote_state.insert(file("a.md"), object('6', 1, B, &[(A, 1), (B, 2)]));
        remote_state.insert(file("b.md"), object('7', 1, B, &[(B, 3)]));
        let result = merge_state(&state, &|_| false, &[&remote_state], &|_| true);
        assert_eq!(result.paused_vaults, BTreeSet::from([V.to_string()]));
        assert_eq!(result.entries, state, "nothing in a paused vault changes");
        assert_eq!(result.pending, vec![(key, remote)]);
    }

    #[test]
    fn concurrent_plugin_values_keep_local_and_record_the_remote() {
        let key = "plugin/denote.sample/state/count";
        let local = inline(json!(1), 10, A, &[(A, 1)]);
        let remote = inline(json!(2), 20, B, &[(B, 1)]);
        let merged = merge_key(key, Some(&local), false, &[&remote]);
        let entry = merged.entry.unwrap();
        assert_eq!(entry.value, local.value);
        assert_eq!(entry.vv, vv(&[(A, 1), (B, 1)]));
        assert_eq!(merged.pending, vec![remote.clone()]);

        // Other inline kinds resolve deterministically without pending conflicts.
        let pref = "app/preferences/denote-theme";
        let merged = merge_key(pref, Some(&local), false, &[&remote]);
        assert_eq!(merged.entry.unwrap().value, remote.value);
        assert!(merged.pending.is_empty());
    }

    #[test]
    fn history_is_a_set_union() {
        let first = format!("v/{V}/history/{}", "1".repeat(64));
        let second = format!("v/{V}/history/{}", "2".repeat(64));
        let mut local = BTreeMap::new();
        local.insert(first.clone(), object('1', 1, A, &[(A, 1)]));
        let mut remote = BTreeMap::new();
        remote.insert(second.clone(), object('2', 1, B, &[(B, 1)]));
        let merged = merge_state(&local, &|_| false, &[&remote], &|_| true);
        assert_eq!(merged.entries.len(), 2);
        assert!(merged.pending.is_empty());
    }

    #[test]
    fn unconfirmed_local_entries_are_never_silently_replaced() {
        let key = file("a.md");
        // The restored local entry claims to dominate, but it is unconfirmed.
        let local = object('1', 10, A, &[(A, 5), (B, 5)]);
        let remote = object('2', 5, B, &[(B, 3)]);
        let merged = merge_key(&key, Some(&local), true, &[&remote]);
        assert_eq!(merged.entry.unwrap().value, local.value);
        assert_eq!(merged.conflict_copies.len(), 1);
        assert_eq!(merged.conflict_copies[0].1.value, remote.value);

        let equal = merge_key(&key, Some(&local), true, &[&object('1', 3, B, &[(B, 1)])]);
        assert!(equal.conflict_copies.is_empty());
    }

    #[test]
    fn merge_is_independent_of_remote_order() {
        let key = file("n.md");
        let local = object('1', 10, A, &[(A, 3)]);
        let r1 = object('2', 30, B, &[(A, 1), (B, 2)]);
        let r2 = object('3', 20, C, &[(A, 1), (B, 2), (C, 1)]);
        let r3 = tombstone(40, C, &[(C, 4)]);
        let remotes = [&r1, &r2, &r3];
        let reference = merge_key(&key, Some(&local), false, &remotes);
        for order in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let permuted: Vec<&Entry> = order.iter().map(|index| remotes[*index]).collect();
            let mut merged = merge_key(&key, Some(&local), false, &permuted);
            merged.conflict_copies.sort_by(|a, b| a.0.cmp(&b.0));
            let mut expected = reference.clone();
            expected.conflict_copies.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(merged, expected);
        }
        // r2 dominates r1, so only local and r2 compete; the tombstone loses to edits.
        assert_eq!(reference.entry.unwrap().value, r2.value);
        assert_eq!(reference.conflict_copies.len(), 1);
    }

    #[test]
    fn excluded_keys_keep_their_local_entries() {
        let mut local = BTreeMap::new();
        local.insert(file("a.md"), object('1', 1, A, &[(A, 1)]));
        let mut remote = BTreeMap::new();
        remote.insert(file("a.md"), object('2', 1, B, &[(A, 1), (B, 1)]));
        remote.insert("future/kind".to_string(), inline(json!(1), 1, B, &[(B, 1)]));
        let merged = merge_state(&local, &|_| false, &[&remote], &|key| key.starts_with("future"));
        assert_eq!(merged.entries.get(&file("a.md")), local.get(&file("a.md")));
        assert!(merged.entries.contains_key("future/kind"));
    }
}
