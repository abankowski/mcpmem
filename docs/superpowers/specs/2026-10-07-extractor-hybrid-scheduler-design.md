# Hybrid Extractor Scheduler Design

## Goal

Remove the 250 ms idle SQLite poll. Start local extraction without a poll delay. Keep durable retry, lease recovery, upload expiry, and separate-process recovery.

## Evidence

`src/runtime.rs` runs one extractor turn, then sleeps for 250 ms. The turn logs every successful report. A report with all zero fields means that no extraction, retry, dead-letter action, or upload cleanup occurred.

The production service enables the `extractor` role and debug logging. Its repeated zero reports are idle polls. The alternating workspace IDs match the round-robin cursor. They do not show a process restart.

A retry is due after one second. A lost lease is due after 30 seconds. An unfinished upload expires after one hour. The scheduler must use a timer. A local event alone cannot recover a missed signal or wake a separate extractor process.

## Scope

- Wake the local extractor after a successful attachment upload commit.
- Wait for an upload wake or the earliest durable due time.
- Use a 30 second fallback poll for a separate process and a lost local wake.
- Stop the zero-report debug spam.
- Preserve one-job and one-workspace fairness per turn.

## Non-goals

- Do not add a configuration option.
- Do not add a SQLite migration.
- Do not add cross-process notifications.
- Do not change attachment states, retry delays, lease duration, or upload TTL.

## Components

### Extractor wake handle

`src/runtime.rs` defines a cloneable internal `ExtractorWake` over `Arc<tokio::sync::Notify>`. It has one `wake()` method. A wake is coalesced. One stored notification is enough because the worker reads the durable queue after every wake.

`main.rs` creates one wake handle when it enables the extractor role. It passes a clone to `ExtractorService` and to both attachment write surfaces.

If the extractor role is absent, attachment writes have no wake handle. This keeps the present separate-role behavior. A separate extractor discovers the durable job through its 30 second fallback poll.

### Attachment write surfaces

`src/attachment_actions.rs` stores an optional `ExtractorWake` in `ToolSettings`. It calls `wake()` only after `finish_attachment_upload` commits successfully.

`src/http.rs` adds the same optional handle to `HttpState`. `src/ui/api/attachments.rs` calls `wake()` only after `store_reader` returns a committed attachment.

A failed, cancelled, or incomplete upload does not wake the extractor. A wake never crosses a transaction boundary before the durable job exists.

### Durable due-time query

`crates/mcpmem-extractor/src/pdf.rs` adds an internal worker query that returns the earliest microsecond timestamp at which this graph needs a turn. It includes:

- the earliest due pending attachment job;
- the earliest expired leased attachment job;
- the earliest unfinished upload expiry.

The query uses the same live entity, revision, attachment state, and attempt predicates as `claim_due`. It returns `None` when no graph row needs a future turn.

The runtime scans registered graphs after an idle turn. It takes the earliest returned timestamp. It waits until that timestamp, but never longer than 30 seconds.

### Runtime loop

`ExtractorService` owns the wake receiver. A turn still selects one workspace through the existing cursor and processes at most one job.

After each turn, the runtime selects between the next local wake and a timer. The timer target is the earlier of the durable due time and the 30 second fallback deadline. A due timestamp in the past starts the next turn immediately.

The worker repeats immediate turns while any graph has due work. The existing cursor continues to rotate workspace selection. This prevents one graph from starving another graph.

### Logs

`extractor turn finished` is emitted only when `claimed`, `committed`, `retried`, `dead`, or `expired_sessions` is nonzero. A zero report is silent.

Existing error logs remain unchanged. They identify database or extraction failures without a high-rate success log.

## Failure handling

`Notify` can coalesce many local upload events. This is correct because the SQLite queue is the source of truth.

A local wake can be absent, lost before service start, or sent by another process. The 30 second fallback poll discovers the durable work in each case.

A retry, lease expiry, or upload expiry does not depend on a local wake. The durable due-time timer schedules each event.

A missing or deleted workspace keeps the current error and round-robin behavior. It does not end the extractor role.

## Test contract

1. A zero-report workspace turn emits no completion event.
2. A nonzero report emits one completion event with the report fields.
3. A completed MCP upload wakes a local extractor after the database commit.
4. A completed HTTP upload wakes a local extractor after the database commit.
5. The due-time query returns the earliest of pending retry, expired lease, and upload expiry.
6. A worker wakes at a due retry without a new upload event.
7. A worker with no local wake checks the durable queue within 30 seconds.
8. Two workspaces with due jobs both make progress under the existing cursor.

## Acceptance criteria

- An idle extractor writes no repeated completion logs.
- A local upload starts an extractor turn without the former 250 ms poll delay.
- A retry and a lost lease become claimable at their durable due time.
- A separate extractor process observes new work within 30 seconds.
- The full attachment and runtime test suites pass.
