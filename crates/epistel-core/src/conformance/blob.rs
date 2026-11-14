// SPDX-FileCopyrightText: 2026 Sigmastone OÜ
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;

use super::assert_not_found;
use crate::blob::BlobStore;
use crate::id::BlobId;

/// Runs the whole [`BlobStore`] contract against `store`.
pub async fn blob_store(store: &dyn BlobStore) {
    let text = Bytes::from_static(b"Subject: hello\r\n\r\nbody\r\n");
    // Every byte value, to catch a backend that mangles anything that is not text.
    let binary = Bytes::from((0..=255u8).cycle().take(100_000).collect::<Vec<_>>());

    let first = store.put(text.clone()).await.expect("put");
    let second = store.put(binary.clone()).await.expect("put");
    let duplicate = store.put(text.clone()).await.expect("put the same content again");
    assert_ne!(first, second);
    assert_ne!(first, duplicate, "every put gets its own identifier");

    assert_eq!(store.get(first).await.expect("get"), text);
    assert_eq!(store.get(second).await.expect("get"), binary);

    let empty = store.put(Bytes::new()).await.expect("put nothing");
    assert!(store.get(empty).await.expect("get").is_empty());

    store.delete(first).await.expect("delete");
    assert_not_found(store.get(first).await);
    assert_eq!(store.get(duplicate).await.expect("the other copy is untouched"), text);
    store.delete(first).await.expect("deleting again is not an error");
    store.delete(BlobId::generate()).await.expect("deleting what never existed is not an error");
    assert_not_found(store.get(BlobId::generate()).await);

    for id in [second, duplicate, empty] {
        store.delete(id).await.expect("clean up");
    }
}
