//! Cron trigger and one-off scheduled event commands: `create_cron_trigger`,
//! `delete_cron_trigger`, `get_cron_triggers`, `create_scheduled_event`,
//! `delete_scheduled_event`, `get_scheduled_events` and `get_scheduled_event_invocations`.
//!
//! Cron triggers live in metadata; their events and the one-off events live in the metadata
//! database. As in V2, creating a cron trigger stores its next 100 events at once, replacing
//! it regenerates them, and deleting it drops its untried future events. Unlike V2, those
//! changes are queued on the [`Ctx`] and made only once the metadata is committed, so a
//! rejected request (e.g. a failing `bulk` item) never changes what fires.

use deadpool_postgres::Pool;
use hge_core::Code;
use hge_events::admin::sorted;
use hge_events::cron::CronSchedule;
use hge_events::scheduled::Kind;
use hge_events::scheduled_admin::{
    self as sa, CronEventChange, InvocationsOf, NewOneOff, OrderBy, Page,
};
use hge_transforms::{RequestTransform, ResponseTransform, validate_transforms};
use serde_json::{Map, Number, Value, json};

use super::args::{
    Obj, boolean, field, headers, int, missing, object, required_string, required_text, text,
    transform_invalid, transforms,
};
use crate::api::json_kind;
use crate::ctx::{Check, Ctx};
use crate::response::{ApiError, ApiResult, already_exists, not_exists, parse_failed, success};
use crate::runtime::Runtime;

const STATUSES: [&str; 5] = ["scheduled", "locked", "delivered", "error", "dead"];

// ---------------------------------------------------------------------------------------
// Argument parsing.

/// A whole number as aeson writes it (`10`, not `10.0`).
fn number(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        Value::from(f as i64)
    } else {
        Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

/// A `NonNegativeDiffTime` field of `STRetryConf`.
fn seconds(obj: &Obj, key: &str, path: &str, default: f64) -> ApiResult<f64> {
    let p = format!("{path}.{key}");
    match field(obj, key) {
        None => Ok(default),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f >= 0.0 => Ok(f),
            _ => Err(parse_failed(
                &p,
                "  The predicate (From 0) failed with the message: Value is less than 0\n",
            )),
        },
        Some(v) => Err(parse_failed(
            &p,
            format!(
                "parsing DiffTime failed, expected Number, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

/// `STRetryConf`, with V2's defaults filled in.
fn retry_conf(obj: &Obj, path: &str) -> ApiResult<Value> {
    let Some(v) = field(obj, "retry_conf") else {
        return Ok(json!({
            "num_retries": 0,
            "retry_interval_seconds": 10,
            "timeout_seconds": 60,
            "tolerance_seconds": 21600,
        }));
    };
    let path = format!("{path}.retry_conf");
    let conf = object(v, &path)?;
    let num_retries = int(conf, "num_retries", &path)?.unwrap_or(0);
    let interval = seconds(conf, "retry_interval_seconds", &path, 10.0)?;
    let timeout = seconds(conf, "timeout_seconds", &path, 60.0)?;
    let tolerance = seconds(conf, "tolerance_seconds", &path, 21600.0)?;
    if num_retries < 0 {
        return Err(parse_failed(
            &path,
            "num_retries cannot be a negative value",
        ));
    }
    Ok(json!({
        "num_retries": num_retries,
        "retry_interval_seconds": number(interval),
        "timeout_seconds": number(timeout),
        "tolerance_seconds": number(tolerance),
    }))
}

/// `create_cron_trigger` arguments, in the order V2 decodes them.
struct CronArgs {
    name: String,
    webhook: String,
    payload: Option<Value>,
    retry_conf: Value,
    headers: Vec<Value>,
    schedule: CronSchedule,
    include_in_metadata: bool,
    comment: Option<String>,
    replace: bool,
    request_transform: Option<RequestTransform>,
    response_transform: Option<ResponseTransform>,
}

impl CronArgs {
    fn parse(args: &Value) -> ApiResult<Self> {
        let p = "$.args";
        let obj = object(args, p)?;
        let name = required_string(obj, "name", p, "String")?;
        if name.is_empty() {
            return Err(parse_failed(
                &format!("{p}.name"),
                "empty string not allowed",
            ));
        }
        let webhook = required_text(obj, "webhook", p)?;
        let payload = field(obj, "payload").cloned();
        let retry_conf = retry_conf(obj, p)?;
        let headers = headers(obj, p)?.unwrap_or_default();
        let schedule = required_text(obj, "schedule", p)?;
        let schedule = CronSchedule::parse_v2(&schedule)
            .map_err(|e| parse_failed(&format!("{p}.schedule"), e))?;
        let include_in_metadata = match obj.get("include_in_metadata") {
            None => return Err(missing(p, "include_in_metadata")),
            Some(Value::Bool(b)) => *b,
            Some(v) => {
                return Err(parse_failed(
                    &format!("{p}.include_in_metadata"),
                    format!(
                        "parsing Bool failed, expected Boolean, but encountered {}",
                        json_kind(v)
                    ),
                ));
            }
        };
        let comment = text(obj, "comment", p)?;
        let replace = boolean(obj, "replace", p)?.unwrap_or(false);
        let (request_transform, response_transform) = transforms(obj)?;
        validate_transforms(request_transform.as_ref(), response_transform.as_ref())
            .map_err(transform_invalid)?;
        Ok(CronArgs {
            name,
            webhook,
            payload,
            retry_conf,
            headers,
            schedule,
            include_in_metadata,
            comment,
            replace,
            request_transform,
            response_transform,
        })
    }

    /// The metadata entry, as V2 stores it (normalised schedule, full `retry_conf`,
    /// `headers` always present).
    fn to_metadata(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("webhook".into(), json!(self.webhook));
        m.insert("schedule".into(), json!(self.schedule.to_string()));
        m.insert(
            "include_in_metadata".into(),
            Value::Bool(self.include_in_metadata),
        );
        if let Some(p) = &self.payload {
            m.insert("payload".into(), p.clone());
        }
        m.insert("retry_conf".into(), self.retry_conf.clone());
        m.insert("headers".into(), Value::Array(self.headers.clone()));
        if let Some(c) = &self.comment {
            m.insert("comment".into(), json!(c));
        }
        if let Some(t) = &self.request_transform {
            m.insert("request_transform".into(), t.to_json());
        }
        if let Some(t) = &self.response_transform {
            m.insert("response_transform".into(), t.to_json());
        }
        Value::Object(m)
    }
}

/// `ScheduledEventType`, the `type` of the event commands.
fn event_type(obj: &Obj, path: &str, missing_type: impl Fn() -> ApiError) -> ApiResult<Kind> {
    match obj.get("type") {
        None => Err(missing_type()),
        Some(Value::String(s)) if s == "cron" => Ok(Kind::Cron),
        Some(Value::String(s)) if s == "one_off" => Ok(Kind::OneOff),
        Some(v) => Err(parse_failed(
            &format!("{path}.type"),
            format!(
                "parsing Hasura.RQL.Types.ScheduledTrigger.ScheduledEventType failed, expected one of the tags [\"cron\",\"one_off\"], but found tag {v}"
            ),
        )),
    }
}

/// `limit`, `offset` and `get_rows_count`.
fn page(obj: &Obj, path: &str) -> ApiResult<Page> {
    Ok(Page {
        limit: int(obj, "limit", path)?,
        offset: int(obj, "offset", path)?,
        count: boolean(obj, "get_rows_count", path)?.unwrap_or(false),
    })
}

fn statuses(obj: &Obj, path: &str) -> ApiResult<Vec<String>> {
    let path = format!("{path}.status");
    match field(obj, "status") {
        None => Ok(vec![]),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, s)| match s {
                Value::String(s) if STATUSES.contains(&s.as_str()) => Ok(s.clone()),
                Value::String(s) => Err(parse_failed(
                    &format!("{path}[{i}]"),
                    format!("unexpected status: {s}"),
                )),
                v => Err(parse_failed(
                    &format!("{path}[{i}]"),
                    format!(
                        "parsing Text failed, expected String, but encountered {}",
                        json_kind(v)
                    ),
                )),
            })
            .collect(),
        Some(v) => Err(parse_failed(
            &path,
            format!(
                "parsing [] failed, expected Array, but encountered {}",
                json_kind(v)
            ),
        )),
    }
}

/// `order_by`: `[{"column": c, "type": "asc" | "desc"}]`.
fn order_by(obj: &Obj, path: &str) -> ApiResult<Vec<OrderBy>> {
    let path = format!("{path}.order_by");
    let items = match field(obj, "order_by") {
        None => return Ok(vec![]),
        Some(Value::Array(items)) => items,
        Some(v) => {
            return Err(parse_failed(
                &path,
                format!(
                    "parsing [] failed, expected Array, but encountered {}",
                    json_kind(v)
                ),
            ));
        }
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let p = format!("{path}[{i}]");
            let o = object(item, &p)?;
            let column = required_text(o, "column", &p)?;
            let descending = match text(o, "type", &p)?.as_deref() {
                None | Some("asc") => false,
                Some("desc") => true,
                Some(other) => {
                    return Err(parse_failed(
                        &format!("{p}.type"),
                        format!("unexpected order by type: {other}"),
                    ));
                }
            };
            Ok(OrderBy { column, descending })
        })
        .collect()
}

/// Check `schedule_at` like aeson's `UTCTime` parser (an ISO 8601 time with a zone) and
/// return it in a form Postgres reads as `timestamptz`.
fn parse_utc_time(s: &str) -> Result<String, String> {
    let fail = |c: Option<char>, what: &str| {
        let found = match c {
            Some(c) => format!("'{c:?}"),
            None => "end-of-input".to_owned(),
        };
        format!("could not parse date: Unexpected {found}, expecting {what}")
    };
    let mut chars = s.chars().peekable();
    // Read exactly `n` digits (`at_least` allows more) as a number.
    let number = |chars: &mut std::iter::Peekable<std::str::Chars>, n: usize, at_least: bool| {
        let mut value: u64 = 0;
        let mut count = 0;
        while count < n || (at_least && chars.peek().is_some_and(char::is_ascii_digit)) {
            match chars.next() {
                Some(c) if c.is_ascii_digit() => {
                    value = value
                        .saturating_mul(10)
                        .saturating_add(u64::from(c as u8 - b'0'));
                    count += 1;
                }
                c => return Err(fail(c, "a digit")),
            }
        }
        Ok(value)
    };
    let expect =
        |chars: &mut std::iter::Peekable<std::str::Chars>, want: &[char]| match chars.next() {
            Some(c) if want.contains(&c) => Ok(()),
            c => {
                let what = want
                    .iter()
                    .map(|w| format!("'{w}'"))
                    .collect::<Vec<_>>()
                    .join(" or ");
                Err(fail(c, &what))
            }
        };
    let negative = match chars.peek() {
        Some('-') => {
            chars.next();
            true
        }
        Some('+') => {
            chars.next();
            false
        }
        Some(c) if c.is_ascii_digit() => false,
        c => return Err(fail(c.copied(), "-, +, or a digit")),
    };
    let year = number(&mut chars, 4, true)?;
    expect(&mut chars, &['-'])?;
    let month = number(&mut chars, 2, false)?;
    expect(&mut chars, &['-'])?;
    let day = number(&mut chars, 2, false)?;
    expect(&mut chars, &['T', 't', ' '])?;
    let hour = number(&mut chars, 2, false)?;
    expect(&mut chars, &[':'])?;
    let minute = number(&mut chars, 2, false)?;
    let mut second = 0;
    if chars.peek() == Some(&':') {
        chars.next();
        second = number(&mut chars, 2, false)?;
        if chars.peek() == Some(&'.') {
            chars.next();
            number(&mut chars, 1, true)?;
        }
    }
    let zone = match chars.next() {
        Some('Z' | 'z') => (0, 0),
        Some('+' | '-') => {
            let h = number(&mut chars, 2, false)?;
            if chars.peek() == Some(&':') {
                chars.next();
            }
            let m = if chars.peek().is_some() {
                number(&mut chars, 2, false)?
            } else {
                0
            };
            (h, m)
        }
        c => return Err(fail(c, "timezone")),
    };
    if let Some(c) = chars.next() {
        return Err(fail(Some(c), "end-of-input"));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let invalid = |what: &str| Err(format!("could not parse date: invalid {what}"));
    if negative {
        return invalid("year");
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month).contains(&day) {
        return invalid("date");
    }
    if hour > 23 || minute > 59 || second > 59 {
        return invalid("time of day");
    }
    if zone.0 > 23 || zone.1 > 59 {
        return invalid("time zone");
    }
    // Postgres does not take a `+` before the year.
    Ok(s.strip_prefix('+').unwrap_or(s).to_owned())
}

// ---------------------------------------------------------------------------------------
// Helpers.

fn metadata_db<'a, R: Runtime>(ctx: &Ctx<'a, R>) -> ApiResult<&'a Pool> {
    ctx.rt.metadata_db().ok_or_else(|| {
        ApiError::bad(
            Code::NotSupported,
            "scheduled events need a metadata database",
        )
    })
}

fn query_error(e: String) -> ApiError {
    ApiError::new(500, Code::Unexpected, "database query error")
        .with_internal(json!({ "error": { "message": e } }))
}

/// Where a cron trigger is in the working metadata, inconsistent or not.
fn stored_position<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> Option<usize> {
    ctx.metadata
        .cron_triggers
        .iter()
        .position(|t| t.get("name").and_then(Value::as_str) == Some(name))
}

/// Is a cron trigger of the working metadata inconsistent?
fn inconsistent<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> bool {
    let id = format!("cron_trigger {name}");
    ctx.inconsistencies
        .iter()
        .any(|o| o.kind == "cron_trigger" && o.name == id)
}

/// Where a consistent cron trigger is: V2 looks triggers up in its schema cache, which
/// leaves inconsistent ones out.
fn cron_position<R: Runtime>(ctx: &Ctx<'_, R>, name: &str) -> Option<usize> {
    stored_position(ctx, name).filter(|_| !inconsistent(ctx, name))
}

fn cron_missing(name: &str) -> ApiError {
    not_exists(format!("cron trigger with name: {name} does not exist"))
}

/// `{"count": n, key: rows}` (`count` only when asked for).
fn listing(count: Option<i64>, key: &str, rows: Vec<Value>) -> Value {
    let mut m = Map::new();
    if let Some(n) = count {
        m.insert("count".into(), json!(n));
    }
    m.insert(key.into(), Value::Array(rows));
    Value::Object(m)
}

// ---------------------------------------------------------------------------------------
// Commands.

pub async fn create_cron_trigger<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let t = CronArgs::parse(args)?;
    match (cron_position(ctx, &t.name), t.replace) {
        (Some(_), false) => {
            return Err(already_exists(format!(
                "cron trigger with name: {} already exists",
                t.name
            )));
        }
        (None, true) => return Err(cron_missing(&t.name)),
        _ => {}
    }
    // An inconsistent trigger of the same name is overwritten, as in V2.
    let stored = stored_position(ctx, &t.name);
    match stored {
        Some(i) => ctx.metadata.cron_triggers[i] = t.to_metadata(),
        None => ctx.metadata.cron_triggers.push(t.to_metadata()),
    }
    ctx.check(Check::object(
        "cron_trigger",
        format!("cron_trigger {}", t.name),
    ))
    .await?;
    ctx.cron_changes.push(CronEventChange::Populate {
        trigger: t.name,
        schedule: t.schedule,
        replace: stored.is_some(),
    });
    Ok(success())
}

pub async fn delete_cron_trigger<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let obj = object(args, "$.args")?;
    let name = required_string(obj, "name", "$.args", "String")?;
    let i = cron_position(ctx, &name).ok_or_else(|| cron_missing(&name))?;
    ctx.metadata.cron_triggers.remove(i);
    ctx.changed().await?;
    ctx.cron_changes
        .push(CronEventChange::Drop { trigger: name });
    Ok(success())
}

/// Every consistent cron trigger, by name, with V2's normalised fields and sorted keys.
pub fn get_cron_triggers<R: Runtime>(ctx: &Ctx<'_, R>) -> ApiResult<Value> {
    let mut triggers: Vec<Value> = ctx
        .metadata
        .cron_triggers
        .iter()
        .filter(|raw| !inconsistent(ctx, raw["name"].as_str().unwrap_or_default()))
        .map(|raw| {
            let normalised = CronArgs::parse(raw).map_or_else(|_| raw.clone(), |t| t.to_metadata());
            sorted(normalised)
        })
        .collect();
    triggers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({ "cron_triggers": triggers }))
}

pub async fn create_scheduled_event<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let webhook = required_text(obj, "webhook", p)?;
    let schedule_at = required_text(obj, "schedule_at", p)?;
    let schedule_at =
        parse_utc_time(&schedule_at).map_err(|e| parse_failed("$.args.schedule_at", e))?;
    let payload = field(obj, "payload").map(Value::to_string);
    let headers = Value::Array(headers(obj, p)?.unwrap_or_default()).to_string();
    let retry_conf = retry_conf(obj, p)?.to_string();
    let comment = text(obj, "comment", p)?;
    // Checked but not kept: like V2, which has nowhere to store them, one-off events are
    // delivered untransformed.
    transforms(obj)?;
    let pool = metadata_db(ctx)?;
    let id = sa::create_one_off(
        pool,
        &NewOneOff {
            webhook_conf: &Value::String(webhook).to_string(),
            schedule_at: &schedule_at,
            payload: payload.as_deref(),
            retry_conf: &retry_conf,
            header_conf: &headers,
            comment: comment.as_deref(),
        },
    )
    .await
    .map_err(query_error)?;
    Ok(json!({ "event_id": id, "message": "success" }))
}

pub async fn delete_scheduled_event<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let generic = |key: &str| {
        parse_failed(
            p,
            format!(
                "parsing Hasura.RQL.Types.ScheduledTrigger.DeleteScheduledEvent(DeleteScheduledEvent) failed, key \"{key}\" not found"
            ),
        )
    };
    let kind = event_type(obj, p, || generic("type"))?;
    if !obj.contains_key("event_id") {
        return Err(generic("event_id"));
    }
    let event_id = required_text(obj, "event_id", p)?;
    sa::delete_event(metadata_db(ctx)?, kind, &event_id)
        .await
        .map_err(query_error)?;
    Ok(success())
}

pub async fn get_scheduled_events<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let kind = event_type(obj, p, || missing(p, "type"))?;
    let trigger = match kind {
        Kind::Cron => Some(required_text(obj, "trigger_name", p)?),
        Kind::OneOff => None,
    };
    let page = page(obj, p)?;
    let statuses = statuses(obj, p)?;
    let order_by = order_by(obj, p)?;
    if let Some(name) = &trigger {
        cron_position(ctx, name).ok_or_else(|| cron_missing(name))?;
    }
    let (count, events) = sa::events(
        metadata_db(ctx)?,
        kind,
        trigger.as_deref(),
        &statuses,
        &order_by,
        &page,
    )
    .await
    .map_err(query_error)?;
    Ok(listing(count, "events", events))
}

pub async fn get_scheduled_event_invocations<R: Runtime>(
    ctx: &mut Ctx<'_, R>,
    args: &Value,
) -> ApiResult<Value> {
    let p = "$.args";
    let obj = object(args, p)?;
    let kind = event_type(obj, p, || missing(p, "type"))?;
    let page = page(obj, p)?;
    let event_id = text(obj, "event_id", p)?;
    let trigger = match (&event_id, kind) {
        (None, Kind::Cron) => {
            let name = required_text(obj, "trigger_name", p)?;
            cron_position(ctx, &name).ok_or_else(|| cron_missing(&name))?;
            Some(name)
        }
        _ => None,
    };
    let of = match (&event_id, &trigger) {
        (Some(id), _) => InvocationsOf::Event(id),
        (None, Some(name)) => InvocationsOf::Trigger(name),
        (None, None) => InvocationsOf::AllOneOff,
    };
    let (count, invocations) = sa::invocations(metadata_db(ctx)?, kind, of, &page)
        .await
        .map_err(query_error)?;
    Ok(listing(count, "invocations", invocations))
}

#[cfg(test)]
mod tests {
    use super::parse_utc_time;

    #[test]
    fn utc_times() {
        for ok in [
            "2030-05-01T10:00:00Z",
            "2030-05-01 12:30:00.5+02:00",
            "2030-05-01T10:00+0530",
            "2030-05-01t10:00:00-01",
            "2028-02-29T00:00:00Z",
        ] {
            assert_eq!(parse_utc_time(ok).as_deref(), Ok(ok), "{ok}");
        }
        assert_eq!(
            parse_utc_time("+2030-05-01T10:00:00Z").as_deref(),
            Ok("2030-05-01T10:00:00Z")
        );
        assert_eq!(
            parse_utc_time("2030-05-01T10:00:00").unwrap_err(),
            "could not parse date: Unexpected end-of-input, expecting timezone"
        );
        assert_eq!(
            parse_utc_time("nope").unwrap_err(),
            "could not parse date: Unexpected ''n', expecting -, +, or a digit"
        );
        for bad in [
            "2030-13-45T10:00:00Z",
            "2030-02-29T10:00:00Z",
            "2030-05-01T25:00:00Z",
            "2030-05-01T10:00:00+24:00",
        ] {
            assert!(parse_utc_time(bad).is_err(), "{bad}");
        }
    }
}
