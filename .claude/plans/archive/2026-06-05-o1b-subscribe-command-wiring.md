# O1-B Implementation Plan: SubscribeCommand wiring on WebSocketConnection

**Status:** READY (design locked, all dependencies in place)
**Predecessors shipped:** O1-A (`crates/core/src/instrument/phase2_scheduler.rs`), full notification machinery
**Risk:** HIGH — touches `WebSocketConnection::run()` hot path
**Estimated effort:** 1 focused session (~6 hours including tests)

## What this delivers

Today (after O1-A): the Phase 2 scheduler wakes at 09:12 IST, waits for LTPs,
emits `Phase2Complete { added_count: 0 }` Telegram. The 0 is honest — we
don't actually subscribe yet.

After O1-B: the scheduler computes the actual stock-F&O delta from
`SharedSpotPrices` + `FnoUniverse`, dispatches `SubscribeCommand`s to the
main-feed pool, each connection accepts and sends RequestCode 17/21 JSON
on its existing WebSocket (zero disconnect), and the real `added_count`
goes out on Telegram.

## Step-by-step changes

### 1. New type in `crates/core/src/websocket/mod.rs` (or `subscribe_command.rs`)

```rust
/// Runtime subscribe-add command for main-feed connections.
///
/// O1-B (2026-04-17): Added to support 09:12 IST Phase 2 stock-F&O
/// subscription via the existing pool (zero-disconnect, RequestCode 17/21).
/// Mirrors `DepthCommand::Swap20` / `DepthCommand::Swap200` for depth.
#[derive(Debug, Clone)]
pub enum SubscribeCommand {
    /// Subscribe these instruments on whichever pool connection has
    /// spare capacity. The pool dispatcher picks the connection.
    AddInstruments {
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
    },
}
```

### 2. `WebSocketConnection` struct additions

In `crates/core/src/websocket/connection.rs`, add to the struct:

```rust
/// O1-B: Optional runtime subscribe-command receiver. None on construction;
/// installed via `with_subscribe_channel`. The read loop's select! polls
/// this alongside the socket; on receipt it builds subscribe messages and
/// sends them via the existing write sink.
subscribe_cmd_rx: tokio::sync::Mutex<Option<mpsc::Receiver<SubscribeCommand>>>,
```

Initialize in `new()`: `subscribe_cmd_rx: tokio::sync::Mutex::new(None)`.

### 3. Builder method (mirrors `with_wal_spill`)

```rust
/// O1-B: attach a runtime subscribe-command channel.
#[must_use]
pub fn with_subscribe_channel(
    self,
    rx: mpsc::Receiver<SubscribeCommand>,
) -> Self {
    *self.subscribe_cmd_rx.try_lock()
        .expect("with_subscribe_channel called before run()") = Some(rx);
    self
}
```

### 4. Read-loop select! arm in `run()`

Inside the existing `tokio::select!` (around `connection.rs:592`):

```rust
maybe_cmd = async {
    match subscribe_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
} => {
    if let Some(SubscribeCommand::AddInstruments { instruments, feed_mode }) = maybe_cmd {
        let messages = build_subscription_messages(
            &instruments,
            feed_mode,
            self.ws_config.subscription_batch_size,
        );
        let mut sink = write.lock().await;
        for msg in &messages {
            if let Err(e) = sink.send(Message::Text(msg.into())).await {
                error!(?e, "O1-B: subscribe-command send failed");
                break;
            }
        }
        // Update self.instruments so reconnect cycles include them.
    }
}
```

Take the receiver once before the loop:
`let mut subscribe_rx = self.subscribe_cmd_rx.lock().await.take();`

### 5. Pool-level dispatch

In `crates/core/src/websocket/connection_pool.rs`:

```rust
/// O1-B: per-connection sender for runtime subscribe commands.
/// Populated by `new_with_optional_wal` if `enable_runtime_subscribe = true`.
subscribe_cmd_senders: Vec<mpsc::Sender<SubscribeCommand>>,
```

Add public method:

```rust
/// Dispatch a subscribe-command to the connection with the most spare
/// capacity. Returns the picked connection_id on success, None if all
/// connections are full or no command channel was wired.
pub async fn dispatch_subscribe(&self, cmd: SubscribeCommand) -> Option<usize> {
    // Find connection with smallest health.subscribed_count.
    let healths = self.health();
    let target_idx = healths
        .iter()
        .enumerate()
        .min_by_key(|(_, h)| h.subscribed_count)
        .map(|(i, _)| i)?;
    let max_per_conn = self.dhan_config.max_instruments_per_connection;
    if healths[target_idx].subscribed_count >= max_per_conn {
        return None; // pool truly full
    }
    let sender = self.subscribe_cmd_senders.get(target_idx)?;
    sender.try_send(cmd).ok()?;
    Some(target_idx)
}
```

### 6. Wire in `crates/app/src/main.rs` (both boot paths)

In the WebSocket-spawn block:

```rust
// O1-B: create per-connection subscribe channels alongside spawn.
let mut cmd_senders = Vec::with_capacity(MAX_WEBSOCKET_CONNECTIONS);
let mut conns_with_cmd = Vec::with_capacity(MAX_WEBSOCKET_CONNECTIONS);
for conn in pool.connections.iter() {
    let (tx, rx) = mpsc::channel(8);  // 8 commands buffered per conn
    cmd_senders.push(tx);
    // need to inject `with_subscribe_channel(rx)` BEFORE Arc-wrapping for spawn.
}
pool.set_subscribe_senders(cmd_senders);
```

### 7. Phase 2 scheduler delta computation

In `phase2_scheduler.rs`, replace the stubbed `planned_phase2_count = 0`
with real delta:

```rust
pub async fn compute_phase2_delta(
    spot_prices: &SharedSpotPrices,
    universe: &FnoUniverse,
    boot_subscribed: &HashSet<u32>,  // security_ids subscribed at boot
) -> Vec<InstrumentSubscription> {
    // For each stock with a fresh LTP, find ATM ± 25 strikes, return
    // those security_ids that are NOT already in boot_subscribed.
    // ...
}
```

### 8. Wire scheduler → pool dispatch

In `run_phase2_scheduler`, replace the stub log with:

```rust
let delta = compute_phase2_delta(&spot_prices, &universe, &boot_set).await;
let cmd = SubscribeCommand::AddInstruments {
    instruments: delta.clone(),
    feed_mode: FeedMode::Quote,
};
match pool.dispatch_subscribe(cmd).await {
    Some(conn_id) => {
        info!(conn_id, count = delta.len(), "O1-B: dispatched Phase 2 subscribe");
        notifier.notify(NotificationEvent::Phase2Complete {
            added_count: delta.len(),
            duration_ms,
        });
    }
    None => {
        notifier.notify(NotificationEvent::Phase2Failed {
            reason: "pool has no spare capacity".to_string(),
            attempts: 1,
        });
    }
}
```

### 9. Tests

- `test_subscribe_command_serializes_correctly` — RequestCode 17/21 JSON shape
- `test_pool_dispatch_picks_lowest_count_connection`
- `test_pool_dispatch_returns_none_when_full`
- `test_compute_phase2_delta_excludes_boot_subscribed`
- `test_compute_phase2_delta_skips_stocks_without_ltp`
- Integration: `phase2_end_to_end` with mock pool, mock LTPs, asserts
  `Phase2Complete` fires with correct `added_count`

### 10. Rollback plan

Each step is independently revertable. If steps 5-8 break, revert and the
scheduler reverts to alert-only mode (current O1-A behavior). Steps 1-4
are zero-behavior-change additions (no command sender wired = the new
select! arm is `pending` forever).

## Mechanical guards required

- New pub-fn count must stay flat (TEST-EXEMPT or matched test for each)
- `boot_path_notify_parity.rs`-style guard: assert both boot paths call
  `pool.set_subscribe_senders(...)` so FAST BOOT also wires the channel
- Hot-path scan: `select!` arm must not allocate per-frame (build_subscription_messages
  runs once per command, not per tick)

## Open questions for the next session

1. Should `feed_mode` for stock F&O be `Quote` or `Full`? Currently
   stock equities run `Full` per `subscription_planner.rs`; F&O could
   match. Confirm with Parthiban.
2. What's the boot-subscribed set's source of truth? Snapshot
   `SubscriptionPlan` at boot end and pass to scheduler.
3. Telegram batching: if delta is 5000+ instruments, do we want a
   per-connection summary alert? Current `Phase2Complete` is one event.
