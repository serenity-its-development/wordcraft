//! Pictures as content-addressed references (the host's op contract: never bytes in the room).
//!
//! A picture in the document model names its bytes by a key in `Document::media` (`word/media/image1.png`). Two people
//! inserting two different pictures can both call theirs `image1.png`, so a key means nothing between windows. What the shared
//! tree holds instead is the picture's CONTENT ADDRESS: the shared form of the key is `cas-<sha256 hex>.<ext>`, written into the
//! picture's inline object, so the same bytes are the same reference in every window and different bytes never collide. The
//! bytes themselves are not in the room: a window that meets a reference it has no bytes for lists the hash in
//! [`MediaIndex::wanted`], and the host answers with the file's latest saved version, which carries every picture; the
//! window picks the bytes whose address it wants out of that file ([`MediaIndex::arrived`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use sha2::{Digest, Sha256};

/// The prefix of a shared (content-addressed) media key.
pub const SHARED_PREFIX: &str = "cas-";

/// SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The extension of a media key (`png` for `word/media/image1.png`), lowercase, or `bin`.
#[must_use]
pub fn extension_of(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 5 && ext.bytes().all(|b| b.is_ascii_alphanumeric()) => ext.to_ascii_lowercase(),
        _ => "bin".to_owned(),
    }
}

/// The shared form of a media key: `cas-<hash>.<ext>`.
#[must_use]
pub fn shared_key(hash: &str, local_key: &str) -> String {
    format!("{SHARED_PREFIX}{hash}.{}", extension_of(local_key))
}

/// The hash a shared key names (`None` if `key` is not a shared key).
#[must_use]
pub fn hash_of_shared(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(SHARED_PREFIX)?;
    let (hash, _) = rest.split_once('.')?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hash)
}

/// Which pictures this window holds, by key and by content address.
#[derive(Debug, Default, Clone)]
pub struct MediaIndex {
    /// local key -> (the bytes it was hashed from, their hash). The bytes are held so a changed entry is noticed by pointer.
    by_key: HashMap<String, (Arc<Vec<u8>>, String)>,
    /// content address -> a local key holding those bytes.
    by_hash: HashMap<String, String>,
    /// Content addresses the shared tree refers to that this window has no bytes for.
    wanted: BTreeSet<String>,
}

impl MediaIndex {
    /// Bring the index up to date with the document's media. Cheap when nothing changed (the bytes are compared by pointer).
    pub fn refresh(&mut self, media: &BTreeMap<String, Arc<Vec<u8>>>) {
        self.by_key.retain(|k, _| media.contains_key(k));
        for (key, bytes) in media {
            let fresh = match self.by_key.get(key) {
                Some((held, _)) => !Arc::ptr_eq(held, bytes),
                None => true,
            };
            if fresh {
                self.by_key.insert(key.clone(), (bytes.clone(), sha256_hex(bytes)));
            }
        }
        self.by_hash.clear();
        for (key, (_, hash)) in &self.by_key {
            // Several keys may hold the same bytes: the smallest key is the one every rebuild picks.
            match self.by_hash.get(hash) {
                Some(have) if have <= key => {}
                _ => {
                    self.by_hash.insert(hash.clone(), key.clone());
                }
            }
        }
        let have = &self.by_hash;
        self.wanted.retain(|h| !have.contains_key(h));
    }

    /// The content address of the picture a local key names.
    #[must_use]
    pub fn hash_of_key(&self, key: &str) -> Option<&str> {
        self.by_key.get(key).map(|(_, h)| h.as_str())
    }

    /// A local key holding the bytes with this content address.
    #[must_use]
    pub fn key_of_hash(&self, hash: &str) -> Option<&str> {
        self.by_hash.get(hash).map(String::as_str)
    }

    /// Note that the shared tree refers to `hash` (idempotent). A hash this window holds is not wanted.
    pub fn want(&mut self, hash: &str) {
        if !self.by_hash.contains_key(hash) {
            self.wanted.insert(hash.to_owned());
        }
    }

    /// The content addresses to ask the host for.
    #[must_use]
    pub fn wanted(&self) -> Vec<String> {
        self.wanted.iter().cloned().collect()
    }

    /// Whether `bytes` is the picture `hash` names. A host that answers with the wrong bytes is not believed.
    #[must_use]
    pub fn is_address_of(hash: &str, bytes: &[u8]) -> bool {
        sha256_hex(bytes) == hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_keys_carry_the_address_and_the_extension() {
        let h = sha256_hex(b"abc");
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let k = shared_key(&h, "word/media/Image1.PNG");
        assert_eq!(k, format!("cas-{h}.png"));
        assert_eq!(hash_of_shared(&k), Some(h.as_str()));
        assert_eq!(hash_of_shared("word/media/image1.png"), None);
        assert_eq!(hash_of_shared("cas-xyz.png"), None);
        assert_eq!(extension_of("noext"), "bin");
    }

    #[test]
    fn the_index_follows_the_media_and_forgets_what_arrived() {
        let mut media: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
        media.insert("b.png".into(), Arc::new(b"same".to_vec()));
        media.insert("a.png".into(), Arc::new(b"same".to_vec()));
        let mut idx = MediaIndex::default();
        idx.refresh(&media);
        let h = sha256_hex(b"same");
        assert_eq!(idx.hash_of_key("b.png"), Some(h.as_str()));
        assert_eq!(idx.key_of_hash(&h), Some("a.png"), "the smallest key stands for equal bytes");
        let other = sha256_hex(b"other");
        idx.want(&other);
        idx.want(&h);
        assert_eq!(idx.wanted(), vec![other.clone()], "bytes held are not wanted");
        media.insert(shared_key(&other, "x.jpg"), Arc::new(b"other".to_vec()));
        idx.refresh(&media);
        assert!(idx.wanted().is_empty());
        assert!(MediaIndex::is_address_of(&other, b"other") && !MediaIndex::is_address_of(&other, b"nope"));
    }
}
