//! Reusable conformance checks for every [`Backend`]. See `docs/sync.md` ("Backend contract").

use std::collections::BTreeSet;

use super::backend::{Backend, PutMode, PutOutcome};

/// Exercises the storage contract against an empty store. Listing assertions are
/// order-agnostic and use enough keys to cross small page sizes.
pub(crate) fn conformance(backend: &dyn Backend) {
    backend.refresh().expect("refresh");
    assert_eq!(backend.get("repository.json").expect("missing get"), None);
    assert!(backend.list("").expect("empty list").is_empty());
    backend
        .delete("objects/aa/missing")
        .expect("deleting a missing key succeeds");

    assert_eq!(
        backend
            .put("repository.json", b"first", PutMode::CreateOnly)
            .expect("create"),
        PutOutcome::Written
    );
    assert_eq!(
        backend
            .put("repository.json", b"second", PutMode::CreateOnly)
            .expect("create again"),
        PutOutcome::AlreadyExists
    );
    assert_eq!(
        backend.get("repository.json").expect("get").as_deref(),
        Some(&b"first"[..])
    );
    assert_eq!(
        backend
            .put("repository.json", b"replacement", PutMode::Overwrite)
            .expect("overwrite"),
        PutOutcome::Written
    );
    assert_eq!(
        backend.get("repository.json").expect("get").as_deref(),
        Some(&b"replacement"[..])
    );

    let empty = "heads/00000000000000000000000000000000/00000000000000000001";
    backend
        .put(empty, b"", PutMode::CreateOnly)
        .expect("empty value");
    assert_eq!(
        backend.get(empty).expect("empty get").as_deref(),
        Some(&b""[..])
    );

    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let mut expected = BTreeSet::new();
    for index in 0..120u32 {
        let id = format!("{index:02x}{}", "e".repeat(62));
        let key = format!("objects/{}/{id}", &id[..2]);
        backend
            .put(&key, &binary, PutMode::CreateOnly)
            .expect("object put");
        expected.insert(key);
    }
    let listed: BTreeSet<String> = backend
        .list("objects/")
        .expect("list objects")
        .into_iter()
        .collect();
    assert_eq!(listed, expected);
    let first = expected.iter().next().expect("first key").clone();
    assert_eq!(
        backend.get(&first).expect("object get").as_deref(),
        Some(binary.as_slice())
    );

    let all: BTreeSet<String> = backend.list("").expect("list all").into_iter().collect();
    assert!(all.contains("repository.json") && all.contains(empty) && all.is_superset(&expected));
    assert_eq!(
        backend.list("retired/").expect("empty prefix"),
        Vec::<String>::new()
    );

    backend.delete(&first).expect("delete");
    assert_eq!(backend.get(&first).expect("deleted get"), None);
    assert!(
        !backend
            .list("objects/")
            .expect("list after delete")
            .contains(&first)
    );

    for invalid in ["../escape", "/absolute", "Upper/case", "a//b"] {
        assert!(backend.get(invalid).is_err(), "{invalid}");
        assert!(
            backend.put(invalid, b"x", PutMode::Overwrite).is_err(),
            "{invalid}"
        );
    }
    assert!(backend.list("../").is_err());
    backend.flush().expect("flush");
}
