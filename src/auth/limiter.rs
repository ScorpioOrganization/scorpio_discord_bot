use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failed attempts allowed per client IP inside one window.
const IP_LIMIT: u32 = 5;
/// Failed attempts allowed per username inside one window. Higher than the IP limit so a
/// single attacker cannot trivially lock a real user out of their account.
const USER_LIMIT: u32 = 20;
const WINDOW: Duration = Duration::from_secs(15 * 60);
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// In-memory brute force protection for the login form.
#[derive(Default)]
pub struct LoginLimiter {
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

impl LoginLimiter {
    /// Returns the remaining lockout time when either the IP or the username is locked.
    pub fn check(&self, ip: &str, username: &str) -> Option<Duration> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        keys(ip, username)
            .iter()
            .filter_map(|(key, _)| entries.get(key)?.locked_until)
            .filter(|until| *until > now)
            .map(|until| until - now)
            .max()
    }

    pub fn record_failure(&self, ip: &str, username: &str) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        for (key, limit) in keys(ip, username) {
            let entry = entries.entry(key).or_insert(Entry {
                failures: 0,
                window_start: now,
                locked_until: None,
            });
            if now.duration_since(entry.window_start) > WINDOW {
                entry.failures = 0;
                entry.window_start = now;
                entry.locked_until = None;
            }
            entry.failures += 1;
            if entry.failures >= limit {
                entry.locked_until = Some(now + LOCKOUT);
            }
        }
    }

    pub fn record_success(&self, ip: &str, username: &str) {
        let mut entries = self.entries.lock().unwrap();
        for (key, _) in keys(ip, username) {
            entries.remove(&key);
        }
    }

    /// Drops entries that can no longer affect anything.
    pub fn prune(&self) {
        let now = Instant::now();
        self.entries.lock().unwrap().retain(|_, e| {
            e.locked_until.is_some_and(|u| u > now) || now.duration_since(e.window_start) <= WINDOW
        });
    }
}

fn keys(ip: &str, username: &str) -> [(String, u32); 2] {
    [
        (format!("ip:{ip}"), IP_LIMIT),
        (format!("user:{}", username.to_lowercase()), USER_LIMIT),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_ip_after_limit() {
        let l = LoginLimiter::default();
        for _ in 0..IP_LIMIT - 1 {
            l.record_failure("1.2.3.4", "root");
            assert!(l.check("1.2.3.4", "root").is_none());
        }
        l.record_failure("1.2.3.4", "root");
        assert!(l.check("1.2.3.4", "root").is_some());
        // Different IP, same user is still below the user limit.
        assert!(l.check("5.6.7.8", "root").is_none());
    }

    #[test]
    fn locks_username_across_ips() {
        let l = LoginLimiter::default();
        for i in 0..USER_LIMIT {
            l.record_failure(&format!("10.0.0.{i}"), "Root");
        }
        assert!(l.check("192.168.1.1", "root").is_some());
    }

    #[test]
    fn success_clears() {
        let l = LoginLimiter::default();
        for _ in 0..IP_LIMIT {
            l.record_failure("1.2.3.4", "root");
        }
        l.record_success("1.2.3.4", "root");
        assert!(l.check("1.2.3.4", "root").is_none());
    }
}
