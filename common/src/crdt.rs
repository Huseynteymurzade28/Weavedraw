//! Conflict-free replication primitives.
//!
//! The whiteboard document is an **LWW-Element-Set** keyed by [`StrokeId`].
//! Every mutation is stamped with a [`Timestamp`] from a per-peer
//! [`LamportClock`]; because timestamps are totally ordered and unique
//! (`(counter, client_id)`), any two replicas that have seen the same set of
//! [`StrokeOp`]s — in *any* order, with *any* duplication — converge to the
//! same visible set of strokes and the same draw order.
//!
//! Strokes are immutable once created, so the CRDT only tracks membership:
//! an *add* carries the payload, a *remove* leaves a tombstone. An element is
//! visible when its newest add is strictly newer than its newest remove
//! (remove-biased on the impossible tie).

use std::collections::{BTreeMap, HashMap, hash_map::Entry};

use serde::{Deserialize, Serialize};

use crate::types::{ClientId, Stroke, StrokeId};

/// A Lamport timestamp. Ordering is `counter` first, then `client` as a
/// deterministic tie-breaker, which makes every timestamp in the system unique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp {
    pub counter: u64,
    pub client: ClientId,
}

impl Timestamp {
    pub const fn new(counter: u64, client: ClientId) -> Self {
        Self { counter, client }
    }
}

/// Per-peer logical clock. Call [`tick`](Self::tick) to stamp a local
/// operation and [`observe`](Self::observe) for every remote timestamp seen,
/// so local operations always sort after everything already known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LamportClock {
    client: ClientId,
    counter: u64,
}

impl LamportClock {
    pub fn new(client: ClientId) -> Self {
        Self { client, counter: 0 }
    }

    pub fn client(&self) -> ClientId {
        self.client
    }

    /// Current counter without advancing it.
    pub fn now(&self) -> u64 {
        self.counter
    }

    /// Advance the clock and return a fresh, unique timestamp.
    pub fn tick(&mut self) -> Timestamp {
        self.counter += 1;
        Timestamp::new(self.counter, self.client)
    }

    /// Merge a remote timestamp so subsequent ticks are causally after it.
    pub fn observe(&mut self, ts: Timestamp) {
        self.counter = self.counter.max(ts.counter);
    }
}

/// A replicated mutation. This is the unit sent over the wire and persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StrokeOp {
    /// Insert (or re-insert after an undo of a delete) a stroke.
    Add { stroke: Stroke, ts: Timestamp },
    /// Tombstone a stroke.
    Remove { id: StrokeId, ts: Timestamp },
}

impl StrokeOp {
    pub fn id(&self) -> StrokeId {
        match self {
            StrokeOp::Add { stroke, .. } => stroke.id,
            StrokeOp::Remove { id, .. } => *id,
        }
    }

    pub fn timestamp(&self) -> Timestamp {
        match self {
            StrokeOp::Add { ts, .. } | StrokeOp::Remove { ts, .. } => *ts,
        }
    }
}

/// The newest *add* observed for an element, with its payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Added {
    pub ts: Timestamp,
    pub stroke: Stroke,
}

/// Replication state for one element. Kept even when tombstoned so that a
/// late-arriving add can be correctly out-voted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrokeEntry {
    /// `None` if only a tombstone has been seen so far.
    pub add: Option<Added>,
    /// Newest remove timestamp, if any.
    pub removed: Option<Timestamp>,
}

impl StrokeEntry {
    pub fn is_visible(&self) -> bool {
        match (&self.add, self.removed) {
            (Some(a), Some(r)) => a.ts > r,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    fn add_ts(&self) -> Option<Timestamp> {
        self.add.as_ref().map(|a| a.ts)
    }
}

/// LWW-Element-Set of strokes with a maintained draw-order index.
///
/// Serialises as a flat `Vec<(StrokeId, StrokeEntry)>` — the index is
/// rebuilt on load — so the same type doubles as the on-disk snapshot and
/// the "full sync" payload sent to newly joined peers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(
    from = "Vec<(StrokeId, StrokeEntry)>",
    into = "Vec<(StrokeId, StrokeEntry)>"
)]
pub struct StrokeSet {
    entries: HashMap<StrokeId, StrokeEntry>,
    /// Visible strokes keyed by their add timestamp: iteration order == draw order.
    order: BTreeMap<Timestamp, StrokeId>,
}

impl StrokeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one operation. Returns `true` if the *visible* state changed
    /// (a stroke appeared, disappeared, or moved in draw order) — callers use
    /// this to decide whether to repaint or rebroadcast.
    pub fn apply(&mut self, op: StrokeOp) -> bool {
        let id = op.id();
        let entry = self.entries.entry(id).or_default();
        let was_visible = entry.is_visible();
        let old_key = entry.add_ts();

        match op {
            StrokeOp::Add { stroke, ts } => {
                debug_assert_eq!(stroke.id, id);
                if entry.add.as_ref().is_none_or(|a| ts > a.ts) {
                    entry.add = Some(Added { ts, stroke });
                }
            }
            StrokeOp::Remove { ts, .. } => {
                if entry.removed.is_none_or(|r| ts > r) {
                    entry.removed = Some(ts);
                }
            }
        }

        let now_visible = entry.is_visible();
        let new_key = entry.add_ts();
        let changed = was_visible != now_visible || (now_visible && old_key != new_key);

        if changed {
            if was_visible {
                if let Some(k) = old_key {
                    self.order.remove(&k);
                }
            }
            if now_visible {
                if let Some(k) = new_key {
                    self.order.insert(k, id);
                }
            }
        }
        changed
    }

    /// Apply a batch, returning how many ops changed visible state.
    pub fn apply_all(&mut self, ops: impl IntoIterator<Item = StrokeOp>) -> usize {
        ops.into_iter().filter(|op| self.apply(op.clone())).count()
    }

    /// Merge another replica into this one (state-based sync). Commutative,
    /// associative and idempotent. Returns how many elements changed visibility.
    pub fn merge(&mut self, other: StrokeSet) -> usize {
        let mut changed = 0;
        for (id, entry) in other.entries {
            if let Some(Added { ts, stroke }) = entry.add {
                changed += usize::from(self.apply(StrokeOp::Add { stroke, ts }));
            }
            if let Some(ts) = entry.removed {
                changed += usize::from(self.apply(StrokeOp::Remove { id, ts }));
            }
        }
        changed
    }

    /// Visible strokes in draw order (oldest first).
    pub fn visible(&self) -> impl Iterator<Item = &Stroke> + '_ {
        self.order.values().filter_map(|id| self.get(*id))
    }

    /// Visible strokes in draw order, paired with their add timestamp.
    pub fn visible_with_ts(&self) -> impl Iterator<Item = (Timestamp, &Stroke)> + '_ {
        self.order
            .iter()
            .filter_map(|(ts, id)| self.get(*id).map(|s| (*ts, s)))
    }

    /// Look up a stroke by id, only if it is currently visible.
    pub fn get(&self, id: StrokeId) -> Option<&Stroke> {
        let entry = self.entries.get(&id)?;
        entry
            .is_visible()
            .then(|| entry.add.as_ref().map(|a| &a.stroke))
            .flatten()
    }

    /// The payload for an id regardless of visibility (used to undo a delete).
    pub fn get_any(&self, id: StrokeId) -> Option<&Stroke> {
        self.entries.get(&id)?.add.as_ref().map(|a| &a.stroke)
    }

    pub fn contains(&self, id: StrokeId) -> bool {
        self.get(id).is_some()
    }

    /// Number of visible strokes.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Total elements tracked, including tombstones.
    pub fn tracked_len(&self) -> usize {
        self.entries.len()
    }

    /// All entries including tombstones (for persistence / debugging).
    pub fn entries(&self) -> impl Iterator<Item = (&StrokeId, &StrokeEntry)> + '_ {
        self.entries.iter()
    }

    /// The largest timestamp in the set. A freshly joined peer feeds this into
    /// [`LamportClock::observe`] so its first local op sorts after history.
    pub fn latest_timestamp(&self) -> Option<Timestamp> {
        self.entries
            .values()
            .flat_map(|e| e.add_ts().into_iter().chain(e.removed))
            .max()
    }

    /// Build the op that undoes `op`, stamped with `ts`. Undoing a remove
    /// requires the payload, which is `None` if we only ever saw a tombstone.
    pub fn inverse(&self, op: &StrokeOp, ts: Timestamp) -> Option<StrokeOp> {
        match op {
            StrokeOp::Add { stroke, .. } => Some(StrokeOp::Remove { id: stroke.id, ts }),
            StrokeOp::Remove { id, .. } => self.get_any(*id).map(|s| StrokeOp::Add {
                stroke: s.clone(),
                ts,
            }),
        }
    }

    /// Ops that would tombstone every currently visible stroke ("clear").
    pub fn clear_ops(&self, clock: &mut LamportClock) -> Vec<StrokeOp> {
        self.order
            .values()
            .map(|id| StrokeOp::Remove {
                id: *id,
                ts: clock.tick(),
            })
            .collect()
    }

    /// Ops that would tombstone every visible stroke drawn by `client`.
    pub fn clear_ops_for(&self, client: ClientId, clock: &mut LamportClock) -> Vec<StrokeOp> {
        self.visible()
            .filter(|s| s.client_id == client)
            .map(|s| StrokeOp::Remove {
                id: s.id,
                ts: clock.tick(),
            })
            .collect()
    }
}

impl PartialEq for StrokeSet {
    /// Equal when replication state is identical (the index is derived).
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl From<Vec<(StrokeId, StrokeEntry)>> for StrokeSet {
    fn from(entries: Vec<(StrokeId, StrokeEntry)>) -> Self {
        let mut set = StrokeSet::new();
        for (id, entry) in entries {
            match set.entries.entry(id) {
                Entry::Vacant(v) => {
                    if entry.is_visible() {
                        if let Some(ts) = entry.add_ts() {
                            set.order.insert(ts, id);
                        }
                    }
                    v.insert(entry);
                }
                // Duplicate ids in the input: fold them in via the LWW rules.
                Entry::Occupied(_) => {
                    if let Some(Added { ts, stroke }) = entry.add {
                        set.apply(StrokeOp::Add { stroke, ts });
                    }
                    if let Some(ts) = entry.removed {
                        set.apply(StrokeOp::Remove { id, ts });
                    }
                }
            }
        }
        set
    }
}

impl From<StrokeSet> for Vec<(StrokeId, StrokeEntry)> {
    fn from(set: StrokeSet) -> Self {
        set.entries.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Point, Rgba};
    use uuid::Uuid;

    fn peer() -> (ClientId, LamportClock) {
        let id = Uuid::new_v4();
        (id, LamportClock::new(id))
    }

    fn stroke(client: ClientId) -> Stroke {
        Stroke::new(client, Rgba::WHITE, 4.0)
            .with_points([Point::new(0.0, 0.0), Point::new(1.0, 1.0)])
    }

    #[test]
    fn clock_ticks_are_strictly_increasing_and_observe_merges() {
        let (_, mut clock) = peer();
        let a = clock.tick();
        let b = clock.tick();
        assert!(b > a);
        clock.observe(Timestamp::new(100, Uuid::new_v4()));
        assert!(clock.tick().counter > 100);
    }

    #[test]
    fn timestamps_are_totally_ordered_with_client_tiebreak() {
        let lo = Uuid::from_u128(1);
        let hi = Uuid::from_u128(2);
        assert!(Timestamp::new(1, hi) < Timestamp::new(2, lo));
        assert!(Timestamp::new(1, lo) < Timestamp::new(1, hi));
    }

    #[test]
    fn add_then_remove_hides_stroke() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let s = stroke(me);
        let id = s.id;
        assert!(set.apply(StrokeOp::Add {
            stroke: s,
            ts: clock.tick()
        }));
        assert_eq!(set.len(), 1);
        assert!(set.apply(StrokeOp::Remove {
            id,
            ts: clock.tick()
        }));
        assert_eq!(set.len(), 0);
        assert!(
            set.get_any(id).is_some(),
            "payload retained behind tombstone"
        );
    }

    #[test]
    fn remove_arriving_before_add_still_wins_if_newer() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let s = stroke(me);
        let add_ts = clock.tick();
        let rm_ts = clock.tick();
        assert!(!set.apply(StrokeOp::Remove {
            id: s.id,
            ts: rm_ts
        }));
        assert!(!set.apply(StrokeOp::Add {
            stroke: s,
            ts: add_ts
        }));
        assert!(set.is_empty());
        assert_eq!(set.tracked_len(), 1);
    }

    #[test]
    fn re_add_after_remove_restores_stroke_at_new_position() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let first = stroke(me);
        let second = stroke(me);
        let first_id = first.id;
        set.apply(StrokeOp::Add {
            stroke: first.clone(),
            ts: clock.tick(),
        });
        set.apply(StrokeOp::Add {
            stroke: second.clone(),
            ts: clock.tick(),
        });
        set.apply(StrokeOp::Remove {
            id: first_id,
            ts: clock.tick(),
        });
        // Undo the delete: re-add with a fresh timestamp.
        set.apply(StrokeOp::Add {
            stroke: first,
            ts: clock.tick(),
        });
        let order: Vec<_> = set.visible().map(|s| s.id).collect();
        assert_eq!(
            order,
            vec![second.id, first_id],
            "re-added stroke draws on top"
        );
    }

    #[test]
    fn stale_add_does_not_resurrect() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let s = stroke(me);
        let t1 = clock.tick();
        let t2 = clock.tick();
        set.apply(StrokeOp::Add {
            stroke: s.clone(),
            ts: t1,
        });
        set.apply(StrokeOp::Remove { id: s.id, ts: t2 });
        // A duplicate of the original add (e.g. redelivered) must not undo the remove.
        assert!(!set.apply(StrokeOp::Add { stroke: s, ts: t1 }));
        assert!(set.is_empty());
    }

    #[test]
    fn ops_commute_and_are_idempotent() {
        let (a, mut ca) = peer();
        let (b, mut cb) = peer();
        let sa = stroke(a);
        let sb = stroke(b);
        let ops = [
            StrokeOp::Add {
                stroke: sa.clone(),
                ts: ca.tick(),
            },
            StrokeOp::Add {
                stroke: sb.clone(),
                ts: cb.tick(),
            },
            StrokeOp::Remove {
                id: sa.id,
                ts: cb.tick(),
            },
            StrokeOp::Add {
                stroke: sa.clone(),
                ts: {
                    ca.observe(Timestamp::new(cb.now(), b));
                    ca.tick()
                },
            },
        ];

        let mut forward = StrokeSet::new();
        forward.apply_all(ops.iter().cloned());
        let mut backward = StrokeSet::new();
        backward.apply_all(ops.iter().rev().cloned());
        let mut doubled = StrokeSet::new();
        doubled.apply_all(ops.iter().cloned());
        doubled.apply_all(ops.iter().cloned());

        let ids = |s: &StrokeSet| s.visible().map(|x| x.id).collect::<Vec<_>>();
        assert_eq!(ids(&forward), ids(&backward));
        assert_eq!(ids(&forward), ids(&doubled));
        assert_eq!(ids(&forward), vec![sb.id, sa.id]);
    }

    #[test]
    fn merge_converges_both_directions() {
        let (a, mut ca) = peer();
        let (b, mut cb) = peer();
        let mut left = StrokeSet::new();
        let mut right = StrokeSet::new();
        let sa = stroke(a);
        let sb = stroke(b);
        left.apply(StrokeOp::Add {
            stroke: sa.clone(),
            ts: ca.tick(),
        });
        right.apply(StrokeOp::Add {
            stroke: sb.clone(),
            ts: cb.tick(),
        });
        right.apply(StrokeOp::Remove {
            id: sa.id,
            ts: cb.tick(),
        });

        let mut lr = left.clone();
        lr.merge(right.clone());
        let mut rl = right.clone();
        rl.merge(left.clone());

        let ids = |s: &StrokeSet| s.visible().map(|x| x.id).collect::<Vec<_>>();
        assert_eq!(ids(&lr), ids(&rl));
        assert_eq!(ids(&lr), vec![sb.id]);
    }

    #[test]
    fn inverse_ops_round_trip() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let s = stroke(me);
        let add = StrokeOp::Add {
            stroke: s.clone(),
            ts: clock.tick(),
        };
        set.apply(add.clone());

        let undo = set.inverse(&add, clock.tick()).unwrap();
        assert!(matches!(undo, StrokeOp::Remove { id, .. } if id == s.id));
        set.apply(undo.clone());
        assert!(set.is_empty());

        let redo = set.inverse(&undo, clock.tick()).unwrap();
        assert!(matches!(&redo, StrokeOp::Add { stroke, .. } if *stroke == s));
        set.apply(redo);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn clear_ops_tombstone_everything_visible() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        for _ in 0..3 {
            set.apply(StrokeOp::Add {
                stroke: stroke(me),
                ts: clock.tick(),
            });
        }
        let ops = set.clear_ops(&mut clock);
        assert_eq!(ops.len(), 3);
        set.apply_all(ops);
        assert!(set.is_empty());
        assert_eq!(set.tracked_len(), 3);
    }

    #[test]
    fn latest_timestamp_covers_tombstones() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let s = stroke(me);
        set.apply(StrokeOp::Add {
            stroke: s.clone(),
            ts: clock.tick(),
        });
        let rm = clock.tick();
        set.apply(StrokeOp::Remove { id: s.id, ts: rm });
        assert_eq!(set.latest_timestamp(), Some(rm));
    }

    #[test]
    fn serde_round_trip_rebuilds_draw_order() {
        let (me, mut clock) = peer();
        let mut set = StrokeSet::new();
        let mut expected = Vec::new();
        for _ in 0..5 {
            let s = stroke(me);
            expected.push(s.id);
            set.apply(StrokeOp::Add {
                stroke: s,
                ts: clock.tick(),
            });
        }
        set.apply(StrokeOp::Remove {
            id: expected.remove(2),
            ts: clock.tick(),
        });

        let bytes = crate::codec::encode(&set).unwrap();
        let restored: StrokeSet = crate::codec::decode(&bytes).unwrap();
        let order: Vec<_> = restored.visible().map(|s| s.id).collect();
        assert_eq!(order, expected);
        assert_eq!(restored.tracked_len(), 5);
    }
}
