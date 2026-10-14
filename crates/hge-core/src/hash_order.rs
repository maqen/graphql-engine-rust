//! V2's iteration order for text-keyed `HashMap`s and `HashSet`s.
//!
//! Some V2 error messages list names straight from `HashMap.keys` or `HS.toList`, so their
//! order is the hash order of `unordered-containers` (0.2.21), not insertion or name order.
//! Keys are hashed by `hashable` (1.5.1, as pinned by v2.51's `cabal.project.freeze`), whose
//! `hash` for `Text` is XXH3-64 of the UTF-8 bytes with seed 0. The HAMT then orders keys by
//! 5-bit chunks of the hash, lowest chunk first.

use xxhash_rust::xxh3::xxh3_64;

/// Sort `names` into the order V2 lists them in when they come from a text-keyed hash map
/// or set (`PGCol`, `G.Name`, `Text` and other newtypes over `Text`).
pub fn sort_v2_hash_order<S: AsRef<str>>(names: &mut [S]) {
    names.sort_by_cached_key(|n| chunk_key(xxh3_64(n.as_ref().as_bytes())));
}

/// The hash with its 5-bit chunks reversed, so that comparing keys compares the lowest chunk
/// first (64 bits: twelve full chunks and a 4-bit top one).
fn chunk_key(hash: u64) -> u64 {
    let low = (0..12).fold(0, |key, i| (key << 5) | ((hash >> (5 * i)) & 0x1f));
    (low << 4) | (hash >> 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
        sort_v2_hash_order(&mut v);
        v
    }

    /// Orders observed on hasura/graphql-engine:v2.51.0-ce.
    #[test]
    fn matches_v2() {
        assert_eq!(sorted(&["author_id", "id"]), ["id", "author_id"]);
        assert_eq!(sorted(&["x", "y", "z"]), ["y", "z", "x"]);
        assert_eq!(
            sorted(&[
                "c1_smallint",
                "c2_integer",
                "c3_bigint",
                "c4_decimal",
                "c5_numeric",
                "c6_real",
                "c7_double_precision",
                "c9_serial",
            ]),
            [
                "c9_serial",
                "c5_numeric",
                "c2_integer",
                "c3_bigint",
                "c6_real",
                "c4_decimal",
                "c7_double_precision",
                "c1_smallint",
            ]
        );
    }
}
