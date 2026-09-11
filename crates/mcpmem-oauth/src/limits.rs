//! Per-peer request limits for the endpoints no credential guards.
//!
//! Five OAuth endpoints answer an anonymous caller, because the
//! specifications say they must: a client that has just discovered this server
//! holds nothing to authenticate a registration, an authorization request or a
//! token exchange with. Each of those requests costs this server a row or a
//! database round trip, so the number of them one peer may send in a minute is
//! bounded here.
//!
//! This bounds **how many** requests arrive. It does not bound how large one
//! request is: `crate::registration::register` caps the fields of a client
//! row, and `src/oauth_routes.rs` caps the scope lists. The two are different
//! defences and neither substitutes for the other.
//!
//! # What this is not
//!
//! It is one process's own counter, held in memory. It is not a distributed
//! quota: two `mcpmem` processes behind one proxy count separately, and a
//! restart forgets every window. It also does nothing against a caller that
//! commands many source addresses — a per-peer limit cannot, by construction.
//! An operator who needs either wants a limit at the proxy, and
//! `docs/runbooks/oauth-deployment.md` says so.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// How long one counting window lasts, in microseconds.
pub const WINDOW_US: i64 = 60 * 1_000_000;

/// How many registrations one peer may send in a window.
///
/// Lower than the rest because a registration is the one anonymous request
/// that writes a row nothing expires: `oauth_client` has no `expires_us`
/// column, and a registered client leaves only by the eviction in
/// [`crate::store::Store::evict_stale_clients`].
pub const REGISTER_PER_WINDOW: u32 = 20;

/// How many requests one peer may send in a window to each of the endpoints
/// that carry a login or a credential.
///
/// A browser walking one consent page sends a handful, and a connector
/// refreshing a token sends one an hour, so this is far above any legitimate
/// caller and still bounds a loop.
pub const LOGIN_PER_WINDOW: u32 = 60;

/// The most peers one limiter counts inside one window.
///
/// A bound on memory, and the reason [`RateLimiter`] counts against a shared
/// window rather than a per-key one: the whole map is dropped at the window
/// boundary, so nothing has to be scanned or evicted to keep this true. At
/// this size the five limiters together hold tens of thousands of short keys,
/// which is under a megabyte, and the map is emptied every minute.
pub const MAX_KEYS: usize = 8192;

/// The window every key of one limiter is counted against.
struct Window {
    /// When this window opened, in microseconds. Zero before the first
    /// request, which is in the past for every real clock and so opens a
    /// window on the first call.
    started_us: i64,
    /// How many requests each peer has sent inside this window. A peer absent
    /// from the map has sent none.
    counts: HashMap<Box<str>, u32>,
}

/// A fixed-window request counter, keyed by peer address.
///
/// The window is shared by every key rather than started per key, and that is
/// the whole reason this type has no pruning, no eviction and no background
/// work: when the window ends, one [`HashMap::clear`] retires every counter at
/// once, in constant amortized time and without releasing the map's capacity.
/// A per-key window would need an expiry scan, and an expiry scan on a map an
/// anonymous caller fills is itself a cost that caller chooses.
///
/// The price is that a peer's allowance resets on the shared boundary instead
/// of one window after its own first request, so a peer that starts late in a
/// window gets its next allowance early. That is what a fixed window means,
/// and the alternative — a sliding window — costs a timestamp queue per key.
pub struct RateLimiter {
    limit: u32,
    window_us: i64,
    max_keys: usize,
    window: Mutex<Window>,
}

impl RateLimiter {
    /// A limiter admitting `limit` requests per peer in each [`WINDOW_US`],
    /// counting at most [`MAX_KEYS`] peers at a time.
    pub fn per_window(limit: u32) -> RateLimiter {
        RateLimiter::new(limit, WINDOW_US, MAX_KEYS)
    }

    /// A limiter with every bound named. Private: the server wants one window
    /// length and one key bound everywhere, and a second pair of values here
    /// would be a second set of numbers to reason about with nothing choosing
    /// between them.
    fn new(limit: u32, window_us: i64, max_keys: usize) -> RateLimiter {
        RateLimiter {
            limit,
            window_us,
            max_keys,
            window: Mutex::new(Window {
                started_us: 0,
                counts: HashMap::new(),
            }),
        }
    }

    /// How long a refused caller must wait before the window is certain to
    /// have rolled, in seconds — the value a 429 carries in `Retry-After`.
    ///
    /// It is the whole window rather than the time left in the current one, and
    /// deliberately: RFC 9110 section 10.2.3 makes `Retry-After` a minimum
    /// delay, so an upper bound is the honest answer and a caller that waits it
    /// out is always admitted. Reading the remaining time would need a second
    /// lock acquisition whose answer can disagree with the decision that was
    /// just taken.
    pub const fn retry_after_seconds(&self) -> i64 {
        self.window_us / 1_000_000
    }

    /// Count one request from `key` and say whether it may proceed.
    ///
    /// `true` admits the request and consumes one of the peer's allowance.
    /// `false` refuses it and consumes nothing, so a refused caller cannot
    /// push its own window further out by continuing to send.
    ///
    /// A key arriving when [`MAX_KEYS`] peers are already counted
    /// is **admitted and not counted**. That is fail-open, and it is the right
    /// way round: the map is full only when that many distinct addresses have
    /// sent inside one window, which is a caller this limiter cannot bound
    /// anyway, and failing closed would let such a caller lock every
    /// legitimate peer out of the endpoint. The state ends by itself at the
    /// next window boundary.
    pub fn check(&self, key: &str, now_us: i64) -> bool {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if now_us.saturating_sub(window.started_us) >= self.window_us {
            window.counts.clear();
            window.started_us = now_us;
        }
        if let Some(count) = window.counts.get_mut(key) {
            if *count >= self.limit {
                return false;
            }
            *count += 1;
            return true;
        }
        if window.counts.len() < self.max_keys {
            window.counts.insert(Box::from(key), 1);
        }
        true
    }
}

/// The limiter behind each anonymous endpoint.
///
/// One per endpoint rather than one shared counter, because the budgets are
/// not interchangeable: a client walks `authorize`, `consent` and `token` once
/// each in a single login, and a shared counter would let a loop against one
/// of them refuse the other two. `token` and `revoke` do share a limiter —
/// both are one caller presenting one credential, and neither is reached in a
/// browser.
pub struct Limits {
    /// `POST /oauth/register`, which writes a client row.
    pub register: RateLimiter,
    /// `GET /oauth/authorize`, which writes a ten-minute login row.
    pub authorize: RateLimiter,
    /// `POST /oauth/consent`. Bounded because a form carrying the wrong token
    /// is refused *and* leaves the login row in place, so a guess costs the
    /// guesser nothing and may be retried.
    pub consent: RateLimiter,
    /// `POST /oauth/token` and `POST /oauth/revoke`, which take a credential
    /// from whoever sends it.
    pub credential: RateLimiter,
}

impl Limits {
    /// The defaults this server runs with.
    pub fn new() -> Limits {
        Limits {
            register: RateLimiter::per_window(REGISTER_PER_WINDOW),
            authorize: RateLimiter::per_window(LOGIN_PER_WINDOW),
            consent: RateLimiter::per_window(LOGIN_PER_WINDOW),
            credential: RateLimiter::per_window(LOGIN_PER_WINDOW),
        }
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::new()
    }
}
