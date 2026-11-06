//! Server-side scrubbing of ingested events.
//!
//! The app scrubs before it sends; this is the second line, applied in
//! `digest::prepare` before anything is stored, logged or alerted on. Two
//! rules, both mirrored from the product's other components so a value
//! refused there is not kept here:
//!
//! * Keys the licensing server refuses as non-metadata (`licensing/privacy.py`
//!   `FORBIDDEN`) are dropped wherever they appear, case-insensitively, plus
//!   frame `vars` (local variables). `user` and `request` are dropped whole:
//!   they carry IP addresses, e-mail addresses, cookies and URLs.
//! * The client scrubber's patterns are replaced in messages, exception
//!   values, breadcrumbs, tags and extra: a VIN `[A-HJ-NPR-Z0-9]{17}` becomes
//!   `<vin>`, `0x` plus 4 or more hex digits and bare hex runs of 8 or more
//!   become `<hex>`, an e-mail address `<email>`, and a Bearer token, a JWT
//!   or a long base64 run `<secret>`.
//!
//! Hand-rolled scanners instead of the `regex` crate: errexd keeps its binary
//! free of regex-automata (see `config.rs` on log directives), and every
//! pattern here is a run of ASCII classes.
//!
//! `extra.dart_stack` is a machine trace whose build id and offsets are hex
//! by nature; it keeps the VIN, e-mail and token rules but not the hex and
//! base64 ones, or offline symbolication would have nothing to read. The
//! same holds for `extra.build_id`, the id that picks the symbols file, and
//! for the `dart_stack` of each entry of `extra.causes` (the outer-to-inner
//! chain of wrapped errors); the rest of a cause keeps every rule.
//!
//! The Zeus app's `ecu_reset` event is the same kind of record: the ECU's reset
//! record decoded and raw (`extra.reset_record`, `reset_record_raw`,
//! `reset_details_raw`) and four tags that are one hex number each (the
//! cause code, the loader's reset type, the cause detail and the code
//! address in the ECU software, `reset.*`). The app checks those values by
//! shape before it sends them.

use errex_proto::Event;
use serde_json::{Map, Value};

/// `licensing/privacy.py` FORBIDDEN, plus `vars`.
const FORBIDDEN: &[&str] = &[
    "file",
    "files",
    "filename",
    "attachment",
    "attachments",
    "zfl",
    "zfle",
    "diff",
    "diffs",
    "calibration",
    "calibration_bytes",
    "table_values",
    "axis_values",
    "screenshot",
    "crash_dump",
    "private_key",
    "content_key",
    "ciphertext",
    "plaintext",
    "project_history",
    "manifest",
    "intent",
    "commitment_nonce",
    "vars",
];

/// `extra` keys holding a machine trace: see the module comment.
const TRACE_KEYS: &[&str] = &[
    "dart_stack",
    "build_id",
    "reset_record",
    "reset_record_raw",
    "reset_details_raw",
];

/// Tags whose value is one machine hex number: see the module comment.
const TRACE_TAGS: &[&str] = &["reset.cause_code", "reset.tag", "reset.info", "reset.where"];

fn tag_mode(key: &str) -> Mode {
    if TRACE_TAGS.contains(&key) {
        Mode::Trace
    } else {
        Mode::Full
    }
}

/// `extra` key holding a list of causes, each with its own trace.
const CAUSES_KEY: &str = "causes";

fn is_forbidden(key: &str) -> bool {
    FORBIDDEN.iter().any(|f| f.eq_ignore_ascii_case(key))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Full,
    Trace,
}

/// Apply every text rule to `s`.
pub fn scrub_text(s: &str) -> String {
    scrub_with(s, Mode::Full)
}

fn scrub_with(s: &str, mode: Mode) -> String {
    let s = bearer(s);
    let s = jwt(&s);
    let s = emails(&s);
    let s = if mode == Mode::Full {
        hex(&base64(&s))
    } else {
        s
    };
    vins(&s)
}

/// Scrub an event in place. The `fingerprint` list is left alone so the
/// caller can hash it as sent; scrub it afterwards with [`scrub_strings`].
pub fn scrub_event(ev: &mut Event) {
    ev.user = None;
    ev.request = None;

    if let Some(m) = ev.message.as_mut() {
        *m = scrub_text(m);
    }
    if let Some(le) = ev.logentry.as_mut() {
        for s in [le.message.as_mut(), le.formatted.as_mut()]
            .into_iter()
            .flatten()
        {
            *s = scrub_text(s);
        }
        scrub_map(&mut le.other, Mode::Full);
    }
    if let Some(container) = ev.exception.as_mut() {
        for ex in &mut container.values {
            if let Some(v) = ex.value.as_mut() {
                *v = scrub_text(v);
            }
            drop_forbidden_map(&mut ex.other);
            if let Some(st) = ex.stacktrace.as_mut() {
                drop_forbidden_map(&mut st.other);
                for frame in &mut st.frames {
                    drop_forbidden_map(&mut frame.other);
                }
            }
        }
    }
    if let Some(v) = ev.breadcrumbs.as_mut() {
        scrub_value(v, Mode::Full);
    }
    if let Some(v) = ev.tags.as_mut() {
        scrub_tags(v);
    }
    if let Some(Value::Object(extra)) = ev.extra.as_mut() {
        extra.retain(|k, _| !is_forbidden(k));
        for (k, v) in extra.iter_mut() {
            if TRACE_KEYS.contains(&k.as_str()) {
                scrub_value(v, Mode::Trace);
            } else if k == CAUSES_KEY {
                scrub_causes(v);
            } else {
                scrub_value(v, Mode::Full);
            }
        }
    } else if let Some(v) = ev.extra.as_mut() {
        scrub_value(v, Mode::Full);
    }
    if let Some(v) = ev.contexts.as_mut() {
        drop_forbidden(v);
    }
    drop_forbidden_map(&mut ev.other);
}

pub fn scrub_strings(list: &mut [String]) {
    for s in list {
        *s = scrub_text(s);
    }
}

fn scrub_tags(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.retain(|k, _| !is_forbidden(k));
            for (k, value) in map.iter_mut() {
                scrub_value(value, tag_mode(k));
            }
        }
        Value::Array(pairs) => {
            pairs.retain(
                |pair| !matches!(pair.get(0).and_then(Value::as_str), Some(k) if is_forbidden(k)),
            );
            for pair in pairs.iter_mut() {
                match pair {
                    Value::Array(kv) if kv.len() >= 2 => {
                        let mode = kv[0].as_str().map_or(Mode::Full, tag_mode);
                        for value in kv.iter_mut().skip(1) {
                            scrub_value(value, mode);
                        }
                    }
                    other => scrub_value(other, Mode::Full),
                }
            }
        }
        other => scrub_value(other, Mode::Full),
    }
}

/// `extra.causes`: each entry's `dart_stack` is a trace, everything else
/// gets every rule. Anything but a list of objects is not a cause chain and
/// is scrubbed like any other value.
fn scrub_causes(v: &mut Value) {
    let Value::Array(entries) = v else {
        return scrub_value(v, Mode::Full);
    };
    for entry in entries {
        match entry {
            Value::Object(fields) => {
                fields.retain(|k, _| !is_forbidden(k));
                for (k, field) in fields.iter_mut() {
                    let mode = if k == "dart_stack" {
                        Mode::Trace
                    } else {
                        Mode::Full
                    };
                    scrub_value(field, mode);
                }
            }
            other => scrub_value(other, Mode::Full),
        }
    }
}

fn scrub_map(map: &mut Map<String, Value>, mode: Mode) {
    map.retain(|k, _| !is_forbidden(k));
    for v in map.values_mut() {
        scrub_value(v, mode);
    }
}

fn scrub_value(v: &mut Value, mode: Mode) {
    match v {
        Value::String(s) => *s = scrub_with(s, mode),
        Value::Array(items) => items.iter_mut().for_each(|i| scrub_value(i, mode)),
        Value::Object(map) => scrub_map(map, mode),
        _ => {}
    }
}

fn drop_forbidden_map(map: &mut Map<String, Value>) {
    map.retain(|k, _| !is_forbidden(k));
    map.values_mut().for_each(drop_forbidden);
}

fn drop_forbidden(v: &mut Value) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(drop_forbidden),
        Value::Object(map) => drop_forbidden_map(map),
        _ => {}
    }
}

// ----- text scanners -----
//
// Every pattern is a run of ASCII bytes, so run boundaries are always char
// boundaries and slicing the `&str` there is sound for any UTF-8 input.

/// Rebuild `s`, offering each maximal run of `class` bytes to `replace`.
fn map_runs(
    s: &str,
    class: impl Fn(u8) -> bool,
    replace: impl Fn(&str) -> Option<String>,
) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if !class(bytes[i]) {
            let start = i;
            while i < bytes.len() && !class(bytes[i]) {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        let start = i;
        while i < bytes.len() && class(bytes[i]) {
            i += 1;
        }
        let run = &s[start..i];
        match replace(run) {
            Some(r) => out.push_str(&r),
            None => out.push_str(run),
        }
    }
    out
}

fn bearer(s: &str) -> String {
    const WORD: &[u8] = b"bearer";
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i + WORD.len() <= bytes.len() {
        let at_word = bytes[i..i + WORD.len()].eq_ignore_ascii_case(WORD)
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric());
        if !at_word {
            i += 1;
            continue;
        }
        let mut j = i + WORD.len();
        let gap = j;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        let token_start = j;
        while j < bytes.len() && !bytes[j].is_ascii_whitespace() && !b"\"',;".contains(&bytes[j]) {
            j += 1;
        }
        if gap == token_start || token_start == j {
            i += 1;
            continue;
        }
        out.push_str(&s[copied..token_start]);
        out.push_str("<secret>");
        copied = j;
        i = j;
    }
    out.push_str(&s[copied..]);
    out
}

fn jwt(s: &str) -> String {
    let class = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.';
    map_runs(s, class, |run| {
        let at = run.find("eyJ")?;
        let tail = &run[at..];
        let mut parts = tail.split('.');
        let header = parts.next()?;
        let payload = parts.next()?;
        parts.next()?;
        if header.len() < 4 || payload.is_empty() {
            return None;
        }
        Some(format!("{}<secret>", &run[..at]))
    })
}

fn emails(s: &str) -> String {
    let class = |b: u8| b.is_ascii_alphanumeric() || b"._%+-@".contains(&b);
    map_runs(s, class, |run| {
        let trimmed = run.trim_end_matches('.');
        let (local, domain) = trimmed.split_once('@')?;
        if local.is_empty() || domain.contains('@') {
            return None;
        }
        let labels: Vec<&str> = domain.split('.').collect();
        let tld = labels.last()?;
        let valid = labels.len() >= 2
            && labels.iter().all(|l| !l.is_empty())
            && tld.len() >= 2
            && tld.bytes().all(|b| b.is_ascii_alphabetic());
        valid.then(|| format!("<email>{}", &run[trimmed.len()..]))
    })
}

/// A run of 24 or more base64 characters that looks random: at least two
/// digits and both letter cases, or `+`/`/`. Long identifiers (no digits,
/// or one) and snake_case names stay readable.
fn base64(s: &str) -> String {
    let class = |b: u8| b.is_ascii_alphanumeric() || b"+/_-".contains(&b);
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if !class(bytes[i]) {
            let start = i;
            while i < bytes.len() && !class(bytes[i]) {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        let start = i;
        while i < bytes.len() && class(bytes[i]) {
            i += 1;
        }
        let run = &bytes[start..i];
        let digits = run.iter().filter(|b| b.is_ascii_digit()).count();
        let upper = run.iter().any(u8::is_ascii_uppercase);
        let lower = run.iter().any(u8::is_ascii_lowercase);
        let symbol = run.iter().any(|b| *b == b'+' || *b == b'/');
        let secret = run.len() >= 24 && digits >= 2 && (upper && lower || symbol);
        if secret {
            let mut end = i;
            while end < bytes.len() && end - i < 2 && bytes[end] == b'=' {
                end += 1;
            }
            out.push_str("<secret>");
            i = end;
        } else {
            out.push_str(&s[start..i]);
        }
    }
    out
}

fn hex(s: &str) -> String {
    map_runs(
        s,
        |b| b.is_ascii_alphanumeric(),
        |run| {
            let b = run.as_bytes();
            if b.len() >= 6 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
                let digits = b[2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
                if digits >= 4 {
                    return Some(format!("<hex>{}", &run[2 + digits..]));
                }
            }
            (b.len() >= 8 && b.iter().all(u8::is_ascii_hexdigit)).then(|| "<hex>".to_string())
        },
    )
}

fn vins(s: &str) -> String {
    let class = |b: u8| (b.is_ascii_uppercase() && !b"IOQ".contains(&b)) || b.is_ascii_digit();
    map_runs(s, class, |run| {
        (run.len() >= 17).then(|| "<vin>".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: Value) -> Event {
        serde_json::from_value(v).expect("valid event")
    }

    // ----- text rules -----

    #[test]
    fn vin_is_replaced() {
        assert_eq!(
            scrub_text("car WBS8M9C50J5K12345 bricked"),
            "car <vin> bricked"
        );
        // Inside a longer run of lowercase text the VIN-class run still matches.
        assert_eq!(scrub_text("vin=WBS8M9C50J5K12345;"), "vin=<vin>;");
    }

    #[test]
    fn vin_rule_is_case_sensitive_like_the_client() {
        assert_eq!(
            scrub_text("wbs8m9c50j5k12345 stays"),
            "wbs8m9c50j5k12345 stays"
        );
    }

    #[test]
    fn sixteen_vin_class_chars_are_not_a_vin() {
        assert_eq!(scrub_text("ABCDEFGH1234567Z"), "ABCDEFGH1234567Z");
    }

    #[test]
    fn prefixed_hex_of_four_or_more_digits_is_replaced() {
        assert_eq!(
            scrub_text("read at 0x80040000 failed"),
            "read at <hex> failed"
        );
        assert_eq!(scrub_text("addr 0X1A2B"), "addr <hex>");
        assert_eq!(scrub_text("service 0x31 nrc 0x7F"), "service 0x31 nrc 0x7F");
    }

    #[test]
    fn bare_hex_runs_of_eight_or_more_are_replaced() {
        assert_eq!(scrub_text("reply deadbeefcafe"), "reply <hex>");
        assert_eq!(scrub_text("crc 1234abcd ok"), "crc <hex> ok");
        assert_eq!(scrub_text("short beef"), "short beef");
        assert_eq!(scrub_text("accepted"), "accepted");
    }

    #[test]
    fn email_is_replaced() {
        assert_eq!(
            scrub_text("contact john.doe+x@example.co.uk."),
            "contact <email>."
        );
        assert_eq!(scrub_text("user@localhost"), "user@localhost");
    }

    #[test]
    fn bearer_token_is_replaced() {
        assert_eq!(
            scrub_text("Authorization: Bearer abc.def-ghi"),
            "Authorization: Bearer <secret>"
        );
        assert_eq!(scrub_text("bearer   tok123"), "bearer   <secret>");
    }

    #[test]
    fn jwt_is_replaced() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl";
        assert_eq!(
            scrub_text(&format!("token {jwt} end")),
            "token <secret> end"
        );
    }

    #[test]
    fn long_base64_run_is_replaced() {
        assert_eq!(
            scrub_text("key=Zm9vYmFyQmF6UXV4MTIzNDU2Nzg5MA== used"),
            "key=<secret> used"
        );
    }

    #[test]
    fn long_identifiers_are_not_secrets() {
        let s = "FlutterErrorDetailsForPointerEventDispatcher threw";
        assert_eq!(scrub_text(s), s);
        let snake = "flash_stage_transfer_data_block_2";
        assert_eq!(scrub_text(snake), snake);
    }

    #[test]
    fn non_ascii_text_survives() {
        assert_eq!(
            scrub_text("Fehler: Größe 0x1234 überschritten"),
            "Fehler: Größe <hex> überschritten"
        );
    }

    // ----- event rules -----

    #[test]
    fn user_and_request_are_dropped() {
        let mut e = ev(json!({
            "user": { "ip_address": "203.0.113.9", "email": "a@b.com" },
            "request": { "url": "https://x/y", "env": { "REMOTE_ADDR": "203.0.113.9" } },
        }));
        scrub_event(&mut e);
        assert!(e.user.is_none());
        assert!(e.request.is_none());
    }

    #[test]
    fn forbidden_keys_are_dropped_everywhere() {
        let mut e = ev(json!({
            "attachments": [1],
            "extra": { "calibration": "AAAA", "ok": 1, "nested": { "ZFL": "x", "keep": 2 } },
            "contexts": { "app": { "screenshot": "x", "name": "suite" } },
            "breadcrumbs": { "values": [{ "message": "m", "data": { "diff": "x", "stage": 3 } }] },
            "tags": { "zfle": "x", "flash.stage": "erase" },
            "exception": { "values": [{
                "type": "E",
                "mechanism": { "type": "t", "data": { "crash_dump": "x" } },
                "stacktrace": { "frames": [{ "function": "f", "vars": { "vin": "x" }, "instruction_addr": "0x10" }] }
            }]}
        }));
        scrub_event(&mut e);
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("attachments").is_none());
        assert_eq!(
            v.pointer("/extra"),
            Some(&json!({ "ok": 1, "nested": { "keep": 2 } }))
        );
        assert_eq!(
            v.pointer("/contexts/app"),
            Some(&json!({ "name": "suite" }))
        );
        assert_eq!(
            v.pointer("/breadcrumbs/values/0/data"),
            Some(&json!({ "stage": 3 }))
        );
        assert_eq!(v.pointer("/tags"), Some(&json!({ "flash.stage": "erase" })));
        assert_eq!(
            v.pointer("/exception/values/0/mechanism/data"),
            Some(&json!({}))
        );
        let frame = v
            .pointer("/exception/values/0/stacktrace/frames/0")
            .unwrap();
        assert!(frame.get("vars").is_none(), "frame locals are dropped");
        assert_eq!(frame.get("instruction_addr"), Some(&json!("0x10")));
    }

    #[test]
    fn tag_pairs_with_forbidden_keys_are_dropped() {
        let mut e = ev(json!({ "tags": [["zfl", "x"], ["ecu.family", "WBS8M9C50J5K12345"]] }));
        scrub_event(&mut e);
        assert_eq!(e.tags, Some(json!([["ecu.family", "<vin>"]])));
    }

    #[test]
    fn messages_breadcrumbs_tags_and_extra_are_scrubbed() {
        let mut e = ev(json!({
            "message": "flash of WBS8M9C50J5K12345 failed",
            "logentry": { "message": "addr 0x80040000", "formatted": "mail a@b.com" },
            "exception": { "values": [{ "type": "DmeRejection", "value": "reply 7f3478 at 0x8004ABCD" }] },
            "breadcrumbs": [{ "message": "Bearer abcdef", "data": { "addr": "0x0001FFFF" } }],
            "tags": { "transport.kind": "wifi", "x": "deadbeef00" },
            "extra": { "flash": { "note": "a@b.com" }, "list": ["0x12345678"] },
        }));
        scrub_event(&mut e);
        assert_eq!(e.message.as_deref(), Some("flash of <vin> failed"));
        let le = e.logentry.as_ref().unwrap();
        assert_eq!(le.message.as_deref(), Some("addr <hex>"));
        assert_eq!(le.formatted.as_deref(), Some("mail <email>"));
        assert_eq!(
            e.primary_exception().unwrap().value.as_deref(),
            Some("reply 7f3478 at <hex>")
        );
        assert_eq!(
            e.breadcrumbs,
            Some(json!([{ "message": "Bearer <secret>", "data": { "addr": "<hex>" } }]))
        );
        assert_eq!(
            e.tags,
            Some(json!({ "transport.kind": "wifi", "x": "<hex>" }))
        );
        assert_eq!(
            e.extra,
            Some(json!({ "flash": { "note": "<email>" }, "list": ["<hex>"] }))
        );
    }

    #[test]
    fn dart_stack_keeps_its_addresses() {
        // The non-symbolic Dart trace is what offline symbolication reads; its
        // build id and offsets are hex by nature.
        let stack = "build_id: '0f3a5b7c9d1e2f3a4152c37485e60718'\n\
                     #00 abs 000000750a2f5c1f virt 00000000002f5c1f _kDartIsolateSnapshotInstructions+0x2f5c1f";
        let mut e = ev(json!({ "extra": { "dart_stack": stack, "other": "0x12345678" } }));
        scrub_event(&mut e);
        let extra = e.extra.unwrap();
        assert_eq!(extra["dart_stack"], json!(stack));
        assert_eq!(extra["other"], json!("<hex>"));
    }

    #[test]
    fn build_id_and_cause_stacks_keep_their_hex() {
        // `extra.build_id` names the symbols file; each cause carries its own
        // non-symbolic trace. Both are read by offline symbolication.
        let stack = "build_id: '0f3a5b7c9d1e2f3a4152c37485e60718'\n\
                     #00 abs 000000750a2f5c1f virt 00000000002f5c1f _kDartIsolateSnapshotInstructions+0x2f5c1f";
        let mut e = ev(json!({ "extra": {
            "build_id": "0f3a5b7c9d1e2f3a4152c37485e60718",
            "causes": [
                { "type": "flash_failure", "message": "at 0x80040000", "dart_stack": stack },
                { "type": "host_refusal", "message": "host_refusal",
                  "dart_stack": format!("{stack}\nWBS8M9C50J5K12345"), "zfl": "x" },
                "0x12345678",
            ],
        }}));
        scrub_event(&mut e);
        let extra = e.extra.unwrap();
        assert_eq!(extra["build_id"], json!("0f3a5b7c9d1e2f3a4152c37485e60718"));
        let causes = &extra["causes"];
        assert_eq!(causes[0]["dart_stack"], json!(stack));
        // Only the stack is a trace: the rest of a cause keeps every rule.
        assert_eq!(causes[0]["message"], json!("at <hex>"));
        assert_eq!(
            causes[1]["dart_stack"],
            json!(format!("{stack}\n<vin>")),
            "the VIN rule holds inside a cause's stack"
        );
        assert!(causes[1].get("zfl").is_none(), "forbidden keys are dropped");
        assert_eq!(causes[2], json!("<hex>"));
    }

    #[test]
    fn causes_that_are_not_a_list_get_every_rule() {
        let mut e = ev(json!({ "extra": { "causes": { "dart_stack": "at 0x80040000" } } }));
        scrub_event(&mut e);
        assert_eq!(
            e.extra.unwrap()["causes"],
            json!({ "dart_stack": "at <hex>" })
        );
    }

    #[test]
    fn ecu_reset_record_keeps_its_hex() {
        // The Zeus app's ecu_reset event: the record's addresses and raw
        // replies are what pinpoints the reset.
        // The app sends raw replies as spaced bytes: a long digit-only run
        // would meet the VIN rule, which holds here too.
        let raw = "71 c2 00 01 05 00 05 08 18 00 0a 00 00 40 40 00";
        let mut e = ev(json!({
            "tags": {
                "reset.where": "0x801a678e",
                "reset.info": "0x00010006",
                "reset.cause_code": "0x20",
                "reset.tag": "0x44",
                "reset.cause": "TRAP",
                "ecu.software": "0x801a678e",
            },
            "extra": {
                "reset_record": { "entries": [{ "where": "0x801a678e", "note": "WBS8M9C50J5K12345" }] },
                "reset_record_raw": raw,
                "reset_details_raw": [raw, null],
                "flash": { "addr": "0x801a678e" },
            },
        }));
        scrub_event(&mut e);
        assert_eq!(
            e.tags,
            Some(json!({
                "reset.where": "0x801a678e",
                "reset.info": "0x00010006",
                "reset.cause_code": "0x20",
                "reset.tag": "0x44",
                "reset.cause": "TRAP",
                "ecu.software": "<hex>",
            }))
        );
        let extra = e.extra.unwrap();
        assert_eq!(
            extra["reset_record"]["entries"][0]["where"],
            json!("0x801a678e")
        );
        // The VIN rule still holds inside the record.
        assert_eq!(extra["reset_record"]["entries"][0]["note"], json!("<vin>"));
        assert_eq!(extra["reset_record_raw"], json!(raw));
        assert_eq!(extra["reset_details_raw"], json!([raw, null]));
        assert_eq!(extra["flash"]["addr"], json!("<hex>"));
    }

    #[test]
    fn every_reset_hex_tag_survives_and_still_loses_a_vin_or_email() {
        for key in ["reset.cause_code", "reset.tag", "reset.info", "reset.where"] {
            let mut e = ev(json!({ "tags": { key: "0x801a678e" } }));
            scrub_event(&mut e);
            assert_eq!(
                e.tags,
                Some(json!({ key: "0x801a678e" })),
                "{key} keeps its hex"
            );

            let mut e = ev(json!({ "tags": [[key, "WBS8M9C50J5K12345"], [key, "a@b.com"]] }));
            scrub_event(&mut e);
            assert_eq!(
                e.tags,
                Some(json!([[key, "<vin>"], [key, "<email>"]])),
                "{key} still gets the VIN and e-mail rules"
            );
        }
    }

    #[test]
    fn ecu_reset_tag_pairs_keep_their_hex() {
        let mut e = ev(json!({
            "tags": [["reset.where", "0x801a678e"], ["x", "0x801a678e"], ["reset.where2", "deadbeef00"]],
        }));
        scrub_event(&mut e);
        assert_eq!(
            e.tags,
            Some(json!([
                ["reset.where", "0x801a678e"],
                ["x", "<hex>"],
                ["reset.where2", "<hex>"]
            ]))
        );
    }

    #[test]
    fn debug_meta_is_not_rewritten() {
        let meta = json!({ "images": [{ "debug_id": "0f3a5b7c-0000-4000-8000-000000000001", "image_addr": "0x7a00000000" }] });
        let mut e = ev(json!({ "debug_meta": meta.clone() }));
        scrub_event(&mut e);
        assert_eq!(e.other.get("debug_meta"), Some(&meta));
    }

    #[test]
    fn scrub_strings_scrubs_each_entry() {
        let mut l = vec!["flash".to_string(), "WBS8M9C50J5K12345".to_string()];
        scrub_strings(&mut l);
        assert_eq!(l, vec!["flash".to_string(), "<vin>".to_string()]);
    }
}
