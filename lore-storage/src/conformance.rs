// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The contract every [`ImmutableStore`] owes its callers, as an executable battery.
//!
//! A caller reaching a store through `Arc<dyn ImmutableStore>` cannot see which implementation it
//! has, so the guarantees must not depend on that. They currently do: the same question asked of
//! two stores, or of one store through two of its methods, can come back differently. This module
//! is where those guarantees are stated once and checked against every implementation, rather than
//! restated in each store's own tests in each store's own words.
//!
//! It is deliberately not `#[cfg(test)]`. The implementations live in several crates, and a store
//! reached over a wire is only assemblable where a server is; each store calls
//! [`verify_immutable_store`] from a test of its own, with the construction that crate already
//! uses.
//!
//! The clauses, as written into the trait:
//!
//! 1. **Never over-report.** A reported level must hold: the association it names exists. Whether
//!    the payload can be served from here is `stored_local` and `stored_durable`, not the level — a
//!    store may hold the representation alone and still report a full match.
//! 2. **May under-report.** A store may answer with a weaker level than the truth when establishing
//!    the stronger one costs more than it is worth. This is why the assertions below are bounds —
//!    `<= MatchPartition`, never `== MatchPartition`.
//! 3. **Obliterated never matches**, through any method.
//! 4. **Reads do not under-serve, and agree with each other.** `get` serves everything within the
//!    store's read scope and `get_metadata` describes whatever `get` would serve. `query` reaches
//!    further, to the store's query scope, because a level is something to act on with another
//!    operation rather than bytes owed here.
//! 5. **A match names where it was found, and prefers where it was asked.** Another partition is
//!    named only when the one asked about holds nothing. The context may be left unnamed, but a
//!    named one is an association `copy` will read from.
//!
//! # Known violations
//!
//! Not every store passes today. A store declares the checks it is known to fail via
//! [`Capabilities::known_violations`], and the battery then *requires* those to fail: a defect that
//! gets fixed without being delisted fails the test just as loudly as a new one. Nothing is
//! silently skipped, and the list is the work queue.

use std::sync::Arc;

use bytes::Bytes;
use lore_base::types::Address;
use lore_base::types::Context;
use lore_base::types::Fragment;
use lore_base::types::Partition;

use crate::immutable_store::ImmutableStore;
use crate::immutable_store::StoreError;
use crate::immutable_store::query_one;
use crate::store_types::StoreGetData;
use crate::store_types::StoreMatch;
use crate::store_types::StoreMatchResult;
use crate::store_types::StoreObliterateStats;

/// One case in the battery. Named so a store can declare which ones it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// An address the store has never seen matches nothing, through any method.
    UnknownAddressMatchesNothing,
    /// Content stored with its payload reports a full match and reads back byte for byte.
    StoredAddressMatchesFully,
    /// The same hash under another context in the same partition is never a full match.
    OtherContextNeverMatchesFully,
    /// The same hash in another partition never matches above the hash, and never at all across a
    /// trust boundary.
    OtherPartitionNeverMatchesAboveHash,
    /// Obliterated content matches nothing, through any method.
    ObliteratedMatchesNothing,
    /// Resolution and metadata agree with each other about content re-stored over a tombstone.
    MethodsAgreeAfterObliteration,
    /// Batch results correspond positionally to the addresses that were asked about.
    BatchResultsLineUp,
    /// A hash held by both the partition asked about and another names the one asked about.
    MatchPrefersTheAskedPartition,
    /// The source a partition match names can be copied from, whether or not it named a context.
    NamedSourceCanBeCopied,
    /// Obliterating one reference leaves the others readable.
    ObliterationLeavesSiblingsReadable,
    /// A fragment stored without its payload is reported and described, but not served.
    MetadataOnlyIsDescribedNotServed,
    /// `get` and `get_metadata` agree on the stored payload's `size_content` and `size_payload`.
    GetFragmentMatchesMetadata,
    /// After a put with payload, the query result reports `stored_local` or `stored_durable`.
    StoredPayloadIsAccountedFor,
    /// After a copy, `get` on the destination serves the same bytes as the original.
    CopiedAddressIsServable,
    /// After a copy, `get` on the source still serves the original bytes.
    CopySourceRemainsReadable,
}

/// What the store under test can do, so the battery runs the subset that applies.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    /// Names the store in assertion messages. A failure has to say which implementation failed.
    pub label: &'static str,
    /// Whether the store accepts `put`. Everything needing content to exist is skipped without it,
    /// which is most of the battery — read replicas get the absence cases only.
    pub can_put: bool,
    /// Whether the store accepts `obliterate`.
    pub can_obliterate: bool,
    /// Whether the store accepts `copy`. The trait defaults it to unsupported, so a store that
    /// takes the default reports a level no caller can act on and is exempted rather than failed.
    pub can_copy: bool,
    /// Whether answers from this store cross a trust boundary. A hash held only by a partition the
    /// caller has no claim to is another tenant's content, and its existence is not the caller's to
    /// learn, so such a store must report nothing rather than a weak match.
    pub over_wire: bool,
    /// Whether a read that misses leaves the store unable to answer any later read. The gRPC
    /// storage transport multiplexes every read of a session onto one bidirectional stream and
    /// reports a missing fragment as that stream's terminal status, so the first miss ends the
    /// stream and every read after it waits forever. Checks that read after a miss are skipped
    /// rather than declared, because a hang is not an outcome the battery can record.
    pub miss_poisons_session: bool,
    /// Whether this store accepts a `put` carrying no payload, keeping the representation alone.
    /// Only the local store does; the durable store refuses one outright, and the composite relies
    /// on the local store taking it — `with_local_metadata_only` writes nothing else.
    pub stores_metadata_only: bool,
    /// Checks this store is known to fail today. Required to keep failing — see the module docs.
    pub known_violations: &'static [Check],
}

impl Capabilities {
    /// A store that supports everything, answers in process, and is expected to be correct.
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            can_put: true,
            can_obliterate: true,
            can_copy: true,
            over_wire: false,
            miss_poisons_session: false,
            stores_metadata_only: false,
            known_violations: &[],
        }
    }

    pub fn no_put(mut self) -> Self {
        self.can_put = false;
        self
    }

    pub fn no_obliterate(mut self) -> Self {
        self.can_obliterate = false;
        self
    }

    pub fn no_copy(mut self) -> Self {
        self.can_copy = false;
        self
    }

    pub fn stores_metadata_only(mut self) -> Self {
        self.stores_metadata_only = true;
        self
    }

    pub fn miss_poisons_session(mut self) -> Self {
        self.miss_poisons_session = true;
        self
    }

    pub fn over_wire(mut self) -> Self {
        self.over_wire = true;
        self
    }

    /// Declare the checks this store fails today. Each entry should name the defect in a comment at
    /// the call site.
    pub fn known_violations(mut self, checks: &'static [Check]) -> Self {
        self.known_violations = checks;
        self
    }
}

/// Fail the current check with a message, rather than panicking, so the driver can decide whether
/// the failure was expected.
macro_rules! require {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

macro_rules! require_eq {
    ($left:expr, $right:expr, $($arg:tt)*) => {
        {
            let left = $left;
            let right = $right;
            if left != right {
                return Err(format!(
                    "{} (got {left:?}, expected {right:?})",
                    format!($($arg)*)
                ));
            }
        }
    };
}

/// Run the whole battery. Panics on the first unexpected outcome, naming the check and the store.
pub async fn verify_immutable_store(store: Arc<dyn ImmutableStore>, caps: Capabilities) {
    settle(
        &caps,
        Check::UnknownAddressMatchesNothing,
        an_unknown_address_matches_nothing(&store, &caps).await,
    );
    settle(
        &caps,
        Check::BatchResultsLineUp,
        batch_results_line_up_with_their_addresses(&store, &caps).await,
    );

    if !caps.can_put {
        return;
    }

    settle(
        &caps,
        Check::StoredAddressMatchesFully,
        a_stored_address_matches_fully(&store, &caps).await,
    );
    settle(
        &caps,
        Check::GetFragmentMatchesMetadata,
        get_and_get_metadata_report_the_same_fragment(&store, &caps).await,
    );
    settle(
        &caps,
        Check::StoredPayloadIsAccountedFor,
        stored_content_is_accounted_for(&store, &caps).await,
    );
    settle(
        &caps,
        Check::OtherContextNeverMatchesFully,
        another_context_in_the_same_partition_never_matches_fully(&store, &caps).await,
    );
    settle(
        &caps,
        Check::OtherPartitionNeverMatchesAboveHash,
        another_partition_never_matches_above_the_hash(&store, &caps).await,
    );

    settle(
        &caps,
        Check::MatchPrefersTheAskedPartition,
        a_match_prefers_the_partition_it_was_asked_about(&store, &caps).await,
    );

    if caps.can_copy {
        settle(
            &caps,
            Check::NamedSourceCanBeCopied,
            the_source_a_match_names_can_be_copied_from(&store, &caps).await,
        );
        settle(
            &caps,
            Check::CopiedAddressIsServable,
            a_copied_address_is_servable(&store, &caps).await,
        );
        settle(
            &caps,
            Check::CopySourceRemainsReadable,
            a_copy_leaves_the_source_readable(&store, &caps).await,
        );
    }

    if caps.stores_metadata_only {
        settle(
            &caps,
            Check::MetadataOnlyIsDescribedNotServed,
            a_metadata_only_entry_is_described_but_not_served(&store, &caps).await,
        );
    }

    if caps.can_obliterate {
        settle(
            &caps,
            Check::ObliterationLeavesSiblingsReadable,
            obliterating_one_reference_leaves_the_others_readable(&store, &caps).await,
        );
        settle(
            &caps,
            Check::ObliteratedMatchesNothing,
            an_obliterated_address_matches_nothing(&store, &caps).await,
        );
        settle(
            &caps,
            Check::MethodsAgreeAfterObliteration,
            the_methods_agree_after_an_obliteration(&store, &caps).await,
        );
    }
}

/// Decide what an outcome means given what the store declared.
///
/// A check that passes while listed as a known violation is as much a failure as one that fails
/// while not listed: the first means the list has rotted, and a rotted list is how a defect stops
/// being tracked.
fn settle(caps: &Capabilities, check: Check, outcome: Result<(), String>) {
    let known = caps.known_violations.contains(&check);
    match (outcome, known) {
        (Ok(()), false) | (Err(_), true) => {}
        (Err(why), false) => panic!("[{}] {check:?}: {why}", caps.label),
        (Ok(()), true) => panic!(
            "[{}] {check:?} is listed as a known violation but now holds — remove it from \
             known_violations",
            caps.label
        ),
    }
}

/// The best level the store will admit to for an address.
///
/// This asks [`query_one`] rather than a store's own batched `query` so that single-address
/// resolution is exercised through the same helper production callers use.
async fn best_match(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    caps: &Capabilities,
) -> Result<StoreMatch, String> {
    query_one(store, partition, address)
        .await
        .map(|result| result.match_made)
        .map_err(|err| format!("resolve failed on {}: {err:?}", caps.label))
}

/// Content nobody has stored, addressed in a partition nobody has used.
fn unique_content() -> (Partition, Address, Bytes, Fragment) {
    let payload = Bytes::from(rand::random::<[u8; 32]>().to_vec());
    let address = Address {
        hash: crate::hash::hash_slice(payload.as_ref()),
        context: Context::from(rand::random::<[u8; 16]>()),
    };
    let fragment = Fragment {
        flags: 0,
        size_payload: payload.len() as u32,
        size_content: payload.len() as u64,
    };
    (
        Partition::from(rand::random::<[u8; 16]>()),
        address,
        payload,
        fragment,
    )
}

async fn store_content(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    fragment: Fragment,
    payload: Bytes,
) -> Result<(), String> {
    store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .map_err(|err| format!("put failed: {err:?}"))
}

fn is_absent(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::AddressNotFound(_) | StoreError::PayloadNotFound(_) | StoreError::NotFound(_)
    )
}

/// A store may only report absence for content it does not hold — never a weak match, never a
/// fragment, never bytes, and no claim about where a payload it did not match is kept.
async fn assert_absent(
    store: &Arc<dyn ImmutableStore>,
    partition: Partition,
    address: Address,
    caps: &Capabilities,
    context: &str,
) -> Result<(), String> {
    let resolved = query_one(store, partition, address)
        .await
        .map_err(|err| format!("resolve failed on {}: {err:?}", caps.label))?;

    require_eq!(
        resolved.match_made,
        StoreMatch::MatchNone,
        "{context}: exist reported a match for content the store does not hold"
    );

    require!(
        !resolved.stored_local && !resolved.stored_durable,
        "{context}: query claimed a payload location for content it did not match"
    );

    require_eq!(
        resolved.partition,
        Partition::default(),
        "{context}: query named a source partition for content it did not match"
    );

    require_eq!(
        resolved.context,
        Context::default(),
        "{context}: query named a source context for content it did not match"
    );

    match store.clone().get_metadata(partition, address).await {
        Ok(result) => {
            require_eq!(
                result.match_made,
                StoreMatch::MatchNone,
                "{context}: get_metadata reported a match for content the store does not hold"
            );
            require_eq!(
                result.partition,
                Partition::default(),
                "{context}: get_metadata named a source partition for content it did not match"
            );
        }
        Err(ref err) if is_absent(err) => {}
        Err(err) => return Err(format!("{context}: get_metadata failed: {err:?}")),
    }

    require!(
        store.clone().get(partition, address).await.is_err(),
        "{context}: get served content the store does not hold"
    );

    Ok(())
}

async fn an_unknown_address_matches_nothing(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, _payload, _fragment) = unique_content();
    assert_absent(store, partition, address, caps, "never stored").await
}

/// Clause 1 at its strongest: a full match is a promise that the payload comes back.
async fn a_stored_address_matches_fully(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload.clone()).await?;

    let resolved = query_one(store, partition, address)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;
    require_eq!(
        resolved.match_made,
        StoreMatch::MatchFull,
        "stored content must report a full match"
    );
    require_eq!(
        resolved.partition,
        partition,
        "a full match must name the partition it was found in"
    );
    require!(
        resolved.context == address.context || resolved.context.is_zero(),
        "a full match named {:?} as the context it was found under, but the association it matched \
         is the one asked about",
        resolved.context
    );

    let metadata = store
        .clone()
        .get_metadata(partition, address)
        .await
        .map_err(|err| format!("get_metadata failed: {err:?}"))?;
    require_eq!(
        metadata.match_made,
        StoreMatch::MatchFull,
        "get_metadata must agree with resolve about stored content"
    );
    require_eq!(
        metadata.fragment.size_content,
        fragment.size_content,
        "get_metadata must report the size of the content that was stored"
    );

    let (_fragment, bytes) = store
        .clone()
        .get(partition, address)
        .await
        .and_then(StoreGetData::into_payload)
        .map_err(|err| format!("get failed after a full match: {err:?}"))?;
    require!(
        bytes.as_ref() == payload.as_ref(),
        "get returned different bytes than were stored"
    );

    Ok(())
}

/// The same content under a different context in the same partition. A store may resolve this to a
/// partition match or report less if establishing more is not worth the lookup, but never to a full
/// match, because the association the caller asked about does not exist.
///
/// Whether the payload is *served* is the store's own [`ImmutableStore::read_scope`], and both
/// answers are checked against what it declared: a store reading no wider than the exact
/// association must refuse, and one reading wider must serve and describe.
async fn another_context_in_the_same_partition_never_matches_fully(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    let other = Address {
        hash: address.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };

    let resolved = query_one(store, partition, other)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;
    let found = resolved.match_made;
    require!(
        found <= StoreMatch::MatchPartition,
        "a different context in the same partition reported {found:?}, but no association exists \
         for it"
    );

    let served = store.clone().get(partition, other).await.is_ok();
    if store.read_scope() == StoreMatch::MatchFull {
        require!(
            !served,
            "a store that reads only exact associations served a sibling context's payload"
        );
    } else {
        require!(
            served,
            "a store reading past the context refused a sibling context it holds the payload for"
        );
    }
    if found != StoreMatch::MatchNone {
        require_eq!(
            resolved.partition,
            partition,
            "a match inside the partition asked about named a different one as its source"
        );
        require!(
            resolved.context == address.context || resolved.context.is_zero(),
            "a partition match named {:?} as its context, but the only association the store holds \
             for this hash is under {:?}",
            resolved.context,
            address.context
        );
    }

    match store.clone().get_metadata(partition, other).await {
        Ok(result) => {
            require!(
                result.match_made != StoreMatch::MatchFull,
                "get_metadata claimed a full match for an association that does not exist"
            );
            require!(
                !served || result.match_made != StoreMatch::MatchNone,
                "get_metadata reported nothing for content the store just served the payload of"
            );
        }
        Err(ref err) if is_absent(err) => require!(
            !served,
            "get_metadata reported absence for content the store just served the payload of"
        ),
        Err(err) => return Err(format!("get_metadata failed: {err:?}")),
    }

    Ok(())
}

/// The same content in a partition the caller has no claim to. In process a store may say the hash
/// exists; across a wire it must say nothing at all, because the existence of another tenant's
/// content is not the caller's to learn.
async fn another_partition_never_matches_above_the_hash(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    let elsewhere = Partition::from(rand::random::<[u8; 16]>());
    let resolved = query_one(store, elsewhere, address)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;

    if caps.over_wire {
        require_eq!(
            resolved.match_made,
            StoreMatch::MatchNone,
            "a hash held only by another partition leaked across a trust boundary"
        );
    } else {
        require!(
            resolved.match_made <= StoreMatch::MatchHash,
            "another partition reported {:?}, but holds no association for this address",
            resolved.match_made
        );
    }

    // A store that will not read across a partition must not name one either: the two are the same
    // permission, and a source it would refuse to serve from is a copy the caller cannot make.
    if store.isolates_partitions() {
        require_eq!(
            resolved.match_made,
            StoreMatch::MatchNone,
            "a store that isolates partitions reported a match held only by another one"
        );
    }

    // And a store that does read across must say which partition it read, or the caller has a
    // shortcut it cannot name a source for.
    if resolved.match_made == StoreMatch::MatchHash {
        require_eq!(
            resolved.partition,
            partition,
            "a hash match named the wrong partition as its source"
        );
    }

    Ok(())
}

/// Clause 3. Obliteration is a compliance obligation, so it binds every method rather than the ones
/// that happen to consult lifecycle state.
async fn an_obliterated_address_matches_nothing(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    store
        .clone()
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .map_err(|err| format!("obliterate failed: {err:?}"))?;

    assert_absent(store, partition, address, caps, "obliterated").await
}

/// The narrow case where the methods diverge today: content whose hash carries a tombstone, but
/// which has an association again because something re-stored it.
///
/// This asserts only that the methods **agree**, not what they agree on. Whether a store may accept
/// content back over a tombstone is policy that has not been decided; that two callers asking the
/// same store the same question get opposite answers is not policy, it is a defect.
async fn the_methods_agree_after_an_obliteration(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload.clone()).await?;

    store
        .clone()
        .obliterate(
            partition,
            address,
            Arc::new(StoreObliterateStats::default()),
        )
        .await
        .map_err(|err| format!("obliterate failed: {err:?}"))?;

    // A re-store may legitimately fail — refusing content over a tombstone is a defensible policy.
    // What follows compares the answers, and only when the re-store succeeded.
    if store
        .clone()
        .put(partition, address, fragment, Some(payload), false)
        .await
        .is_err()
    {
        return Ok(());
    }

    let resolved = best_match(store, partition, address, caps).await? != StoreMatch::MatchNone;
    let described = store
        .clone()
        .get_metadata(partition, address)
        .await
        .is_ok_and(|result| result.match_made != StoreMatch::MatchNone);

    require!(
        resolved == described,
        "resolve and get_metadata disagree about content re-stored over a tombstone: resolve says \
         {resolved}, get_metadata says {described}"
    );

    Ok(())
}

/// Clause 5's second half. A hash the asked partition holds is reported as being there, even when
/// another partition holds it too — a store that searched in some other order satisfies every other
/// clause and still points a copy at a partition the caller may have no claim to.
async fn a_match_prefers_the_partition_it_was_asked_about(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    // A store reached over a wire is scoped to one partition by construction - a session is opened
    // per partition - so it cannot be put into the state this needs, and the collapse at the trust
    // boundary means it could never name a foreign partition anyway.
    if caps.over_wire {
        return Ok(());
    }

    let (asked, address, payload, fragment) = unique_content();
    let elsewhere = Partition::from(rand::random::<[u8; 16]>());

    store_content(store, asked, address, fragment, payload.clone()).await?;
    let other_context = Address {
        hash: address.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };
    store_content(store, elsewhere, other_context, fragment, payload).await?;

    let third = Address {
        hash: address.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };
    let resolved = query_one(store, asked, third)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;

    if resolved.match_made != StoreMatch::MatchNone {
        require_eq!(
            resolved.partition,
            asked,
            "the partition asked about holds this hash, so it is the one to name"
        );
    }

    Ok(())
}

/// What a partition match is for. The level says the payload is already in the partition, so the
/// address can be registered with a copy rather than a transfer — which is only true if the source
/// the match names is one the store will copy from.
///
/// Both forms are exercised: the source as reported, and the same source with its context dropped,
/// which is what a caller has when the answer named no context. A store may under-report the level
/// and is exempt then, since a caller reading `MatchNone` transfers the payload as before.
async fn the_source_a_match_names_can_be_copied_from(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    let wanted = Address {
        hash: address.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };
    let resolved = query_one(store, partition, wanted)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;
    if resolved.match_made != StoreMatch::MatchPartition {
        return Ok(());
    }

    for source in [
        resolved.source_address(address.hash),
        Address::zero_context_hash(address.hash),
    ] {
        store
            .clone()
            .copy(resolved.partition, source, partition, wanted.context, false)
            .await
            .map_err(|err| {
                format!("a partition match named {source} as a source, which copy refused: {err:?}")
            })?;
    }

    require_eq!(
        best_match(store, partition, wanted, caps).await?,
        StoreMatch::MatchFull,
        "the address a copy registered does not resolve to the association it created"
    );

    Ok(())
}

/// Clause 3 meets clause 4. Obliteration removes one reference, not the content: another context
/// still holding the hash reads as before, while the obliterated one is gone through every method.
async fn obliterating_one_reference_leaves_the_others_readable(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    // The read of the obliterated reference below is a miss, so a store that a miss poisons never
    // answers the read after it.
    if caps.miss_poisons_session {
        return Ok(());
    }

    let (partition, doomed, payload, fragment) = unique_content();
    let survivor = Address {
        hash: doomed.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };

    store_content(store, partition, doomed, fragment, payload.clone()).await?;
    store_content(store, partition, survivor, fragment, payload.clone()).await?;

    store
        .clone()
        .obliterate(partition, doomed, Arc::new(StoreObliterateStats::default()))
        .await
        .map_err(|err| format!("obliterate failed: {err:?}"))?;

    assert_absent(store, partition, doomed, caps, "obliterated reference").await?;

    let (_fragment, bytes) = store
        .clone()
        .get(partition, survivor)
        .await
        .and_then(StoreGetData::into_payload)
        .map_err(|err| format!("a surviving reference stopped reading: {err:?}"))?;
    require!(
        bytes.as_ref() == payload.as_ref(),
        "a surviving reference read back different bytes"
    );

    Ok(())
}

/// Clause 1, on the side people expect to be a defect and is not.
///
/// A store may hold the representation without the bytes — the local store takes a `put` with no
/// payload, and a composite configured `with_local_metadata_only` writes it nothing else. Such an
/// entry reports a full match, because the association is real, and describes itself, because the
/// fragment is right there. What it cannot do is serve, and the failing `get` is load-bearing: it
/// is how the layer above knows to fetch the payload from upstream. A store that reported
/// `MatchNone` here instead would look more conservative and would break that fallback, so this
/// pins the behaviour rather than forbidding it.
async fn a_metadata_only_entry_is_described_but_not_served(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, _payload, fragment) = unique_content();

    store
        .clone()
        .put(partition, address, fragment, None, false)
        .await
        .map_err(|err| format!("put without a payload failed on {}: {err:?}", caps.label))?;

    let resolved = query_one(store, partition, address)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;
    require_eq!(
        resolved.match_made,
        StoreMatch::MatchFull,
        "a representation held without its payload is still an association"
    );
    require!(
        !resolved.stored_local,
        "a store holding no payload must not report it as local"
    );

    let described = store
        .clone()
        .get_metadata(partition, address)
        .await
        .map_err(|err| format!("get_metadata failed: {err:?}"))?;
    require_eq!(
        described.match_made,
        StoreMatch::MatchFull,
        "get_metadata must describe a representation the store holds"
    );
    require_eq!(
        described.fragment.size_content,
        fragment.size_content,
        "get_metadata must report the size the representation records"
    );

    require!(
        store.clone().get(partition, address).await.is_err(),
        "get returned something for a fragment whose payload was never stored — the layer above \
         reads that failure as its signal to fetch from upstream"
    );

    Ok(())
}

/// Clause 4 applied across the two read methods. `get` and `get_metadata` both describe the same
/// stored payload and must agree on what it is. A store that reads `size_content` from one
/// backend for `get_metadata` and from another for `get` will diverge when those backends fall
/// out of sync — this pins the agreement rather than assuming it.
async fn get_and_get_metadata_report_the_same_fragment(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    let described = store
        .clone()
        .get_metadata(partition, address)
        .await
        .map_err(|err| format!("get_metadata failed on {}: {err:?}", caps.label))?;

    let (served_fragment, _bytes) = store
        .clone()
        .get(partition, address)
        .await
        .and_then(StoreGetData::into_payload)
        .map_err(|err| format!("get failed on {}: {err:?}", caps.label))?;

    require_eq!(
        described.fragment.size_content,
        served_fragment.size_content,
        "get_metadata and get reported different size_content for the same stored payload"
    );
    require_eq!(
        described.fragment.size_payload,
        served_fragment.size_payload,
        "get_metadata and get reported different size_payload for the same stored payload"
    );

    Ok(())
}

/// Clause 1 applied to the durability bookkeeping. A full match promises the association is real
/// and the representation is present; `stored_local` and `stored_durable` say where. A store that
/// reports a full match while clearing both flags has hidden whether the payload is available
/// locally, durably, or nowhere — callers use those flags to decide whether to fetch again.
async fn stored_content_is_accounted_for(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload).await?;

    let resolved = query_one(store, partition, address)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;

    require_eq!(
        resolved.match_made,
        StoreMatch::MatchFull,
        "stored content must report a full match (prerequisite for durability check)"
    );
    require!(
        resolved.stored_local || resolved.stored_durable,
        "a full match for stored content must set stored_local or stored_durable: \
         stored_local={}, stored_durable={}",
        resolved.stored_local,
        resolved.stored_durable
    );

    Ok(())
}

/// A copy creates an association the store can serve, not just one it can report. The partner
/// check `NamedSourceCanBeCopied` establishes that the copy operation succeeds and that `query`
/// reports a full match afterwards. This check establishes that `get` also returns the original
/// bytes — a store that registers the `DynamoDB` row without arranging for the payload to be
/// accessible satisfies `NamedSourceCanBeCopied` and breaks this one.
async fn a_copied_address_is_servable(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload.clone()).await?;

    let wanted = Address {
        hash: address.hash,
        context: Context::from(rand::random::<[u8; 16]>()),
    };
    let resolved = query_one(store, partition, wanted)
        .await
        .map_err(|err| format!("query failed on {}: {err:?}", caps.label))?;

    if resolved.match_made != StoreMatch::MatchPartition {
        // Under-reporting is allowed; if the store does not offer the copy shortcut a caller
        // transfers the payload instead, which is always safe. Skip rather than fail.
        return Ok(());
    }

    let source = resolved.source_address(address.hash);
    store
        .clone()
        .copy(resolved.partition, source, partition, wanted.context, false)
        .await
        .map_err(|err| format!("copy from a named source failed: {err:?}"))?;

    let (_frag, served) = store
        .clone()
        .get(partition, wanted)
        .await
        .and_then(StoreGetData::into_payload)
        .map_err(|err| format!("get on a copied address failed: {err:?}"))?;

    require!(
        served.as_ref() == payload.as_ref(),
        "get on a copied address returned different bytes than were originally stored"
    );

    Ok(())
}

/// A copy is a new registration, not a move. The local store test
/// `copy_same_partition_new_context_adopts_payload_without_transfer` demonstrates this: after
/// copying from a source address to a new context, `get` on the source returns the same bytes as
/// before. A store that removes or poisons the source entry after a copy would break every caller
/// that holds an existing reference to that address.
async fn a_copy_leaves_the_source_readable(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, address, payload, fragment) = unique_content();
    store_content(store, partition, address, fragment, payload.clone()).await?;

    let destination_context = Context::from(rand::random::<[u8; 16]>());
    store
        .clone()
        .copy(partition, address, partition, destination_context, false)
        .await
        .map_err(|err| {
            format!(
                "copy from the stored address failed on {}: {err:?}",
                caps.label
            )
        })?;

    let (_frag, source_bytes) = store
        .clone()
        .get(partition, address)
        .await
        .and_then(StoreGetData::into_payload)
        .map_err(|err| format!("get on source after copy failed on {}: {err:?}", caps.label))?;

    require!(
        source_bytes.as_ref() == payload.as_ref(),
        "get on the source address returned different bytes after copying from it"
    );

    Ok(())
}

/// Positional correspondence, including duplicates. A caller pairs results back up with the
/// addresses it sent by index, and has nothing else to pair them by.
async fn batch_results_line_up_with_their_addresses(
    store: &Arc<dyn ImmutableStore>,
    caps: &Capabilities,
) -> Result<(), String> {
    let (partition, known, payload, fragment) = unique_content();
    let (_elsewhere, unknown, _payload, _fragment) = unique_content();

    let expect_known = if caps.can_put {
        store_content(store, partition, known, fragment, payload).await?;
        StoreMatch::MatchFull
    } else {
        StoreMatch::MatchNone
    };

    let addresses = [known, unknown, known, unknown];
    let mut results = [StoreMatchResult::default(); 4];
    store
        .clone()
        .query(partition, &addresses, &mut results)
        .await
        .map_err(|err| format!("resolve failed: {err:?}"))?;

    for (index, (result, address)) in results.iter().zip(addresses.iter()).enumerate() {
        let expected = if *address == known {
            expect_known
        } else {
            StoreMatch::MatchNone
        };
        require_eq!(
            result.match_made,
            expected,
            "resolve result {index} does not belong to the address at that position"
        );
    }

    Ok(())
}
