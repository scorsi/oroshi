//! O0 metrics from ADR 0011. Label values are closed enums, never request strings.
use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
const BOUNDS_MS: [u64; 6] = [10, 50, 250, 1000, 5000, 30000];
const BOUNDS_TEXT: [&str; 7] = ["0.01", "0.05", "0.25", "1", "5", "30", "+Inf"];

#[derive(Clone, Copy)]
pub enum Module {
    Nix,
    Go,
    Cargo,
    Hex,
    Npm,
    Other,
}
const MODULES: [Module; 6] = [
    Module::Nix,
    Module::Go,
    Module::Cargo,
    Module::Hex,
    Module::Npm,
    Module::Other,
];
impl Module {
    pub(crate) fn label(self) -> &'static str {
        ["nix", "go", "cargo", "hex", "npm", "other"][self as usize]
    }
}

#[derive(Clone, Copy)]
pub enum Method {
    Get,
    Head,
    Put,
    Other,
}
const METHODS: [Method; 4] = [Method::Get, Method::Head, Method::Put, Method::Other];
impl From<&str> for Method {
    fn from(method: &str) -> Self {
        match method {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "PUT" => Self::Put,
            _ => Self::Other,
        }
    }
}
impl Method {
    pub(crate) fn label(self) -> &'static str {
        ["GET", "HEAD", "PUT", "other"][self as usize]
    }
}

#[derive(Clone, Copy)]
pub enum Code {
    Informational,
    Success,
    Redirection,
    ClientError,
    ServerError,
}
const CODES: [Code; 5] = [
    Code::Informational,
    Code::Success,
    Code::Redirection,
    Code::ClientError,
    Code::ServerError,
];
impl TryFrom<u16> for Code {
    type Error = &'static str;

    fn try_from(code: u16) -> Result<Self, Self::Error> {
        match code {
            100..=199 => Ok(Self::Informational),
            200..=299 => Ok(Self::Success),
            300..=399 => Ok(Self::Redirection),
            400..=499 => Ok(Self::ClientError),
            500..=599 => Ok(Self::ServerError),
            _ => Err("HTTP status must be between 100 and 599"),
        }
    }
}
impl Code {
    fn label(self) -> &'static str {
        ["1xx", "2xx", "3xx", "4xx", "5xx"][self as usize]
    }
}

#[derive(Default)]
pub struct Counter(AtomicU64);
impl Counter {
    pub fn add(&self, value: u64) {
        // Saturate rather than reset a counter on integer overflow.
        self.0
            .fetch_update(Relaxed, Relaxed, |old| Some(old.saturating_add(value)))
            .unwrap();
    }

    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

#[derive(Default)]
pub struct Gauge(AtomicI64);
impl Gauge {
    pub fn set(&self, value: i64) {
        self.0.store(value, Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Relaxed)
    }
}

#[derive(Default)]
pub struct Histogram {
    // Disjoint bins let a scrape derive monotonic buckets and count from one snapshot.
    bins: [Counter; 7],
    sum: AtomicU64,
}
impl Histogram {
    pub fn observe(&self, duration: Duration) {
        let bin = BOUNDS_MS
            .iter()
            .position(|&ms| duration <= Duration::from_millis(ms))
            .unwrap_or(6);
        let seconds = duration.as_secs_f64();
        self.sum
            .fetch_update(Relaxed, Relaxed, |bits| {
                Some((f64::from_bits(bits) + seconds).to_bits())
            })
            .unwrap();
        self.bins[bin].add(1);
    }
}

pub struct Registry {
    build_info: Gauge,
    requests: [[[Counter; 5]; 4]; 6],
    durations: [Histogram; 6],
    response_bytes: [Counter; 6],
}
impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
impl Registry {
    pub fn new() -> Self {
        Self {
            build_info: Gauge(AtomicI64::new(1)),
            requests: Default::default(),
            durations: Default::default(),
            response_bytes: Default::default(),
        }
    }

    pub fn requests(&self, module: Module, method: Method, code: Code) -> &Counter {
        &self.requests[module as usize][method as usize][code as usize]
    }

    pub fn duration(&self, module: Module) -> &Histogram {
        &self.durations[module as usize]
    }

    pub fn response_bytes(&self, module: Module) -> &Counter {
        &self.response_bytes[module as usize]
    }

    pub fn exposition(&self) -> String {
        self.exposition_with_version(env!("CARGO_PKG_VERSION"))
    }

    fn exposition_with_version(&self, version: &str) -> String {
        let mut out = String::new();
        let version = version
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        out.push_str(
            "# HELP oroshi_build_info Running build version.\n# TYPE oroshi_build_info gauge\n",
        );
        writeln!(
            out,
            "oroshi_build_info{{version=\"{version}\"}} {}",
            self.build_info.get()
        )
        .unwrap();
        out.push_str(
            "# HELP oroshi_requests_total HTTP requests.\n# TYPE oroshi_requests_total counter\n",
        );
        for module in MODULES {
            for method in METHODS {
                for code in CODES {
                    writeln!(
                        out,
                        "oroshi_requests_total{{module=\"{}\",method=\"{}\",code=\"{}\"}} {}",
                        module.label(),
                        method.label(),
                        code.label(),
                        self.requests(module, method, code).get()
                    )
                    .unwrap();
                }
            }
        }
        out.push_str("# HELP oroshi_request_duration_seconds Time to answer an HTTP request.\n# TYPE oroshi_request_duration_seconds histogram\n");
        for module in MODULES {
            let histogram = self.duration(module);
            let mut count = 0u64;
            for (bin, bound) in histogram.bins.iter().zip(BOUNDS_TEXT) {
                count = count.saturating_add(bin.get());
                writeln!(out, "oroshi_request_duration_seconds_bucket{{module=\"{}\",le=\"{bound}\"}} {count}", module.label()).unwrap();
            }
            writeln!(
                out,
                "oroshi_request_duration_seconds_sum{{module=\"{}\"}} {}",
                module.label(),
                f64::from_bits(histogram.sum.load(Relaxed))
            )
            .unwrap();
            writeln!(
                out,
                "oroshi_request_duration_seconds_count{{module=\"{}\"}} {count}",
                module.label()
            )
            .unwrap();
        }
        out.push_str("# HELP oroshi_response_bytes_total HTTP response bytes.\n# TYPE oroshi_response_bytes_total counter\n");
        for module in MODULES {
            writeln!(
                out,
                "oroshi_response_bytes_total{{module=\"{}\"}} {}",
                module.label(),
                self.response_bytes(module).get()
            )
            .unwrap();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_exposition() {
        let registry = Registry::new();
        registry
            .requests(Module::Other, Method::Get, Code::Success)
            .add(3);
        registry.response_bytes(Module::Other).add(42);
        registry
            .duration(Module::Other)
            .observe(Duration::from_millis(250));
        registry
            .duration(Module::Other)
            .observe(Duration::from_secs(31));
        assert_eq!(
            registry.exposition_with_version("test"),
            include_str!("../tests/fixtures/metrics.prom")
        );
        assert_eq!(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8");
        assert!(
            registry
                .exposition()
                .contains(&format!("version=\"{}\"", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn series_budget_includes_full_cartesian_product() {
        let output = Registry::new().exposition();
        let samples = output.lines().filter(|line| !line.starts_with('#')).count();
        let requests = MODULES.len() * METHODS.len() * CODES.len();
        let durations = MODULES.len() * (BOUNDS_MS.len() + 3);
        assert_eq!(requests, 120);
        assert_eq!(durations, 54);
        assert_eq!(samples, 1 + requests + durations + MODULES.len());
        // ADR 0011 reserves later upload bytes, uploads, auth, storage and GC families.
        let later = MODULES.len() + MODULES.len() * 2 + MODULES.len() + 2 + 2 + 2;
        assert_eq!(samples + later, 211);
        assert!(samples + later <= 500);
    }

    #[test]
    fn labels_collapse_methods_and_refuse_invalid_statuses() {
        for (input, label) in [
            ("GET", "GET"),
            ("HEAD", "HEAD"),
            ("PUT", "PUT"),
            ("POST", "other"),
            ("get", "other"),
            ("/secret", "other"),
        ] {
            assert_eq!(Method::from(input).label(), label);
        }
        for status in 100..600 {
            assert_eq!(
                Code::try_from(status).unwrap().label(),
                format!("{}xx", status / 100)
            );
        }
        for status in [0, 99, 600, u16::MAX] {
            assert!(Code::try_from(status).is_err());
        }
    }

    #[test]
    fn histogram_boundaries_and_empty_bins() {
        let registry = Registry::new();
        for ms in BOUNDS_MS {
            registry
                .duration(Module::Nix)
                .observe(Duration::from_millis(ms));
            registry
                .duration(Module::Nix)
                .observe(Duration::from_millis(ms) + Duration::from_nanos(1));
        }
        registry.duration(Module::Nix).observe(Duration::ZERO);
        let histogram = registry.duration(Module::Nix);
        assert_eq!(
            histogram.bins.iter().map(Counter::get).collect::<Vec<_>>(),
            [2, 2, 2, 2, 2, 2, 1]
        );
        assert!(
            registry
                .exposition()
                .contains("oroshi_request_duration_seconds_count{module=\"nix\"} 13\n")
        );
        assert!(
            registry
                .exposition()
                .contains("oroshi_request_duration_seconds_count{module=\"other\"} 0\n")
        );
    }

    #[test]
    fn counters_saturate_and_gauges_can_decrease() {
        let counter = Counter::default();
        counter.add(u64::MAX);
        counter.add(1);
        assert_eq!(counter.get(), u64::MAX);
        let gauge = Gauge::default();
        gauge.set(42);
        gauge.set(-2);
        assert_eq!(gauge.get(), -2);
    }

    #[test]
    fn version_labels_are_escaped() {
        assert!(
            Registry::new()
                .exposition_with_version("a\\b\"c\nd")
                .contains("version=\"a\\\\b\\\"c\\nd\"")
        );
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let registry = Registry::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        registry
                            .requests(Module::Other, Method::Get, Code::Success)
                            .add(1);
                        registry
                            .duration(Module::Other)
                            .observe(Duration::from_millis(250));
                    }
                });
            }
        });
        assert_eq!(
            registry
                .requests(Module::Other, Method::Get, Code::Success)
                .get(),
            4000
        );
        assert_eq!(registry.duration(Module::Other).bins[2].get(), 4000);
        assert_eq!(
            f64::from_bits(registry.duration(Module::Other).sum.load(Relaxed)),
            1000.0
        );
    }
}
