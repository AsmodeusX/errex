//! Fingerprint derivation for grouping similar events.
//!
//! Strategy is deliberately naive: hash the exception type plus the innermost
//! in-app frame (function + filename), or the custom fingerprint the SDK sent.
//! Hashing uses FNV-1a rather than `DefaultHasher`, whose output may change
//! across Rust releases and would silently split persisted issues.

use errex_proto::{Event, Fingerprint};

const DEFAULT_TOKEN: &str = "{{ default }}";

pub fn derive(event: &Event) -> Fingerprint {
    let default = default_parts(event);
    let parts = match event.fingerprint.as_deref().filter(|f| !f.is_empty()) {
        Some(custom) => custom
            .iter()
            .flat_map(|part| {
                if part == DEFAULT_TOKEN {
                    default.clone()
                } else {
                    vec![part.clone()]
                }
            })
            .collect(),
        None => default,
    };
    fingerprint_from(&parts)
}

fn default_parts(event: &Event) -> Vec<String> {
    if let Some(ex) = event.primary_exception() {
        let frame = ex.stacktrace.as_ref().and_then(|s| {
            s.frames
                .iter()
                .rev()
                .find(|f| f.in_app == Some(true))
                .or_else(|| s.frames.last())
        });
        return vec![
            "exc".into(),
            ex.ty.clone().unwrap_or_default(),
            frame.and_then(|f| f.function.clone()).unwrap_or_default(),
            frame.and_then(|f| f.filename.clone()).unwrap_or_default(),
        ];
    }

    // Prefer the unformatted template so the same log line with different
    // parameters still groups together.
    let message = event
        .logentry
        .as_ref()
        .and_then(|l| l.message.as_deref())
        .or(event.message.as_deref())
        .or_else(|| event.logentry.as_ref().and_then(|l| l.formatted.as_deref()));

    match message {
        Some(msg) => vec!["msg".into(), msg.to_string()],
        // Nothing distinguishing — fall back to event id so each event is its
        // own group rather than collapsing all unknown events together.
        None => vec!["id".into(), event.event_id.to_string()],
    }
}

fn fingerprint_from(parts: &[String]) -> Fingerprint {
    Fingerprint::new(format!("{:016x}", stable_hash(parts)))
}

// Each part is length-prefixed so no choice of part contents can make two
// different part lists hash the same input.
fn stable_hash(parts: &[String]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let mut h = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    for part in parts {
        feed(&(part.len() as u64).to_le_bytes());
        feed(part.as_bytes());
    }
    h
}

#[cfg(test)]
mod tests {
    //! Fingerprint behavior tests. We pin the *contract* (what produces
    //! equal vs distinct fingerprints) rather than concrete hash values,
    //! because `DefaultHasher` is intentionally not stable across Rust
    //! versions — a SHA-256 / blake3 cutover later will change values
    //! without breaking any of these assertions.
    //!
    //! Stability across Rust versions is itself a TODO (see issue tracker):
    //! once the daemon stores issues persistently across upgrades, the
    //! fingerprint algorithm must be deterministic across compiler bumps
    //! or grouping would silently fragment.

    use super::*;
    use chrono::Utc;
    use errex_proto::{ExceptionContainer, ExceptionInfo, Frame, Stacktrace};
    use uuid::Uuid;

    fn ev_with_exception(ty: &str, function: &str, filename: &str, lineno: u32) -> Event {
        Event {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            platform: None,
            level: None,
            environment: None,
            release: None,
            server_name: None,
            message: None,
            exception: Some(ExceptionContainer {
                values: vec![ExceptionInfo {
                    ty: Some(ty.into()),
                    value: None,
                    module: None,
                    stacktrace: Some(Stacktrace {
                        frames: vec![Frame {
                            filename: Some(filename.into()),
                            function: Some(function.into()),
                            module: None,
                            lineno: Some(lineno),
                            colno: None,
                            in_app: Some(true),
                        }],
                    }),
                }],
            }),
            breadcrumbs: None,
            tags: None,
            contexts: None,
            extra: None,
            user: None,
            request: None,
            logentry: None,
            fingerprint: None,
        }
    }

    fn ev_message(msg: &str) -> Event {
        Event {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            platform: None,
            level: None,
            environment: None,
            release: None,
            server_name: None,
            message: Some(msg.into()),
            exception: None,
            breadcrumbs: None,
            tags: None,
            contexts: None,
            extra: None,
            user: None,
            request: None,
            logentry: None,
            fingerprint: None,
        }
    }

    fn ev_empty() -> Event {
        Event {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            platform: None,
            level: None,
            environment: None,
            release: None,
            server_name: None,
            message: None,
            exception: None,
            breadcrumbs: None,
            tags: None,
            contexts: None,
            extra: None,
            user: None,
            request: None,
            logentry: None,
            fingerprint: None,
        }
    }

    // ----- shape -----

    #[test]
    fn output_is_16_hex_chars() {
        let fp = derive(&ev_with_exception("E", "f", "a.js", 1));
        let s = fp.as_str();
        assert_eq!(s.len(), 16);
        assert!(
            s.chars().all(|c| c.is_ascii_hexdigit()),
            "non-hex char: {s}"
        );
    }

    #[test]
    fn deterministic_across_calls() {
        let a = derive(&ev_with_exception("E", "f", "a.js", 1));
        let b = derive(&ev_with_exception("E", "f", "a.js", 1));
        assert_eq!(a, b, "same inputs must hash identically");
    }

    // ----- grouping (same fingerprint) -----

    #[test]
    fn lineno_and_event_id_do_not_affect_grouping() {
        // Two events with identical type+function+filename but different
        // lineno and event_id must group together. Otherwise every event
        // would be its own issue and the daemon's value vanishes.
        let a = derive(&ev_with_exception(
            "TypeError",
            "checkout",
            "src/pay.ts",
            10,
        ));
        let b = derive(&ev_with_exception(
            "TypeError",
            "checkout",
            "src/pay.ts",
            273,
        ));
        assert_eq!(a, b);
    }

    // ----- distinction (different fingerprints) -----

    #[test]
    fn different_exception_types_produce_different_fingerprints() {
        let a = derive(&ev_with_exception("TypeError", "f", "a.js", 1));
        let b = derive(&ev_with_exception("ReferenceError", "f", "a.js", 1));
        assert_ne!(a, b);
    }

    #[test]
    fn different_functions_produce_different_fingerprints() {
        let a = derive(&ev_with_exception("E", "alpha", "a.js", 1));
        let b = derive(&ev_with_exception("E", "beta", "a.js", 1));
        assert_ne!(a, b);
    }

    #[test]
    fn different_filenames_produce_different_fingerprints() {
        let a = derive(&ev_with_exception("E", "f", "a.js", 1));
        let b = derive(&ev_with_exception("E", "f", "b.js", 1));
        assert_ne!(a, b);
    }

    // ----- fallbacks -----

    #[test]
    fn message_only_events_group_by_message() {
        let a = derive(&ev_message("Database connection lost"));
        let b = derive(&ev_message("Database connection lost"));
        let c = derive(&ev_message("Different message"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn empty_event_falls_back_to_event_id() {
        // No exception, no message: each event must be its own group rather
        // than collapsing all "unknown" events into a single noisy bucket.
        let a = derive(&ev_empty());
        let b = derive(&ev_empty());
        assert_ne!(a, b, "two empty events must produce distinct fingerprints");
    }

    #[test]
    fn exception_takes_precedence_over_message() {
        // If an event has BOTH a message and an exception, the exception
        // is the grouping key. (The message often differs per occurrence
        // even when the exception is the same.)
        let mut with_ex = ev_with_exception("E", "f", "a.js", 1);
        with_ex.message = Some("changing message 1".into());
        let mut with_ex2 = ev_with_exception("E", "f", "a.js", 1);
        with_ex2.message = Some("changing message 2".into());
        assert_eq!(derive(&with_ex), derive(&with_ex2));
    }

    #[test]
    fn custom_fingerprint_overrides_exception() {
        let mut a = ev_with_exception("E", "f", "a.js", 1);
        a.fingerprint = Some(vec!["checkout-timeout".into()]);
        let mut b = ev_with_exception("Other", "g", "b.js", 2);
        b.fingerprint = Some(vec!["checkout-timeout".into()]);
        assert_eq!(derive(&a), derive(&b));
    }

    #[test]
    fn logentry_message_groups_message_only_events() {
        let mut a = ev_empty();
        a.logentry = Some(errex_proto::LogEntry {
            message: Some("cache miss".into()),
            formatted: None,
        });
        let mut b = ev_empty();
        b.logentry = Some(errex_proto::LogEntry {
            message: Some("cache miss".into()),
            formatted: None,
        });
        assert_eq!(derive(&a), derive(&b));
    }

    #[test]
    fn prefers_innermost_in_app_frame() {
        let mut a = ev_with_exception("E", "vendor_fn", "vendor.js", 1);
        let frames = &mut a.exception.as_mut().unwrap().values[0]
            .stacktrace
            .as_mut()
            .unwrap()
            .frames;
        frames.push(Frame {
            filename: Some("app.js".into()),
            function: Some("handler".into()),
            module: None,
            lineno: Some(9),
            colno: None,
            in_app: Some(true),
        });
        frames.push(Frame {
            filename: Some("lib.js".into()),
            function: Some("deep".into()),
            module: None,
            lineno: Some(3),
            colno: None,
            in_app: Some(false),
        });
        let b = ev_with_exception("E", "handler", "app.js", 9);
        assert_eq!(derive(&a), derive(&b));
    }

    #[test]
    fn default_token_expands_to_default_grouping() {
        let mut a = ev_with_exception("E", "f", "a.js", 1);
        a.fingerprint = Some(vec!["{{ default }}".into(), "tenant-1".into()]);
        let mut b = ev_with_exception("Other", "g", "b.js", 2);
        b.fingerprint = Some(vec!["{{ default }}".into(), "tenant-1".into()]);
        assert_ne!(derive(&a), derive(&b));

        let plain = ev_with_exception("E", "f", "a.js", 1);
        let mut expanded = ev_with_exception("E", "f", "a.js", 1);
        expanded.fingerprint = Some(vec!["{{ default }}".into()]);
        assert_eq!(derive(&plain), derive(&expanded));
    }

    #[test]
    fn separator_characters_cannot_collide_parts() {
        let mut a = ev_empty();
        a.fingerprint = Some(vec!["a:b".into(), "c".into()]);
        let mut b = ev_empty();
        b.fingerprint = Some(vec!["a".into(), "b:c".into()]);
        assert_ne!(derive(&a), derive(&b));
    }

    #[test]
    fn formatted_log_params_group_by_template() {
        let mut a = ev_empty();
        a.logentry = Some(errex_proto::LogEntry {
            message: Some("user %s logged in".into()),
            formatted: Some("user alice logged in".into()),
        });
        let mut b = ev_empty();
        b.logentry = Some(errex_proto::LogEntry {
            message: Some("user %s logged in".into()),
            formatted: Some("user bob logged in".into()),
        });
        assert_eq!(derive(&a), derive(&b));
    }
}
