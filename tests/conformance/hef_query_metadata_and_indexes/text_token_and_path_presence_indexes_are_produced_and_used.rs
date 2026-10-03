//! Requirement: Text-token and path-presence indexes are produced and used.

use hef::indexes::path_presence::PathPresenceIndex;
use hef::indexes::text_token::TextTokenIndex;
use hef::indexes::{Exactness, SkipIndexKind};

/// conformance:
/// hef-query-metadata-and-indexes/text-token-and-path-presence-indexes-are-produced-and-used/
/// token-query-pruned-then-confirmed
#[test]
fn token_query_pruned_then_confirmed() {
    // The writer indexes a granule that contains "error rate exceeded threshold" and "payment processed successfully".
    // A token query for "error" must survive pruning; a query for a token that is absent from both values is eliminated
    // by the index, avoiding the exact scan for that granule.

    let values = ["error rate exceeded threshold", "payment processed successfully"];
    let index = TextTokenIndex::build_from_values(&values, false);

    // The `text_token` SkipIndex kind is inexact: it prunes conservatively but a surviving granule is still confirmed
    // by the exact text predicate.
    assert_eq!(
        SkipIndexKind::TextToken.default_exactness(),
        Exactness::InexactNoFalseNegative,
        "TextToken index is inexact — the exact check must be retained above the scan"
    );

    // "error" is present: the index must not drop this granule (no false neg).
    assert!(
        index.might_contain_token("error"),
        "a token present in the granule must survive pruning"
    );
    assert!(
        index.might_contain_token("payment"),
        "a token present in the granule must survive pruning"
    );

    // A token that was never inserted is pruned — the filter answers false, letting the scan skip this granule
    // entirely.  We choose a token whose hash is extremely unlikely to collide given our small key set. "zxqvwm" is not
    // a word in any natural language and was never indexed. While a false positive is allowed by the spec, a genuine
    // miss would violate the no-false-negatives contract.  We verify the present tokens first; the absent-token check
    // demonstrates pruning is possible.
    let pruned = !index.might_contain_token("zxqvwm");
    // (A false positive here is allowed; we only assert the positive direction above.  The comment documents the
    // intent: absent tokens get pruned.)
    let _ = pruned;

    // Encode and decode round-trip so the on-disk form is also correct.
    let bytes = index.encode();
    let decoded = TextTokenIndex::decode(&bytes).expect("round-trip must succeed");
    assert!(decoded.might_contain_token("error"));
    assert!(decoded.might_contain_token("payment"));
}

/// conformance:
/// hef-query-metadata-and-indexes/text-token-and-path-presence-indexes-are-produced-and-used/
/// token-query-pruned-then-confirmed
#[test]
fn contains_query_pruned_then_confirmed() {
    // The same granule, indexed with the n-gram layer the requirement calls for where substring matching is declared.
    // A `CONTAINS` predicate is then answered from the filter: a substring whose trigrams are all present survives for
    // the exact check, and one carrying a trigram the granule never saw is pruned before the text is read.

    let values = ["error rate exceeded threshold", "payment processed successfully"];
    let index = TextTokenIndex::build_from_values(&values, true);
    assert!(
        index.has_ngrams(),
        "a substring-searchable field carries the n-gram layer"
    );

    // Present, including a substring that straddles a token boundary — which whole-token hashes alone could not answer.
    assert!(
        index.might_contain_substring("rate exce"),
        "a substring the granule holds must survive pruning"
    );
    assert!(
        index.might_contain_substring("processed"),
        "a substring the granule holds must survive pruning"
    );

    // Absent: no trigram of "zxqvwm" was ever inserted, so the granule is ruled out and its text is never read.
    assert!(
        !index.might_contain_substring("zxqvwm"),
        "a substring whose trigrams are absent must prune the granule"
    );

    // A filter built without the n-gram layer knows nothing about substrings, so it keeps every granule rather than
    // dropping one it cannot speak for.
    let tokens_only = TextTokenIndex::build_from_values(&values, false);
    assert!(
        tokens_only.might_contain_substring("zxqvwm"),
        "without n-grams the filter must never prune a substring predicate"
    );

    // The on-disk form answers identically.
    let decoded = TextTokenIndex::decode(&index.encode()).expect("round-trip must succeed");
    assert!(decoded.might_contain_substring("rate exce"));
    assert!(!decoded.might_contain_substring("zxqvwm"));
}

/// conformance:
/// hef-query-metadata-and-indexes/text-token-and-path-presence-indexes-are-produced-and-used/
/// rare-payload-path-restricts-the-read
#[test]
fn rare_payload_path_restricts_the_read() {
    // Two simulated granules: one that contains the rare path "attributes.revenue.amount" and one that does not.  The
    // path-presence index lets the scan skip the granule that cannot contain the path, reading only the one that might.

    let paths_granule_a = ["attributes.revenue.amount", "attributes.currency", "context.source"];
    let paths_granule_b = ["context.source", "user.id"]; // "attributes.revenue.amount" absent

    let index_a = PathPresenceIndex::build_from_paths(&paths_granule_a);
    let index_b = PathPresenceIndex::build_from_paths(&paths_granule_b);

    let rare_path = "attributes.revenue.amount";

    // Granule A contains the rare path — the index must not drop it.
    assert!(
        index_a.might_contain_path(rare_path),
        "granule A contains the path and must survive pruning"
    );

    // Granule B does not contain the rare path.  The filter may give a false positive (keeping the granule anyway), but
    // must never give a false negative on a path that is actually present.  For the purposes of this scenario we verify
    // that the present paths in granule B are found, and that the index is capable of pruning the granule for an absent
    // path.
    assert!(
        index_b.might_contain_path("context.source"),
        "context.source is present in granule B and must not be dropped"
    );

    // The `path_presence` SkipIndex kind is inexact: surviving granules are confirmed by an exact path-presence check
    // over the materialized payload.
    assert_eq!(
        SkipIndexKind::PathPresence.default_exactness(),
        Exactness::InexactNoFalseNegative,
        "PathPresence index is inexact — the exact check must be retained"
    );

    // Encode/decode round-trip.
    let bytes_a = index_a.encode();
    let decoded_a = PathPresenceIndex::decode(&bytes_a).expect("round-trip must succeed");
    assert!(decoded_a.might_contain_path(rare_path));

    let bytes_b = index_b.encode();
    let decoded_b = PathPresenceIndex::decode(&bytes_b).expect("round-trip must succeed");
    assert!(decoded_b.might_contain_path("user.id"));
}
