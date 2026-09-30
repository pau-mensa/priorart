//! In-process metrics in the Prometheus text format. The public set holds only
//! server-wide search figures; the operator set adds per-route and
//! per-collection detail, which reveals what collections exist and their size.
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const BUCKETS: [f64; 14] = [
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const QUANTILES: [f64; 3] = [0.5, 0.95, 0.99];
const WINDOW: Duration = Duration::from_secs(300);
const WINDOW_SAMPLES: usize = 10_000;

#[derive(Default)]
struct Histogram {
    buckets: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, seconds: f64) {
        for (bucket, bound) in self.buckets.iter_mut().zip(BUCKETS) {
            if seconds <= bound {
                *bucket += 1;
            }
        }
        self.sum += seconds;
        self.count += 1;
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let prefix = if labels.is_empty() {
            String::new()
        } else {
            format!("{labels},")
        };
        for (count, bound) in self.buckets.iter().zip(BUCKETS) {
            let _ = writeln!(out, "{name}_bucket{{{prefix}le=\"{bound}\"}} {count}");
        }
        let _ = writeln!(out, "{name}_bucket{{{prefix}le=\"+Inf\"}} {}", self.count);
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{labels} {}", self.sum);
        let _ = writeln!(out, "{name}_count{labels} {}", self.count);
    }
}

#[derive(Default)]
struct Inner {
    searches: Histogram,
    recent: VecDeque<(Instant, f64)>,
    search_outcomes: BTreeMap<&'static str, u64>,
    requests: BTreeMap<(String, &'static str, u16), u64>,
    request_durations: BTreeMap<String, Histogram>,
    collection_searches: BTreeMap<String, u64>,
    ranking: Histogram,
    index_discards: u64,
    collection_writes: BTreeMap<(String, &'static str), u64>,
    indexed_documents: BTreeMap<String, usize>,
    index_loads: Histogram,
    evictions: u64,
    resource_limited: u64,
    rate_limited: u64,
}

impl Inner {
    fn prune(&mut self, now: Instant) {
        while self.recent.len() > WINDOW_SAMPLES
            || self
                .recent
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > WINDOW)
        {
            self.recent.pop_front();
        }
    }
}

#[derive(Default)]
pub struct Metrics(Mutex<Inner>);

impl Metrics {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One HTTP exchange. `route` is the matched path template, so labels stay
    /// bounded; streamed bodies are timed until their headers.
    pub fn request(
        &self,
        route: Option<&str>,
        method: &Method,
        status: StatusCode,
        elapsed: Duration,
    ) {
        let seconds = elapsed.as_secs_f64();
        let route = route.unwrap_or("unmatched");
        let method = match *method {
            Method::GET => "GET",
            Method::HEAD => "HEAD",
            Method::POST => "POST",
            Method::PUT => "PUT",
            Method::DELETE => "DELETE",
            _ => "other",
        };
        let mut inner = self.inner();
        *inner
            .requests
            .entry((route.to_owned(), method, status.as_u16()))
            .or_default() += 1;
        inner
            .request_durations
            .entry(route.to_owned())
            .or_default()
            .observe(seconds);
        if route == "/v1/search" && method == "POST" {
            let outcome = if status.is_success() {
                "ok"
            } else if status.is_client_error() {
                "client_error"
            } else {
                "server_error"
            };
            *inner.search_outcomes.entry(outcome).or_default() += 1;
            if status.is_success() {
                inner.searches.observe(seconds);
                let now = Instant::now();
                inner.recent.push_back((now, seconds));
                inner.prune(now);
            }
        }
    }

    /// One ranking pass over `collections`.
    pub fn ranking(&self, collections: &[&str], elapsed: Duration) {
        let mut inner = self.inner();
        inner.ranking.observe(elapsed.as_secs_f64());
        for collection in collections {
            *inner
                .collection_searches
                .entry((*collection).to_owned())
                .or_default() += 1;
        }
    }

    /// A recipe failed to apply a write, so its index was dropped.
    pub fn index_discarded(&self, collection: &str) {
        let mut inner = self.inner();
        inner.index_discards += 1;
        inner.indexed_documents.remove(collection);
    }

    /// A committed write, with the collection's index size if it is loaded.
    pub fn write(&self, collection: &str, operation: &'static str, indexed: Option<usize>) {
        let mut inner = self.inner();
        *inner
            .collection_writes
            .entry((collection.to_owned(), operation))
            .or_default() += 1;
        if let Some(documents) = indexed {
            inner
                .indexed_documents
                .insert(collection.to_owned(), documents);
        }
    }

    pub fn index_loaded(&self, collection: &str, elapsed: Duration, documents: usize) {
        let mut inner = self.inner();
        inner.index_loads.observe(elapsed.as_secs_f64());
        inner
            .indexed_documents
            .insert(collection.to_owned(), documents);
    }

    pub fn evicted(&self, collection: &str) {
        let mut inner = self.inner();
        inner.evictions += 1;
        inner.indexed_documents.remove(collection);
    }

    pub fn resource_limited(&self) {
        self.inner().resource_limited += 1;
    }

    pub fn rate_limited(&self) {
        self.inner().rate_limited += 1;
    }

    /// Drops every series labelled with a deleted collection.
    pub fn forget(&self, collection: &str) {
        let mut inner = self.inner();
        inner.collection_searches.remove(collection);
        inner
            .collection_writes
            .retain(|(id, _), _| id != collection);
        inner.indexed_documents.remove(collection);
    }

    pub fn render_public(&self) -> String {
        let mut inner = self.inner();
        let mut out = String::new();
        render_public(&mut inner, &mut out);
        out
    }

    pub fn render_admin(
        &self,
        cached_collections: usize,
        search_log_dropped: Option<[(&str, u64); 3]>,
    ) -> String {
        let mut inner = self.inner();
        let mut out = String::new();
        render_public(&mut inner, &mut out);
        header(
            &mut out,
            "priorart_http_requests_total",
            "counter",
            "HTTP requests by route, method, and status.",
        );
        for ((route, method, status), count) in &inner.requests {
            let _ = writeln!(
                out,
                "priorart_http_requests_total{{route=\"{}\",method=\"{method}\",status=\"{status}\"}} {count}",
                escape(route)
            );
        }
        header(
            &mut out,
            "priorart_http_request_duration_seconds",
            "histogram",
            "HTTP request latency by route.",
        );
        for (route, histogram) in &inner.request_durations {
            histogram.render(
                &mut out,
                "priorart_http_request_duration_seconds",
                &format!("route=\"{}\"", escape(route)),
            );
        }
        header(
            &mut out,
            "priorart_search_ranking_duration_seconds",
            "histogram",
            "Time the retrieval recipe spent ranking a search.",
        );
        inner
            .ranking
            .render(&mut out, "priorart_search_ranking_duration_seconds", "");
        header(
            &mut out,
            "priorart_collection_searches_total",
            "counter",
            "Ranked searches that included each collection.",
        );
        for (collection, count) in &inner.collection_searches {
            let _ = writeln!(
                out,
                "priorart_collection_searches_total{{collection=\"{}\"}} {count}",
                escape(collection)
            );
        }
        header(
            &mut out,
            "priorart_collection_writes_total",
            "counter",
            "Committed writes by collection and operation.",
        );
        for ((collection, operation), count) in &inner.collection_writes {
            let _ = writeln!(
                out,
                "priorart_collection_writes_total{{collection=\"{}\",operation=\"{operation}\"}} {count}",
                escape(collection)
            );
        }
        header(
            &mut out,
            "priorart_indexed_documents",
            "gauge",
            "Live records in each loaded index.",
        );
        for (collection, documents) in &inner.indexed_documents {
            let _ = writeln!(
                out,
                "priorart_indexed_documents{{collection=\"{}\"}} {documents}",
                escape(collection)
            );
        }
        header(
            &mut out,
            "priorart_index_load_duration_seconds",
            "histogram",
            "Time to build an index from SQLite.",
        );
        inner
            .index_loads
            .render(&mut out, "priorart_index_load_duration_seconds", "");
        for (name, kind, help, value) in [
            (
                "priorart_cached_collections",
                "gauge",
                "Collections currently cached.",
                cached_collections as u64,
            ),
            (
                "priorart_index_evictions_total",
                "counter",
                "Cached collections evicted to admit another.",
                inner.evictions,
            ),
            (
                "priorart_index_update_failures_total",
                "counter",
                "Writes the recipe could not apply, discarding the collection's index.",
                inner.index_discards,
            ),
            (
                "priorart_resource_limited_total",
                "counter",
                "Requests refused because every cached collection was busy.",
                inner.resource_limited,
            ),
            (
                "priorart_rate_limited_total",
                "counter",
                "Requests refused by a key's per-minute limit.",
                inner.rate_limited,
            ),
        ] {
            header(&mut out, name, kind, help);
            let _ = writeln!(out, "{name} {value}");
        }
        if let Some(dropped) = search_log_dropped {
            header(
                &mut out,
                "priorart_search_log_dropped_total",
                "counter",
                "Search log entries lost, by reason.",
            );
            for (reason, count) in dropped {
                let _ = writeln!(
                    out,
                    "priorart_search_log_dropped_total{{reason=\"{reason}\"}} {count}"
                );
            }
        }
        out
    }
}

fn render_public(inner: &mut Inner, out: &mut String) {
    header(
        out,
        "priorart_search_duration_seconds",
        "histogram",
        "Latency of successful searches.",
    );
    inner
        .searches
        .render(out, "priorart_search_duration_seconds", "");
    header(
        out,
        "priorart_search_duration_recent_seconds",
        "gauge",
        "Successful search latency quantiles over the last five minutes; NaN when idle.",
    );
    inner.prune(Instant::now());
    let mut recent: Vec<f64> = inner.recent.iter().map(|(_, seconds)| *seconds).collect();
    recent.sort_unstable_by(f64::total_cmp);
    for quantile in QUANTILES {
        let value = match recent.len() {
            0 => f64::NAN,
            n => recent[((quantile * n as f64).ceil() as usize).clamp(1, n) - 1],
        };
        let _ = writeln!(
            out,
            "priorart_search_duration_recent_seconds{{quantile=\"{quantile}\"}} {value}"
        );
    }
    header(
        out,
        "priorart_searches_total",
        "counter",
        "Searches by outcome.",
    );
    for outcome in ["ok", "client_error", "server_error"] {
        let count = inner.search_outcomes.get(outcome).copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "priorart_searches_total{{outcome=\"{outcome}\"}} {count}"
        );
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
