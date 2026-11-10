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
    },
    NotFound,
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

type Key = (Source, String);

struct Query {
    name: String,
    remaining: VecDeque<Source>,
    errors: Vec<String>,
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

    /// Starts looking up `name` in `sources`, in order, until one has a skin.
    pub fn submit(&mut self, name: &str, sources: &[Source], now: u64) -> Result<QueryId, Busy> {
        if self.keys.len() >= MAX_QUEUED {
            return Err(Busy);
        }
        let id = self.id();
        self.queries.insert(
            id,
            Query {
                name: name.to_ascii_lowercase(),
                remaining: sources.iter().copied().collect(),
                errors: Vec::new(),
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
                let outcome = if query.errors.is_empty() {
                    Outcome::NotFound
                } else {
                    Outcome::Failed(query.errors.join("; "))
                };
                self.finished.push((id, outcome));
                return;
            };
            if self.backoff_until.get(&source).is_some_and(|&t| now < t) {
                query
                    .errors
                    .push(format!("{} is rate limited", source.as_str()));
                continue;
            }
            let key = (source, query.name.clone());
            match self.keys.get_mut(&key) {
                Some(state) => state.waiters.push(id),
                None => {
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
                }
            }
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
                    self.queries.remove(&id);
                    self.finished.push((
                        id,
                        Outcome::Found {
                            source: key.0,
                            name: key.1.clone(),
                            uuid: uuid.clone(),
                            skin: skin.clone(),
                        },
                    ));
                }
                Ok(None) => self.advance(id, now),
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
        self.backoff_until.insert(source, now + secs * 1000);
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
        assert_eq!(e.take_finished()[0].1, Outcome::NotFound);
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
        assert!(matches!(e.take_finished()[0].1, Outcome::Failed(_)));
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
        assert_eq!(
            e.take_finished()[0].1,
            Outcome::Failed("mojang: timed out".into())
        );
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
        assert_eq!(e.take_finished()[0].1, Outcome::NotFound);
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
}
