//! Per-authority rate-limit circuit breaker.
//!
//! One breaker per provider instance, shared by every HTTP send path in this
//! crate (raw `cx.http()`, typed endpoints, and blob fetches). A 429 with a
//! `Retry-After` arms a window for the request URL's authority until the
//! instant that the upstream states, and a 429 without one arms none: the
//! host asks no time of its own. While the
//! window is open, sends to that authority fail in-guest with a
//! rate-limited error carrying the remaining cooldown, without issuing a
//! callout. An answer is of the authority of the URL that answered it, which
//! is another than the one of the request after a redirect that the host
//! followed; the first answer of an upstream with status < 400 disarms its
//! authority, an answer that the host gave from its projection disarms none,
//! and the whole breaker is cleared on provider shutdown so a window never
//! leaks across instance teardown.
//!
//! Providers do not drive this module directly; the send paths do. The one
//! provider-facing hook is [`note_rate_limited`] (re-exported at the crate
//! root) for upstream-specific throttle signals the generic 429 path cannot
//! see, such as GitHub's `403` with `x-ratelimit-remaining: 0`.

use core::time::Duration;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;
use std::time::Instant;

use hashbrown::HashMap;

/// The instant that a window closes, none for an instant past the clock of
/// the guest, which closes it only at a clear.
type BreakerMap = HashMap<String, Option<Instant>, BuildHasherDefault<DefaultHasher>>;

/// Map of authority to the instant its 429 window closes. Single-threaded
/// (`RefCell`) because provider guests run one operation at a time; access
/// goes through the thread-local in [`with_breaker`].
pub struct RateLimitBreaker {
    open: RefCell<BreakerMap>,
}

impl RateLimitBreaker {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: RefCell::new(HashMap::with_hasher(BuildHasherDefault::new())),
        }
    }

    /// Remaining cooldown if the authority is currently throttled, else None.
    /// Lazily evicts an expired entry so the map cannot grow without bound.
    pub fn check(&self, authority: &str) -> Option<Duration> {
        let now = Instant::now();
        let open = self.open.borrow();
        let open_until = open.get(authority).copied()?;
        let Some(open_until) = open_until else {
            return Some(Duration::MAX);
        };
        if open_until > now {
            return Some(open_until.duration_since(now));
        }
        drop(open);
        self.open.borrow_mut().remove(authority);
        None
    }

    /// Arm after a 429 with the `retry_after` that its upstream states, to
    /// the instant that it names; a 429 without one arms nothing.
    pub fn record_429(&self, authority: &str, retry_after: Option<Duration>) {
        let Some(retry_after) = retry_after else {
            return;
        };
        self.open.borrow_mut().insert(
            authority.to_string(),
            Instant::now().checked_add(retry_after),
        );
    }

    /// Close after a successful (status < 400) response.
    pub fn record_success(&self, authority: &str) {
        self.open.borrow_mut().remove(authority);
    }

    pub fn clear(&self) {
        self.open.borrow_mut().clear();
    }
}

impl Default for RateLimitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static BREAKER: RateLimitBreaker = const { RateLimitBreaker::new() };
}

/// Run `f` with the per-instance breaker. The breaker is SDK-owned (not a
/// macro-emitted per-provider thread-local) because the HTTP/endpoint send
/// path in this crate must reach it.
pub fn with_breaker<R>(f: impl FnOnce(&RateLimitBreaker) -> R) -> R {
    BREAKER.with(f)
}

/// Reset the breaker. Called from the provider macro's `shutdown()` so a 429
/// window does not leak across instance teardown.
pub fn clear_breaker() {
    BREAKER.with(RateLimitBreaker::clear);
}

/// Endpoint authority (`scheme://host[:port]`) used as the breaker key.
/// Returns None for an unparseable URL (caller then skips breaker logic).
pub fn authority_of(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?;
    let mut authority = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        authority.push(':');
        authority.push_str(&port.to_string());
    }
    Some(authority)
}

/// The time of the `Retry-After` value `value` of an answer: its seconds,
/// or the time from the `Date` of the answer, `date`, to its HTTP date, and
/// from now for an answer without a `Date`. None for a value that is
/// neither, and no time for a date that passed.
pub fn retry_after(value: &str, date: Option<&str>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let until = http_date(value)?;
    let from = match date.and_then(http_date) {
        Some(date) => date,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    };
    Some(Duration::from_secs(until.saturating_sub(from)))
}

/// The seconds since the Unix epoch of an HTTP date in each of its three
/// forms (RFC 9110, 5.6.7): `Sun, 06 Nov 1994 08:49:37 GMT`,
/// `Sunday, 06-Nov-94 08:49:37 GMT`, and `Sun Nov  6 08:49:37 1994`.
fn http_date(value: &str) -> Option<u64> {
    let fields: Vec<&str> = value
        .split([' ', ',', '-'])
        .filter(|field| !field.is_empty())
        .collect();
    let ([_, day, month, year, time, "GMT"] | [_, month, day, time, year]) = fields.as_slice()
    else {
        return None;
    };
    let day: u64 = day.parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| name == month)? as u64
        + 1;
    let year = match (year.len(), year.parse::<u64>().ok()?) {
        // A year of two digits is of the fifty years to come or of the
        // fifty before (RFC 9110, 5.6.7).
        (2, year) if year >= 70 => 1900 + year,
        (2, year) => 2000 + year,
        (4, year) if year >= 1970 => year,
        _ => return None,
    };
    let mut clock = time.split(':').map(|part| part.parse::<u64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if clock.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    // The days from the epoch to a civil date, with the year from March.
    let (shifted_year, shifted_month) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = shifted_year / 400;
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_breaker_reports_remaining() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://a.test";

        with_breaker(|b| b.record_429(auth, Some(Duration::from_secs(3))));
        let remaining = with_breaker(|b| b.check(auth)).expect("breaker should be open");

        assert!(remaining <= Duration::from_secs(3));
        assert!(remaining > Duration::from_secs(2));
    }

    #[test]
    fn success_closes_the_breaker() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://success-closes.test";

        with_breaker(|b| {
            b.record_429(auth, Some(Duration::from_secs(3)));
            b.record_success(auth);
        });

        assert_eq!(with_breaker(|b| b.check(auth)), None);
    }

    #[test]
    fn missing_retry_after_arms_no_window() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://missing-retry-after.test";

        with_breaker(|b| b.record_429(auth, None));
        assert_eq!(with_breaker(|b| b.check(auth)), None);
    }

    #[test]
    fn huge_retry_after_holds_the_window_without_panicking() {
        with_breaker(RateLimitBreaker::clear);
        let auth = "https://huge-retry.test";

        // `u64::MAX` seconds is past the clock: the window stands until a
        // clear, without an overflow of `Instant`.
        with_breaker(|b| b.record_429(auth, Some(Duration::from_secs(u64::MAX))));
        let remaining = with_breaker(|b| b.check(auth)).expect("breaker should be open");

        assert_eq!(remaining, Duration::MAX);
        with_breaker(RateLimitBreaker::clear);
    }

    #[test]
    fn authority_keys_isolate_hosts() {
        with_breaker(RateLimitBreaker::clear);
        let auth_a = "https://a-isolated.test";
        let auth_b = "https://b-isolated.test";

        with_breaker(|b| b.record_429(auth_a, Some(Duration::from_secs(3))));

        assert_eq!(with_breaker(|b| b.check(auth_b)), None);
    }

    #[test]
    fn authority_of_includes_explicit_port_only() {
        assert_eq!(
            authority_of("https://x.test/p?q=1"),
            Some("https://x.test".into())
        );
        assert_eq!(
            authority_of("http://x.test:8080/p"),
            Some("http://x.test:8080".into())
        );
    }

    #[test]
    fn a_retry_after_is_seconds_or_an_http_date_from_the_date_of_its_answer() {
        assert_eq!(retry_after(" 120 ", None), Some(Duration::from_secs(120)));
        let date = Some("Sun, 06 Nov 1994 08:49:37 GMT");
        for until in [
            "Sun, 06 Nov 1994 08:50:37 GMT",
            "Sunday, 06-Nov-94 08:50:37 GMT",
            "Sun Nov  6 08:50:37 1994",
        ] {
            assert_eq!(
                retry_after(until, date),
                Some(Duration::from_secs(60)),
                "{until}"
            );
        }
        // A date that passed is no time to wait, and one of an answer
        // without a `Date` counts from now.
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:00 GMT", date),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:00 GMT", None),
            Some(Duration::ZERO)
        );
        assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            http_date("Tue, 29 Feb 2000 12:00:00 GMT"),
            Some(951_825_600)
        );
        assert_eq!(
            http_date("Sun, 22 Nov 2026 00:00:00 GMT"),
            Some(1_795_305_600)
        );
        for other in [
            "soon",
            "",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
        ] {
            assert_eq!(retry_after(other, date), None, "{other}");
        }
    }
}
