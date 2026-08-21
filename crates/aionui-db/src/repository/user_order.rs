use async_trait::async_trait;

use crate::error::DbError;
use crate::models::{OrderItemType, OrderScene, UserOrderRow};

/// A `(item_type, item_id)` reference into the `user_order` table, used by pin
/// writes and cascade deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderItemRef {
    pub item_type: OrderItemType,
    pub item_id: String,
}

impl OrderItemRef {
    pub fn new(item_type: OrderItemType, item_id: impl Into<String>) -> Self {
        Self {
            item_type,
            item_id: item_id.into(),
        }
    }
}

/// Keyset cursor for paginating a scene's rows, ordered by
/// `(order_key, item_type, item_id)`. All three components are non-null, so
/// there is no NULL branch to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedCursor {
    pub order_key: i64,
    pub item_type: OrderItemType,
    pub item_id: String,
}

/// Result of a [`IUserOrderStore::pin`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// A new ordering row was inserted at the top of the scene.
    Inserted,
    /// The item was already pinned; the call was a no-op (order preserved).
    AlreadyPinned,
}

/// Result of a [`IUserOrderStore::move_item`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    /// `moved` was repositioned (its `order_key` was recomputed).
    Moved,
    /// `moved` has no row in the scene — a stale frontend window (→ 404).
    MovedNotFound,
    /// The `after` anchor has no row in the scene — a stale frontend window
    /// (→ 400); the client should refetch the pinned group.
    AfterNotFound,
}

/// Access boundary for the `user_order` table.
///
/// The store owns SQL and transactions; the `aionui-sidebar` service holds an
/// `Arc<dyn IUserOrderStore>` and never opens transactions itself. Every method
/// takes the acting `user_id` and filters/writes the owner column, so a user
/// can neither see nor mutate another user's ordering.
///
/// Pin state is row existence: [`pin`](Self::pin) inserts, [`unpin`](Self::unpin)
/// deletes. There is no boolean column.
#[async_trait]
pub trait IUserOrderStore: Send + Sync {
    /// Pin `item` at the top of `scene`: insert a row with
    /// `order_key = (scene min order_key) - 1000`, or `1000` when the scene is
    /// empty. Idempotent — if the row already exists it is left unchanged and
    /// [`PinOutcome::AlreadyPinned`] is returned. Uses `BEGIN IMMEDIATE` so the
    /// read-min-then-insert is atomic under concurrent pins.
    async fn pin(&self, user_id: &str, scene: OrderScene, item: &OrderItemRef) -> Result<PinOutcome, DbError>;

    /// Unpin `item` in `scene`: delete the row. Returns `true` if a row was
    /// removed, `false` if it was not pinned (idempotent no-op).
    async fn unpin(&self, user_id: &str, scene: OrderScene, item: &OrderItemRef) -> Result<bool, DbError>;

    /// Reposition `moved` within `scene` (drag-drop). `after = None` moves it to
    /// the top; otherwise it lands directly after `after`. The `order_key` is
    /// computed server-side (midpoint of the neighbours, whole-scene rebalance
    /// when the gap is exhausted) — callers never pass a key (BR-26). The whole
    /// read-neighbours → compute → write runs in one `BEGIN IMMEDIATE` write
    /// transaction so concurrent moves cannot interleave (R1-S6). Returns
    /// [`MoveOutcome::MovedNotFound`] / [`MoveOutcome::AfterNotFound`] when an
    /// anchor is absent (stale window), leaving the table unchanged.
    async fn move_item(
        &self,
        user_id: &str,
        scene: OrderScene,
        moved: &OrderItemRef,
        after: Option<&OrderItemRef>,
    ) -> Result<MoveOutcome, DbError>;

    /// One keyset page of a scene's rows, ordered by
    /// `(order_key, item_type, item_id)` ascending. `after = None` starts at the
    /// top; otherwise rows strictly after the cursor are returned. At most
    /// `limit` rows.
    async fn list_pinned(
        &self,
        user_id: &str,
        scene: OrderScene,
        after: Option<&PinnedCursor>,
        limit: i64,
    ) -> Result<Vec<UserOrderRow>, DbError>;

    /// Every pinned reference in `scene` for `user_id` (unpaged). Used to derive
    /// the DTO `pinned` flag and the anti-join exclusion set; the pinned set is
    /// bounded by user behavior, so an unpaged read is acceptable.
    async fn pinned_refs(&self, user_id: &str, scene: OrderScene) -> Result<Vec<OrderItemRef>, DbError>;

    /// Delete every `user_order` row for a single item across all scenes.
    /// Cascade for "the item left the sidebar" (conversation/team deletion).
    /// Idempotent.
    async fn remove_item(&self, user_id: &str, item: &OrderItemRef) -> Result<(), DbError>;

    /// Delete every `user_order` row for a batch of items across all scenes, in
    /// one transaction (atomic all-or-nothing). Cascade for project removal,
    /// where the removal set (including path-merged items) is computed by the
    /// service. Idempotent; empty input is a no-op.
    async fn remove_items(&self, user_id: &str, items: &[OrderItemRef]) -> Result<(), DbError>;
}
