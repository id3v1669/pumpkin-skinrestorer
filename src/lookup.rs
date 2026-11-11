//! Lookup engine: turns "find the skin for this name in these sources" into batched,
//! deduplicated, rate-limit-aware HTTP requests. Pure: the caller sends the requests and
//! feeds the responses back, passing the current time in milliseconds.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::source::{
    HttpRequest, HttpResponse, SkinData, Source, parse_skin, parse_uuid_batch, skin_request,
    uuid_batch_request,
};

pub type QueryId = u64;
pub type ReqId = u64;

/// Names per bulk request, and the window after which a partial batch is sent anyway.
pub const BATCH_SIZE: usize = 10;
pub const BATCH_WINDOW_MS: u64 = 1000;
pub const MAX_IN_FLIGHT: usize = 16;
/// Distinct lookups (source + name) queued or running at once.
pub const MAX_QUEUED: usize = 256;
/// Pause after HTTP 429 without a `Retry-After` header.
pub const DEFAULT_BACKOFF_SECS: u64 = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Found {
        source: Source,
        /// Lowercase account name.
        name: String,
        uuid: String,
        skin: SkinData,
        /// Sources tried before `source` that have no usable skin for the name.
        not_found_in: Vec<Source>,
    },
    /// Every source answered that it has no usable skin.
    NotFound { not_found_in: Vec<Source> },
    Failed {
        error: String,
        not_found_in: Vec<Source>,
    },
}

impl Outcome {
    /// Sources that answered an authoritative "not found" for the query's name: HTTP
    /// 204/404, a name missing from the bulk answer, or a skin that is not signed or
    /// not vanilla. Transport errors, rate limits, other HTTP statuses and "busy" are
    /// not listed: they say nothing about the account.
    pub fn not_found_in(&self) -> &[Source] {
        match self {
            Outcome::Found { not_found_in, .. }
            | Outcome::NotFound { not_found_in }
            | Outcome::Failed { not_found_in, .. } => not_found_in,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

type Key = (Source, String);

struct Query {
    name: String,
    remaining: VecDeque<Source>,
    errors: Vec<String>,
    not_found_in: Vec<Source>,
}

enum Stage {
    WantUuid,
    UuidSent,
    WantSkin { uuid: String },
    SkinSent { uuid: String },
}

struct KeyState {
    stage: Stage,
    waiters: Vec<QueryId>,
}

enum Req {
    Batch { source: Source, keys: Vec<Key> },
    Skin { key: Key },
}

#[derive(Default)]
pub struct Engine {
    next_id: u64,
    queries: HashMap<QueryId, Query>,
    keys: BTreeMap<Key, KeyState>,
    /// Keys waiting for a bulk name lookup, per source, with the time the first one
    /// was queued.
    pending: BTreeMap<Source, (u64, Vec<Key>)>,
    requests: HashMap<ReqId, Req>,
    backoff_until: HashMap<Source, u64>,
    finished: Vec<(QueryId, Outcome)>,
}

impl Engine {
    pub fn new() -> Engine {
        Engine::default()
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Distinct lookups currently queued or running.
    pub fn queued(&self) -> usize {
        self.keys.len()
    }

    /// Starts looking up `name` in `sources`, in order, until one has a skin. `Busy` when
    /// the queue is full and none of the sources already has a lookup for this name.
    pub fn submit(&mut self, name: &str, sources: &[Source], now: u64) -> Result<QueryId, Busy> {
        let name = name.to_ascii_lowercase();
        if self.keys.len() >= MAX_QUEUED
            && !sources
                .iter()
                .any(|&s| self.keys.contains_key(&(s, name.clone())))
        {
            return Err(Busy);
        }
        let id = self.id();
        self.queries.insert(
            id,
            Query {
                name,
                remaining: sources.iter().copied().collect(),
                errors: Vec::new(),
                not_found_in: Vec::new(),
            },
        );
        self.advance(id, now);
        Ok(id)
    }

    /// Outcomes of finished queries since the last call.
    pub fn take_finished(&mut self) -> Vec<(QueryId, Outcome)> {
        std::mem::take(&mut self.finished)
    }

    fn paused(&self, source: Source, now: u64) -> bool {
        self.backoff_until.get(&source).is_some_and(|&t| now < t)
    }

    /// Moves a query to its next usable source, or finishes it.
    fn advance(&mut self, id: QueryId, now: u64) {
        loop {
            let Some(query) = self.queries.get_mut(&id) else {
                return;
            };
            let Some(source) = query.remaining.pop_front() else {
                let query = self.queries.remove(&id).expect("query exists");
                let not_found_in = query.not_found_in;
                let outcome = if query.errors.is_empty() {
                    Outcome::NotFound { not_found_in }
                } else {
                    Outcome::Failed {
                        error: query.errors.join("; "),
                        not_found_in,
                    }
                };
                self.finished.push((id, outcome));
                return;
            };
            let key = (source, query.name.clone());
            // A lookup that is already queued or running is joined, whatever its stage.
            if let Some(state) = self.keys.get_mut(&key) {
                state.waiters.push(id);
                return;
            }
            // Only starting a new lookup is refused on a paused source or a full queue.
            if self.backoff_until.get(&source).is_some_and(|&t| now < t) {
                query
                    .errors
                    .push(format!("{} is rate limited", source.as_str()));
                continue;
            }
            if self.keys.len() >= MAX_QUEUED {
                if !query.errors.iter().any(|e| e == "busy") {
                    query.errors.push("busy".into());
                }
                continue;
            }
            self.keys.insert(
                key.clone(),
                KeyState {
                    stage: Stage::WantUuid,
                    waiters: vec![id],
                },
            );
            let entry = self.pending.entry(source).or_insert((now, Vec::new()));
            if entry.1.is_empty() {
                entry.0 = now;
            }
            entry.1.push(key);
            return;
        }
    }

    fn finish_key(
        &mut self,
        key: &Key,
        result: Result<Option<(String, SkinData)>, String>,
        now: u64,
    ) {
        let Some(state) = self.keys.remove(key) else {
            return;
        };
        for id in state.waiters {
            match &result {
                Ok(Some((uuid, skin))) => {
                    let not_found_in = self
                        .queries
                        .remove(&id)
                        .map(|q| q.not_found_in)
                        .unwrap_or_default();
                    self.finished.push((
                        id,
                        Outcome::Found {
                            source: key.0,
                            name: key.1.clone(),
                            uuid: uuid.clone(),
                            skin: skin.clone(),
                            not_found_in,
                        },
                    ));
                }
                Ok(None) => {
                    if let Some(q) = self.queries.get_mut(&id) {
                        q.not_found_in.push(key.0);
                    }
                    self.advance(id, now);
                }
                Err(e) => {
                    if let Some(q) = self.queries.get_mut(&id) {
                        q.errors.push(format!("{}: {e}", key.0.as_str()));
                    }
                    self.advance(id, now);
                }
            }
        }
    }

    /// Requests to start now.
    pub fn poll_requests(&mut self, now: u64) -> Vec<(ReqId, HttpRequest)> {
        let mut out = Vec::new();
        for source in Source::ALL {
            let due = self.pending.get(&source).is_some_and(|(first, keys)| {
                !keys.is_empty()
                    && (keys.len() >= BATCH_SIZE || now.saturating_sub(*first) >= BATCH_WINDOW_MS)
            });
            if !due {
                continue;
            }
            if self.paused(source, now) {
                let (_, keys) = self.pending.remove(&source).expect("due batch exists");
                for key in keys {
                    self.finish_key(&key, Err("rate limited".into()), now);
                }
                continue;
            }
            if self.requests.len() >= MAX_IN_FLIGHT {
                break;
            }
            let (first, keys) = self.pending.get_mut(&source).expect("due batch exists");
            let batch: Vec<Key> = keys.drain(..keys.len().min(BATCH_SIZE)).collect();
            *first = now;
            for key in &batch {
                if let Some(state) = self.keys.get_mut(key) {
                    state.stage = Stage::UuidSent;
                }
            }
            let names: Vec<String> = batch.iter().map(|k| k.1.clone()).collect();
            let id = self.id();
            out.push((id, uuid_batch_request(source, &names)));
            self.requests.insert(
                id,
                Req::Batch {
                    source,
                    keys: batch,
                },
            );
        }

        let ready: Vec<(Key, String)> = self
            .keys
            .iter()
            .filter_map(|(k, s)| match &s.stage {
                Stage::WantSkin { uuid } => Some((k.clone(), uuid.clone())),
                _ => None,
            })
            .collect();
        for (key, uuid) in ready {
            if self.requests.len() >= MAX_IN_FLIGHT {
                break;
            }
            if self.paused(key.0, now) {
                self.finish_key(&key, Err("rate limited".into()), now);
                continue;
            }
            if let Some(state) = self.keys.get_mut(&key) {
                state.stage = Stage::SkinSent { uuid: uuid.clone() };
            }
            let id = self.id();
            out.push((id, skin_request(key.0, &key.1, &uuid)));
            self.requests.insert(id, Req::Skin { key });
        }
        out
    }

    fn note_rate_limit(&mut self, source: Source, response: &HttpResponse, now: u64) {
        let secs = response.retry_after.unwrap_or(DEFAULT_BACKOFF_SECS);
        // Never shortens a pause that is already running.
        let until = now.saturating_add(secs.saturating_mul(1000));
        let e = self.backoff_until.entry(source).or_insert(0);
        *e = (*e).max(until);
    }

    /// Feeds back the result of a request from [`Engine::poll_requests`].
    pub fn on_response(&mut self, req: ReqId, result: Result<HttpResponse, String>, now: u64) {
        let Some(req) = self.requests.remove(&req) else {
            return;
        };
        match req {
            Req::Batch { source, keys } => {
                let found = match result {
                    Err(e) => Err(e),
                    Ok(r) if r.status == 429 => {
                        self.note_rate_limit(source, &r, now);
                        Err("rate limited".into())
                    }
                    Ok(r) if r.status == 204 || r.status == 404 => Ok(Vec::new()),
                    Ok(r) if r.status == 200 => parse_uuid_batch(&r.body),
                    Ok(r) => Err(format!("HTTP {}", r.status)),
                };
                match found {
                    Err(e) => {
                        for key in keys {
                            self.finish_key(&key, Err(e.clone()), now);
                        }
                    }
                    Ok(found) => {
                        let ids: HashMap<String, String> = found.into_iter().collect();
                        for key in keys {
                            match ids.get(&key.1) {
                                Some(uuid) => {
                                    if let Some(state) = self.keys.get_mut(&key) {
                                        state.stage = Stage::WantSkin { uuid: uuid.clone() };
                                    }
                                }
                                None => self.finish_key(&key, Ok(None), now),
                            }
                        }
                    }
                }
            }
            Req::Skin { key } => {
                let uuid = match self.keys.get(&key).map(|s| &s.stage) {
                    Some(Stage::SkinSent { uuid }) => uuid.clone(),
                    _ => return,
                };
                let outcome = match result {
                    Err(e) => Err(e),
                    Ok(r) if r.status == 429 => {
                        self.note_rate_limit(key.0, &r, now);
                        Err("rate limited".into())
                    }
                    Ok(r) if r.status == 204 || r.status == 404 => Ok(None),
                    Ok(r) if r.status == 200 => {
                        parse_skin(&r.body).map(|s| s.map(|skin| (uuid, skin)))
                    }
                    Ok(r) => Err(format!("HTTP {}", r.status)),
                };
                self.finish_key(&key, outcome, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Method;
    use base64::Engine as _;

    fn ok(body: &str) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn status(code: u16, retry_after: Option<u64>) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: code,
            retry_after,
            body: Vec::new(),
        })
    }

    fn not_found(sources: &[Source]) -> Outcome {
        Outcome::NotFound {
            not_found_in: sources.to_vec(),
        }
    }

    /// A failure without any source answering "not found".
    fn failed(error: &str) -> Outcome {
        Outcome::Failed {
            error: error.into(),
            not_found_in: Vec::new(),
        }
    }

    fn profile_body() -> String {
        let value = base64::engine::general_purpose::STANDARD
            .encode(r#"{"textures":{"SKIN":{"url":"http://textures.minecraft.net/texture/a"}}}"#);
        format!(
            r#"{{"id":"u","name":"n","properties":[{{"name":"textures","value":"{value}","signature":"s"}}]}}"#
        )
    }

    /// Drives one source through the batch and skin step for `name`.
    fn answer_found(e: &mut Engine, now: u64, name: &str) {
        let reqs = e.poll_requests(now);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1.method, Method::Post);
        e.on_response(
            reqs[0].0,
            ok(&format!(
                r#"[{{"id":"069a79f444e94726a5befca90e38aaf5","name":"{name}"}}]"#
            )),
            now,
        );
        let reqs = e.poll_requests(now);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1.method, Method::Get);
        e.on_response(reqs[0].0, ok(&profile_body()), now);
    }

    #[test]
    fn partial_batch_waits_for_the_window() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Mojang], 0).unwrap();
        assert!(e.poll_requests(BATCH_WINDOW_MS - 1).is_empty());
        assert_eq!(e.poll_requests(BATCH_WINDOW_MS).len(), 1);
    }

    #[test]
    fn full_batch_goes_out_at_once() {
        let mut e = Engine::new();
        for i in 0..BATCH_SIZE {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        let reqs = e.poll_requests(0);
        assert_eq!(reqs.len(), 1);
        let names: Vec<String> = serde_json::from_slice(reqs[0].1.body.as_ref().unwrap()).unwrap();
        assert_eq!(names.len(), BATCH_SIZE);
    }

    #[test]
    fn found_skin_finishes_the_query() {
        let mut e = Engine::new();
        let q = e
            .submit("Notch", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        answer_found(&mut e, 1000, "notch");
        let done = e.take_finished();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, q);
        match &done[0].1 {
            Outcome::Found {
                source, name, uuid, ..
            } => {
                assert_eq!(
                    (*source, name.as_str(), uuid.as_str()),
                    (Source::Ely, "notch", "069a79f444e94726a5befca90e38aaf5")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn not_found_moves_to_the_next_source() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        assert_eq!(reqs[0].1.host, "authserver.ely.by");
        e.on_response(reqs[0].0, ok("[]"), 1000);
        assert!(e.take_finished().is_empty());
        answer_found(&mut e, 2000, "notch");
        assert!(matches!(
            e.take_finished()[0].1,
            Outcome::Found {
                source: Source::Mojang,
                ..
            }
        ));
    }

    #[test]
    fn nothing_anywhere_is_not_found() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, status(204, None), 1000);
        assert_eq!(e.take_finished()[0].1, not_found(&[Source::Mojang]));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn same_name_shares_one_lookup() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Mojang], 0).unwrap();
        e.submit("NOTCH", &[Source::Mojang], 0).unwrap();
        assert_eq!(e.queued(), 1);
        answer_found(&mut e, 1000, "notch");
        assert_eq!(e.take_finished().len(), 2);
    }

    #[test]
    fn rate_limit_pauses_the_source_and_falls_back() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, status(429, Some(30)), 1000);
        // Falls through to Mojang.
        answer_found(&mut e, 2000, "notch");
        assert!(matches!(
            e.take_finished()[0].1,
            Outcome::Found {
                source: Source::Mojang,
                ..
            }
        ));
        // Ely.by stays paused for 30 s: a prefixed lookup fails without a request.
        e.submit("Other", &[Source::Ely], 3000).unwrap();
        assert!(e.poll_requests(4000).is_empty());
        assert!(matches!(e.take_finished()[0].1, Outcome::Failed { .. }));
        // After the pause it is used again.
        e.submit("Other", &[Source::Ely], 31_000).unwrap();
        assert_eq!(e.poll_requests(32_000).len(), 1);
    }

    #[test]
    fn errors_are_reported_when_no_source_succeeds() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, Err("timed out".into()), 1000);
        assert_eq!(e.take_finished()[0].1, failed("mojang: timed out"));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn unsigned_skin_counts_as_not_found() {
        let mut e = Engine::new();
        e.submit("Notch", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(
            reqs[0].0,
            ok(r#"[{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}]"#),
            1000,
        );
        let reqs = e.poll_requests(1000);
        e.on_response(
            reqs[0].0,
            ok(r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[]}"#),
            1000,
        );
        assert_eq!(e.take_finished()[0].1, not_found(&[Source::Mojang]));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn queue_is_bounded() {
        let mut e = Engine::new();
        for i in 0..MAX_QUEUED {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        assert_eq!(e.submit("one_more", &[Source::Mojang], 0), Err(Busy));
    }

    #[test]
    fn in_flight_requests_are_bounded() {
        let mut e = Engine::new();
        for i in 0..(BATCH_SIZE * (MAX_IN_FLIGHT + 2)) {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        let mut total = 0;
        for _ in 0..(MAX_IN_FLIGHT + 2) {
            total += e.poll_requests(0).len();
        }
        assert_eq!(total, MAX_IN_FLIGHT);
    }

    /// A distinct, valid uuid per name, so Mojang skin requests can be told apart.
    fn uuid_for(name: &str) -> String {
        let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
        format!("{hex:0>32}")
    }

    /// A bulk lookup answer that knows `names`.
    fn ids(names: &[&str]) -> String {
        let entries: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"id":"{}","name":"{n}"}}"#, uuid_for(n)))
            .collect();
        format!("[{}]", entries.join(","))
    }

    fn skin_req_for(reqs: &[(ReqId, HttpRequest)], name: &str) -> ReqId {
        reqs.iter()
            .find(|(_, r)| r.method == Method::Get && r.path.contains(&uuid_for(name)))
            .expect("skin request exists")
            .0
    }

    #[test]
    fn a_shorter_retry_after_never_shortens_the_pause() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.on_response(batch[0].0, ok(&ids(&["a"])), 1000);
        let skin = e.poll_requests(1000);
        assert_eq!(skin.len(), 1);
        e.submit("b", &[Source::Mojang], 1000).unwrap();
        let batch = e.poll_requests(2000);
        assert_eq!(batch.len(), 1);
        e.on_response(batch[0].0, status(429, Some(60)), 2000);
        e.on_response(skin[0].0, status(429, Some(1)), 2100);
        assert_eq!(e.take_finished().len(), 2);
        e.submit("c", &[Source::Mojang], 4000).unwrap();
        assert!(e.poll_requests(5000).is_empty());
        assert_eq!(e.take_finished()[0].1, failed("mojang is rate limited"));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn huge_retry_after_does_not_overflow() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, status(429, Some(u64::MAX)), 1000);
        assert_eq!(e.take_finished()[0].1, failed("mojang: rate limited"));
        e.submit("b", &[Source::Mojang], u64::MAX - 1).unwrap();
        assert!(matches!(e.take_finished()[0].1, Outcome::Failed { .. }));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn paused_source_still_joins_an_in_flight_key() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        e.submit("b", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.on_response(batch[0].0, ok(&ids(&["a", "b"])), 1000);
        let skins = e.poll_requests(1000);
        assert_eq!(skins.len(), 2);
        e.on_response(skin_req_for(&skins, "b"), status(429, None), 1000);
        assert_eq!(e.take_finished()[0].1, failed("mojang: rate limited"));
        // Mojang is paused, but the lookup for "a" is already running: join it.
        e.submit("A", &[Source::Mojang], 1100).unwrap();
        assert!(e.take_finished().is_empty());
        assert_eq!(e.queued(), 1);
        e.on_response(skin_req_for(&skins, "a"), ok(&profile_body()), 1200);
        let done = e.take_finished();
        assert_eq!(done.len(), 2);
        assert!(done.iter().all(|(_, o)| matches!(o, Outcome::Found { .. })));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn full_queue_still_accepts_a_queued_name() {
        let mut e = Engine::new();
        for i in 0..MAX_QUEUED {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        assert!(e.submit("P0", &[Source::Mojang], 0).is_ok());
        assert_eq!(e.queued(), MAX_QUEUED);
        assert_eq!(e.submit("one_more", &[Source::Mojang], 0), Err(Busy));
        assert!(e.take_finished().is_empty());
    }

    #[test]
    fn full_queue_skips_sources_that_need_a_new_key() {
        let mut e = Engine::new();
        for i in 0..MAX_QUEUED {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        // Ely.by would need a new key, Mojang already has one for this name.
        let q = e.submit("p0", &[Source::Ely, Source::Mojang], 0).unwrap();
        assert_eq!(e.queued(), MAX_QUEUED);
        let reqs = e.poll_requests(0);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1.host, "api.minecraftservices.com");
        e.on_response(reqs[0].0, ok("[]"), 0);
        let done = e.take_finished();
        assert_eq!(done.len(), BATCH_SIZE + 1);
        let busy: Vec<_> = done.iter().filter(|(id, _)| *id == q).collect();
        assert_eq!(busy.len(), 1);
        assert_eq!(
            busy[0].1,
            Outcome::Failed {
                error: "busy".into(),
                not_found_in: vec![Source::Mojang],
            }
        );
        assert_eq!(e.queued(), MAX_QUEUED - BATCH_SIZE);
    }

    #[test]
    fn same_name_joins_while_the_name_lookup_is_in_flight() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.submit("A", &[Source::Mojang], 1000).unwrap();
        assert_eq!(e.queued(), 1);
        assert!(e.poll_requests(3000).is_empty());
        e.on_response(batch[0].0, ok(&ids(&["a"])), 3000);
        let skins = e.poll_requests(3000);
        assert_eq!(skins.len(), 1);
        e.on_response(skins[0].0, ok(&profile_body()), 3000);
        assert_eq!(e.take_finished().len(), 2);
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn same_name_joins_while_waiting_for_the_skin_request() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.on_response(batch[0].0, ok(&ids(&["a"])), 1000);
        e.submit("A", &[Source::Mojang], 1000).unwrap();
        assert_eq!(e.queued(), 1);
        let skins = e.poll_requests(1000);
        assert_eq!(skins.len(), 1);
        e.on_response(skins[0].0, ok(&profile_body()), 1000);
        assert_eq!(e.take_finished().len(), 2);
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn skin_rate_limit_without_retry_after_uses_the_default_pause() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.on_response(batch[0].0, ok(&ids(&["a"])), 1000);
        let skins = e.poll_requests(1000);
        e.on_response(skins[0].0, status(429, None), 1000);
        assert_eq!(e.take_finished()[0].1, failed("mojang: rate limited"));
        assert_eq!(e.queued(), 0);
        let end = 1000 + DEFAULT_BACKOFF_SECS * 1000;
        e.submit("b", &[Source::Mojang], end - 1).unwrap();
        assert_eq!(e.take_finished()[0].1, failed("mojang is rate limited"));
        e.submit("b", &[Source::Mojang], end).unwrap();
        assert!(e.take_finished().is_empty());
        assert_eq!(e.poll_requests(end + BATCH_WINDOW_MS).len(), 1);
    }

    #[test]
    fn pending_batch_on_a_paused_source_fails() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let batch = e.poll_requests(1000);
        e.submit("b", &[Source::Mojang], 1000).unwrap();
        e.on_response(batch[0].0, status(429, Some(30)), 1500);
        assert_eq!(e.take_finished().len(), 1);
        assert!(e.poll_requests(2000).is_empty());
        assert_eq!(e.take_finished()[0].1, failed("mojang: rate limited"));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn key_waiting_for_its_skin_on_a_paused_source_fails() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let first = e.poll_requests(1000);
        e.submit("b", &[Source::Mojang], 1000).unwrap();
        let second = e.poll_requests(2000);
        assert_eq!(second.len(), 1);
        e.on_response(first[0].0, ok(&ids(&["a"])), 2000);
        e.on_response(second[0].0, status(429, Some(30)), 2000);
        assert_eq!(e.take_finished().len(), 1);
        assert!(e.poll_requests(2000).is_empty());
        assert_eq!(e.take_finished()[0].1, failed("mojang: rate limited"));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn more_names_than_a_batch_go_out_in_two_requests() {
        let mut e = Engine::new();
        for i in 0..=BATCH_SIZE {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        let names = |reqs: &[(ReqId, HttpRequest)]| -> Vec<String> {
            serde_json::from_slice(reqs[0].1.body.as_ref().unwrap()).unwrap()
        };
        let first = e.poll_requests(0);
        assert_eq!(first.len(), 1);
        assert_eq!(names(&first).len(), BATCH_SIZE);
        assert!(e.poll_requests(0).is_empty());
        let second = e.poll_requests(BATCH_WINDOW_MS);
        assert_eq!(second.len(), 1);
        assert_eq!(names(&second), vec![format!("p{BATCH_SIZE}")]);
    }

    #[test]
    fn names_missing_from_a_batch_answer_move_on() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Ely, Source::Mojang], 0).unwrap();
        e.submit("b", &[Source::Ely], 0).unwrap();
        let c = e.submit("c", &[Source::Ely], 0).unwrap();
        let batch = e.poll_requests(1000);
        assert_eq!(batch.len(), 1);
        e.on_response(batch[0].0, ok(&ids(&["b"])), 1000);
        // "c" has no other source, "a" moves to Mojang, "b" gets its skin request.
        assert_eq!(e.take_finished(), vec![(c, not_found(&[Source::Ely]))]);
        let reqs = e.poll_requests(2000);
        assert_eq!(reqs.len(), 2);
        let mojang = reqs
            .iter()
            .find(|(_, r)| r.host == "api.minecraftservices.com")
            .expect("name lookup at Mojang");
        let names: Vec<String> = serde_json::from_slice(mojang.1.body.as_ref().unwrap()).unwrap();
        assert_eq!(names, vec!["a".to_string()]);
        assert!(
            reqs.iter()
                .any(|(_, r)| r.host == "skinsystem.ely.by" && r.path.ends_with("/b"))
        );
    }

    #[test]
    fn a_response_frees_an_in_flight_slot() {
        let mut e = Engine::new();
        for i in 0..(BATCH_SIZE * (MAX_IN_FLIGHT + 1)) {
            e.submit(&format!("p{i}"), &[Source::Mojang], 0).unwrap();
        }
        let mut sent = Vec::new();
        for _ in 0..(MAX_IN_FLIGHT + 1) {
            sent.extend(e.poll_requests(0));
        }
        assert_eq!(sent.len(), MAX_IN_FLIGHT);
        e.on_response(sent[0].0, ok("[]"), 0);
        assert_eq!(e.poll_requests(0).len(), 1);
    }

    #[test]
    fn unknown_request_id_is_ignored() {
        let mut e = Engine::new();
        e.on_response(999, ok("[]"), 0);
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0 + 1000, ok("[]"), 1000);
        assert!(e.take_finished().is_empty());
        assert_eq!(e.queued(), 1);
    }

    #[test]
    fn server_error_is_reported() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, status(500, None), 1000);
        assert_eq!(e.take_finished()[0].1, failed("mojang: HTTP 500"));
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn malformed_answers_are_reported() {
        let mut e = Engine::new();
        e.submit("a", &[Source::Mojang], 0).unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, ok("not json"), 1000);
        match &e.take_finished()[0].1 {
            Outcome::Failed { error: msg, .. } => {
                assert!(msg.starts_with("mojang: bad name lookup answer"))
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(e.queued(), 0);

        e.submit("a", &[Source::Mojang], 2000).unwrap();
        let reqs = e.poll_requests(3000);
        e.on_response(reqs[0].0, ok(&ids(&["a"])), 3000);
        let reqs = e.poll_requests(3000);
        e.on_response(reqs[0].0, ok("not json"), 3000);
        match &e.take_finished()[0].1 {
            Outcome::Failed { error: msg, .. } => {
                assert!(msg.starts_with("mojang: bad profile answer"))
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn no_sources_is_not_found_at_once() {
        let mut e = Engine::new();
        let q = e.submit("a", &[], 0).unwrap();
        assert_eq!(e.take_finished(), vec![(q, not_found(&[]))]);
        assert_eq!(e.queued(), 0);
    }

    #[test]
    fn not_found_before_a_hit_is_reported() {
        let mut e = Engine::new();
        e.submit("Alice", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, status(204, None), 1000);
        answer_found(&mut e, 2000, "alice");
        match &e.take_finished()[0].1 {
            Outcome::Found {
                source,
                not_found_in,
                ..
            } => assert_eq!(
                (*source, not_found_in),
                (Source::Mojang, &vec![Source::Ely])
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn transport_error_before_a_hit_is_not_a_not_found() {
        let mut e = Engine::new();
        e.submit("Alice", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, Err("timed out".into()), 1000);
        answer_found(&mut e, 2000, "alice");
        match &e.take_finished()[0].1 {
            Outcome::Found {
                source,
                not_found_in,
                ..
            } => assert_eq!((*source, not_found_in), (Source::Mojang, &vec![])),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rate_limit_and_server_error_are_not_a_not_found() {
        for code in [429, 500] {
            let mut e = Engine::new();
            e.submit("Alice", &[Source::Ely, Source::Mojang], 0)
                .unwrap();
            let reqs = e.poll_requests(1000);
            e.on_response(reqs[0].0, status(code, None), 1000);
            answer_found(&mut e, 2000, "alice");
            match &e.take_finished()[0].1 {
                Outcome::Found { not_found_in, .. } => assert!(not_found_in.is_empty()),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn unsigned_skin_before_a_hit_is_reported() {
        let mut e = Engine::new();
        e.submit("Alice", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, ok(&ids(&["alice"])), 1000);
        let reqs = e.poll_requests(1000);
        e.on_response(
            reqs[0].0,
            ok(r#"{"id":"u","name":"n","properties":[]}"#),
            1000,
        );
        answer_found(&mut e, 2000, "alice");
        match &e.take_finished()[0].1 {
            Outcome::Found { not_found_in, .. } => assert_eq!(not_found_in, &vec![Source::Ely]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn not_found_and_failed_report_the_sources_without_a_skin() {
        let mut e = Engine::new();
        e.submit("Alice", &[Source::Ely, Source::Mojang], 0)
            .unwrap();
        let reqs = e.poll_requests(1000);
        e.on_response(reqs[0].0, ok("[]"), 1000);
        let reqs = e.poll_requests(2000);
        e.on_response(reqs[0].0, status(404, None), 2000);
        assert_eq!(
            e.take_finished()[0].1,
            not_found(&[Source::Ely, Source::Mojang])
        );

        e.submit("Alice", &[Source::Ely, Source::Mojang], 3000)
            .unwrap();
        let reqs = e.poll_requests(4000);
        e.on_response(reqs[0].0, ok("[]"), 4000);
        let reqs = e.poll_requests(5000);
        e.on_response(reqs[0].0, Err("timed out".into()), 5000);
        assert_eq!(
            e.take_finished()[0].1,
            Outcome::Failed {
                error: "mojang: timed out".into(),
                not_found_in: vec![Source::Ely],
            }
        );
    }
}
