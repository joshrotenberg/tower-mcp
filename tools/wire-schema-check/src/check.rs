//! Frame-log processing: version resolution, request/response correlation and
//! validation against the vendored schemas.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::methods::{ERROR_RESPONSE_DEFS, RESULT_RESPONSE_DEFS, notification_def, request_defs};
use crate::schemas::Schemas;

/// Direction of a captured frame, from the server's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Received by the server.
    In,
    /// Sent by the server.
    Out,
}

impl Dir {
    fn opposite(self) -> Self {
        match self {
            Dir::In => Dir::Out,
            Dir::Out => Dir::In,
        }
    }
}

/// One line of the frame log.
#[derive(Debug, Clone)]
pub struct Line {
    pub dir: Dir,
    pub version: Option<String>,
    pub session: Option<String>,
    pub exchange: Option<u64>,
    pub frame: Value,
}

/// Parse one JSONL line of the frame log.
pub fn parse_line(text: &str) -> Result<Line, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let dir = match v.get("dir").and_then(Value::as_str) {
        Some("in") => Dir::In,
        Some("out") => Dir::Out,
        other => return Err(format!("bad `dir`: {other:?}")),
    };
    let frame = v.get("frame").cloned().ok_or("missing `frame`")?;
    let string = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(Line {
        dir,
        version: string("version"),
        session: string("session"),
        exchange: v.get("exchange").and_then(Value::as_u64),
        frame,
    })
}

/// Options for [`check_lines`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Also validate frames the server received. Off by default: inbound frames
    /// are produced by the test client, not by this crate, and some scenarios
    /// send malformed requests on purpose.
    pub include_inbound: bool,
}

/// A frame that failed validation against one definition.
#[derive(Debug, Clone)]
pub struct Failure {
    pub version: String,
    pub def: String,
    pub method: Option<String>,
    pub id: Option<Value>,
    pub errors: Vec<String>,
}

/// Counters for one protocol version.
#[derive(Debug, Default, Clone)]
pub struct VersionStats {
    /// Every frame seen at this version.
    pub frames: u64,
    /// Frames for which at least one validation ran.
    pub validated: u64,
    /// Validated frames that passed every check.
    pub passed: u64,
    /// Validated frames that failed at least one check.
    pub failed: u64,
    /// Inbound frames not validated.
    pub skipped_inbound: u64,
    /// Responses whose request was not in the log.
    pub uncorrelated: u64,
    /// Frames whose method is not in the table or not in this version's schema.
    pub unknown_methods: BTreeMap<String, u64>,
}

/// The outcome of checking a log.
#[derive(Debug, Default)]
pub struct Report {
    pub by_version: BTreeMap<String, VersionStats>,
    pub failures: Vec<Failure>,
    /// Frames whose version could not be determined, or has no vendored schema.
    pub unversioned: BTreeMap<String, u64>,
}

impl Report {
    /// Fold `other` into `self`. Used to combine logs from separate server runs.
    pub fn merge(&mut self, other: Report) {
        for (version, theirs) in other.by_version {
            let ours = self.by_version.entry(version).or_default();
            ours.frames += theirs.frames;
            ours.validated += theirs.validated;
            ours.passed += theirs.passed;
            ours.failed += theirs.failed;
            ours.skipped_inbound += theirs.skipped_inbound;
            ours.uncorrelated += theirs.uncorrelated;
            for (method, n) in theirs.unknown_methods {
                *ours.unknown_methods.entry(method).or_insert(0) += n;
            }
        }
        self.failures.extend(other.failures);
        for (version, n) in other.unversioned {
            *self.unversioned.entry(version).or_insert(0) += n;
        }
    }

    /// Failure counts keyed by (version, definition).
    pub fn failures_by_def(&self) -> BTreeMap<(String, String), u64> {
        let mut out = BTreeMap::new();
        for f in &self.failures {
            *out.entry((f.version.clone(), f.def.clone())).or_insert(0) += 1;
        }
        out
    }
}

/// A request's id in a form usable as a map key (`1` and `"1"` differ).
fn id_key(id: &Value) -> String {
    id.to_string()
}

enum Kind<'a> {
    Request { method: &'a str },
    Notification { method: &'a str },
    Response,
    Malformed,
}

fn classify(frame: &Value) -> Kind<'_> {
    let method = frame.get("method").and_then(Value::as_str);
    match (method, frame.get("id").is_some()) {
        (Some(method), true) => Kind::Request { method },
        (Some(method), false) => Kind::Notification { method },
        (None, _) if frame.get("result").is_some() || frame.get("error").is_some() => {
            Kind::Response
        }
        _ => Kind::Malformed,
    }
}

/// Lookup tables built in a first pass, so the order of lines does not matter.
#[derive(Default)]
struct Context {
    exchange_versions: HashMap<u64, String>,
    session_versions: HashMap<String, String>,
    /// (exchange, direction of the request, id) -> method
    by_exchange: HashMap<(u64, Dir, String), String>,
    /// (session, direction of the request, id) -> method
    by_session: HashMap<(String, Dir, String), String>,
}

impl Context {
    fn build(lines: &[Line]) -> Self {
        let mut ctx = Self::default();
        for line in lines {
            if let Some(version) = &line.version {
                if let Some(ex) = line.exchange {
                    ctx.exchange_versions
                        .entry(ex)
                        .or_insert_with(|| version.clone());
                }
                if let Some(s) = &line.session {
                    ctx.session_versions
                        .entry(s.clone())
                        .or_insert_with(|| version.clone());
                }
            }
            if let Kind::Request { method } = classify(&line.frame) {
                let key = id_key(&line.frame["id"]);
                if let Some(ex) = line.exchange {
                    ctx.by_exchange
                        .insert((ex, line.dir, key.clone()), method.to_string());
                }
                if let Some(s) = &line.session {
                    ctx.by_session
                        .insert((s.clone(), line.dir, key), method.to_string());
                }
            }
        }
        ctx
    }

    fn version(&self, line: &Line) -> Option<String> {
        line.version
            .clone()
            .or_else(|| self.exchange_versions.get(&line.exchange?).cloned())
            .or_else(|| self.session_versions.get(line.session.as_ref()?).cloned())
    }

    /// The method of the request a response answers.
    fn request_method(&self, line: &Line) -> Option<&str> {
        let id = line.frame.get("id")?;
        let key = id_key(id);
        let req_dir = line.dir.opposite();
        line.exchange
            .and_then(|ex| self.by_exchange.get(&(ex, req_dir, key.clone())))
            .or_else(|| {
                line.session
                    .as_ref()
                    .and_then(|s| self.by_session.get(&(s.clone(), req_dir, key)))
            })
            .map(String::as_str)
    }
}

/// Validate a captured log.
pub fn check_lines(lines: &[Line], schemas: &mut Schemas, opts: Options) -> Report {
    let ctx = Context::build(lines);
    let mut report = Report::default();

    for line in lines {
        let Some(version) = ctx.version(line) else {
            *report.unversioned.entry("unknown".into()).or_insert(0) += 1;
            continue;
        };
        if !schemas.has_version(&version) {
            *report.unversioned.entry(version).or_insert(0) += 1;
            continue;
        }
        let stats = report.by_version.entry(version.clone()).or_default();
        stats.frames += 1;
        if line.dir == Dir::In && !opts.include_inbound {
            stats.skipped_inbound += 1;
            continue;
        }

        let frame = &line.frame;
        // (definition, instance) pairs to validate; the method for reporting.
        let mut checks: Vec<(String, Value)> = Vec::new();
        let method: Option<String>;

        match classify(frame) {
            Kind::Request { method: m } => {
                method = Some(m.to_string());
                match request_defs(m).filter(|d| schemas.has_def(&version, d.request)) {
                    Some(defs) => checks.push((defs.request.to_string(), frame.clone())),
                    None => {
                        *stats.unknown_methods.entry(m.to_string()).or_insert(0) += 1;
                    }
                }
            }
            Kind::Notification { method: m } => {
                method = Some(m.to_string());
                match notification_def(m).filter(|d| schemas.has_def(&version, d)) {
                    Some(def) => checks.push((def.to_string(), frame.clone())),
                    None => {
                        *stats.unknown_methods.entry(m.to_string()).or_insert(0) += 1;
                    }
                }
            }
            Kind::Response if frame.get("error").is_some() => {
                method = ctx.request_method(line).map(str::to_string);
                if let Some(def) = schemas.first_def(&version, ERROR_RESPONSE_DEFS) {
                    checks.push((def.to_string(), frame.clone()));
                }
            }
            Kind::Response => {
                method = ctx.request_method(line).map(str::to_string);
                // Envelope: id and jsonrpc. The result body is checked against
                // its own definition below, so stand in a minimal valid one.
                if let Some(def) = schemas.first_def(&version, RESULT_RESPONSE_DEFS) {
                    let mut envelope = frame.clone();
                    if envelope["result"].is_object() {
                        envelope["result"] = json!({"resultType": "complete"});
                    }
                    checks.push((def.to_string(), envelope));
                }
                let result = &frame["result"];
                match method.as_deref() {
                    None => stats.uncorrelated += 1,
                    Some(m) => {
                        let input_required = result.get("resultType").and_then(Value::as_str)
                            == Some("input_required")
                            && schemas.has_def(&version, "InputRequiredResult");
                        let def = if input_required {
                            Some("InputRequiredResult")
                        } else {
                            request_defs(m)
                                .map(|d| d.result)
                                .filter(|d| schemas.has_def(&version, d))
                        };
                        match def {
                            Some(def) => checks.push((def.to_string(), result.clone())),
                            None => {
                                *stats.unknown_methods.entry(m.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
            Kind::Malformed => {
                // Not a request, notification or response: no definition to
                // pick, and `JSONRPCMessage` is absent from older schemas.
                stats.validated += 1;
                stats.failed += 1;
                report.failures.push(Failure {
                    version: version.clone(),
                    def: "JSONRPCMessage".to_string(),
                    method: None,
                    id: frame.get("id").cloned(),
                    errors: vec!["not a JSON-RPC request, notification or response".into()],
                });
                continue;
            }
        }

        if checks.is_empty() {
            continue;
        }
        stats.validated += 1;
        let mut failed = false;
        for (def, instance) in checks {
            let errors = schemas.validate(&version, &def, &instance);
            if !errors.is_empty() {
                failed = true;
                report.failures.push(Failure {
                    version: version.clone(),
                    def,
                    method: method.clone(),
                    id: frame.get("id").cloned(),
                    errors,
                });
            }
        }
        // `stats` borrow ended above; fetch again for the counters.
        let stats = report.by_version.get_mut(&version).expect("inserted above");
        if failed {
            stats.failed += 1;
        } else {
            stats.passed += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(dir: &str, version: &str, exchange: u64, frame: Value) -> Line {
        let text = json!({"dir": dir, "version": version, "session": null,
            "exchange": exchange, "frame": frame})
        .to_string();
        parse_line(&text).unwrap()
    }

    fn run(lines: &[Line]) -> Report {
        check_lines(lines, &mut Schemas::bundled(), Options::default())
    }

    fn tools_list_request() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
    }

    #[test]
    fn valid_response_passes() {
        let report = run(&[
            line("in", "2025-11-25", 1, tools_list_request()),
            line(
                "out",
                "2025-11-25",
                1,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [
                    {"name": "t", "inputSchema": {"type": "object"}}]}}),
            ),
        ]);
        let stats = &report.by_version["2025-11-25"];
        assert_eq!((stats.frames, stats.validated, stats.passed), (2, 1, 1));
        assert_eq!(stats.skipped_inbound, 1);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }

    #[test]
    fn malformed_result_fails_with_the_correlated_def() {
        let report = run(&[
            line("in", "2025-11-25", 1, tools_list_request()),
            // `tools` must be an array.
            line(
                "out",
                "2025-11-25",
                1,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": "nope"}}),
            ),
        ]);
        assert_eq!(report.failures.len(), 1);
        let failure = &report.failures[0];
        assert_eq!(failure.def, "ListToolsResult");
        assert_eq!(failure.method.as_deref(), Some("tools/list"));
        assert_eq!(report.by_version["2025-11-25"].failed, 1);
    }

    #[test]
    fn malformed_notification_fails_with_its_def() {
        let report = run(&[line(
            "out",
            "2025-11-25",
            1,
            // ProgressNotification requires progressToken and progress.
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
        )]);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].def, "ProgressNotification");
    }

    #[test]
    fn unknown_method_is_reported_not_failed() {
        let report = run(&[line(
            "out",
            "2025-11-25",
            1,
            json!({"jsonrpc": "2.0", "method": "notifications/vendor/thing", "params": {}}),
        )]);
        assert!(report.failures.is_empty());
        let stats = &report.by_version["2025-11-25"];
        assert_eq!(stats.unknown_methods["notifications/vendor/thing"], 1);
        assert_eq!(stats.validated, 0);
    }

    #[test]
    fn method_missing_from_a_versions_schema_is_unknown_there() {
        // tasks/get has no definition in 2025-06-18.
        let report = run(&[line(
            "out",
            "2025-06-18",
            1,
            json!({"jsonrpc": "2.0", "id": 9, "method": "tasks/get", "params": {"taskId": "x"}}),
        )]);
        assert!(report.failures.is_empty());
        assert_eq!(
            report.by_version["2025-06-18"].unknown_methods["tasks/get"],
            1
        );
    }

    #[test]
    fn response_correlates_within_an_exchange_not_across() {
        // Same id in two exchanges with different methods.
        let report = run(&[
            line("in", "2025-11-25", 1, tools_list_request()),
            line(
                "in",
                "2025-11-25",
                2,
                json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/list", "params": {}}),
            ),
            line(
                "out",
                "2025-11-25",
                2,
                json!({"jsonrpc": "2.0", "id": 1, "result": {"prompts": "bad"}}),
            ),
        ]);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].def, "ListPromptsResult");
    }

    #[test]
    fn response_correlates_by_session_across_exchanges() {
        // A server-initiated request answered by the client on a later POST.
        let mut request = line(
            "out",
            "2025-11-25",
            1,
            json!({"jsonrpc": "2.0", "id": 7, "method": "roots/list"}),
        );
        request.session = Some("s".into());
        let mut response = line(
            "in",
            "2025-11-25",
            2,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"roots": "bad"}}),
        );
        response.session = Some("s".into());
        let report = check_lines(
            &[request, response],
            &mut Schemas::bundled(),
            Options {
                include_inbound: true,
            },
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].def, "ListRootsResult");
    }

    #[test]
    fn uncorrelated_response_is_counted_not_failed() {
        let report = run(&[line(
            "out",
            "2025-11-25",
            1,
            json!({"jsonrpc": "2.0", "id": 5, "result": {}}),
        )]);
        assert!(report.failures.is_empty());
        assert_eq!(report.by_version["2025-11-25"].uncorrelated, 1);
    }

    #[test]
    fn error_response_validates_against_the_error_def() {
        let report = run(&[
            line(
                "out",
                "2025-11-25",
                1,
                json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no"}}),
            ),
            line(
                "out",
                "2025-11-25",
                2,
                // `code` must be an integer.
                json!({"jsonrpc": "2.0", "id": 2, "error": {"code": "x", "message": "no"}}),
            ),
        ]);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].def, "JSONRPCErrorResponse");
        // The 2025-06-18 schema names the same definition `JSONRPCError`.
        let old = run(&[line(
            "out",
            "2025-06-18",
            1,
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": "x", "message": "no"}}),
        )]);
        assert_eq!(old.failures[0].def, "JSONRPCError");
    }

    #[test]
    fn null_version_resolves_from_the_exchange() {
        let mut initialize = line(
            "in",
            "2025-11-25",
            1,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        );
        initialize.version = None;
        let report = check_lines(
            &[
                initialize,
                line(
                    "out",
                    "2025-11-25",
                    1,
                    json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
                ),
            ],
            &mut Schemas::bundled(),
            Options {
                include_inbound: true,
            },
        );
        // The inbound initialize resolves to 2025-11-25 and fails (no
        // protocolVersion), as does the empty initialize result.
        let defs: Vec<_> = report.failures.iter().map(|f| f.def.as_str()).collect();
        assert!(defs.contains(&"InitializeRequest"), "{defs:?}");
        assert!(defs.contains(&"InitializeResult"), "{defs:?}");
    }

    #[test]
    fn unresolvable_version_is_counted_separately() {
        let mut l = line("out", "2025-11-25", 1, tools_list_request());
        l.version = None;
        l.exchange = None;
        let report = run(&[l]);
        assert_eq!(report.unversioned["unknown"], 1);
        assert!(report.by_version.is_empty());
    }

    #[test]
    fn merge_sums_counters_and_keeps_failures() {
        let bad = |id| {
            vec![
                line("in", "2025-11-25", 1, tools_list_request()),
                line(
                    "out",
                    "2025-11-25",
                    1,
                    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": 1}}),
                ),
            ]
        };
        let mut a = run(&bad(1));
        a.merge(run(&bad(1)));
        let stats = &a.by_version["2025-11-25"];
        assert_eq!((stats.frames, stats.validated, stats.failed), (4, 2, 2));
        assert_eq!(a.failures.len(), 2);
    }
}
