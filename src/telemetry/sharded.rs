//! Hot request metrics use separate producer shards without adding public labels.
//! Scrapes merge the cumulative observations; writers never wait for a scrape.
use prometheus::{
    HistogramOpts, Opts,
    core::{Collector, Desc},
    proto::{Metric, MetricFamily},
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static SHARD: Cell<usize> = Cell::new(NEXT_SHARD.fetch_add(1, Ordering::Relaxed));
}

/// Native runtimes register stable, disjoint writer slots before starting tasks.
/// Threads outside that host retain the automatic assignment used by Pingora.
#[cfg(any(feature = "hyper-experimental", test))]
pub(crate) fn register_worker(slot: usize) {
    SHARD.with(|shard| shard.set(slot));
}

#[cfg(feature = "hyper-experimental")]
pub(crate) fn unregister_worker() -> usize {
    SHARD.with(|shard| shard.replace(0))
}

fn slot(count: usize) -> usize {
    if count == 1 {
        0
    } else {
        SHARD.with(|s| s.get() % count)
    }
}

#[derive(Clone)]
pub struct Counter {
    shards: Arc<[prometheus::IntCounter]>,
}
impl Counter {
    pub fn new(name: &str, help: &str, count: usize) -> prometheus::Result<Self> {
        Ok(Self {
            shards: (0..count.max(1))
                .map(|_| prometheus::IntCounter::new(name, help))
                .collect::<prometheus::Result<Vec<_>>>()?
                .into(),
        })
    }
    pub fn inc(&self) {
        self.inc_by(1);
    }
    pub fn inc_by(&self, value: u64) {
        self.shards[slot(self.shards.len())].inc_by(value);
    }
}

#[derive(Clone)]
pub struct Histogram {
    shards: Arc<[prometheus::Histogram]>,
}
impl Histogram {
    pub fn with_opts(opts: HistogramOpts, count: usize) -> prometheus::Result<Self> {
        Ok(Self {
            shards: (0..count.max(1))
                .map(|_| prometheus::Histogram::with_opts(opts.clone()))
                .collect::<prometheus::Result<Vec<_>>>()?
                .into(),
        })
    }
    pub fn observe(&self, value: f64) {
        self.shards[slot(self.shards.len())].observe(value);
    }
}

#[derive(Clone)]
pub struct CounterVec {
    shards: Arc<[prometheus::IntCounterVec]>,
}
impl CounterVec {
    pub fn new(opts: Opts, labels: &[&str], count: usize) -> prometheus::Result<Self> {
        Ok(Self {
            shards: (0..count.max(1))
                .map(|_| prometheus::IntCounterVec::new(opts.clone(), labels))
                .collect::<prometheus::Result<Vec<_>>>()?
                .into(),
        })
    }
    pub fn with_label_values(&self, labels: &[&str]) -> Counter {
        Counter {
            shards: self
                .shards
                .iter()
                .map(|s| s.with_label_values(labels))
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

#[derive(Clone)]
pub struct HistogramVec {
    shards: Arc<[prometheus::HistogramVec]>,
}
impl HistogramVec {
    pub fn new(opts: HistogramOpts, labels: &[&str], count: usize) -> prometheus::Result<Self> {
        Ok(Self {
            shards: (0..count.max(1))
                .map(|_| prometheus::HistogramVec::new(opts.clone(), labels))
                .collect::<prometheus::Result<Vec<_>>>()?
                .into(),
        })
    }
    pub fn with_label_values(&self, labels: &[&str]) -> Histogram {
        Histogram {
            shards: self
                .shards
                .iter()
                .map(|s| s.with_label_values(labels))
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

fn merge(families: impl Iterator<Item = MetricFamily>) -> Vec<MetricFamily> {
    type Labels = Vec<(String, String)>;
    let mut merged: BTreeMap<String, (MetricFamily, BTreeMap<Labels, Metric>)> = BTreeMap::new();
    for mut family in families {
        let metrics = family.take_metric();
        let (_, output) = merged
            .entry(family.name().to_owned())
            .or_insert_with(|| (family, BTreeMap::new()));
        for metric in metrics {
            let key = metric
                .get_label()
                .iter()
                .map(|l| (l.name().to_owned(), l.value().to_owned()))
                .collect();
            match output.entry(key) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(metric);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let total = e.get_mut();
                    if let Some(value) = metric.counter.as_ref() {
                        let value = total.counter.as_ref().unwrap().value() + value.value();
                        total.counter.as_mut().unwrap().set_value(value);
                    } else {
                        let value = metric.histogram.as_ref().unwrap();
                        let total = total.histogram.as_mut().unwrap();
                        total.set_sample_count(
                            total
                                .get_sample_count()
                                .wrapping_add(value.get_sample_count()),
                        );
                        total.set_sample_sum(total.get_sample_sum() + value.get_sample_sum());
                        for (a, b) in total.bucket.iter_mut().zip(value.get_bucket()) {
                            a.set_cumulative_count(
                                a.cumulative_count().wrapping_add(b.cumulative_count()),
                            );
                        }
                    }
                }
            }
        }
    }
    merged
        .into_values()
        .map(|(mut family, metrics)| {
            family.set_metric(metrics.into_values().collect());
            family
        })
        .collect()
}

macro_rules! collector {
    ($kind:ty) => {
        impl Collector for $kind {
            fn desc(&self) -> Vec<&Desc> {
                self.shards[0].desc()
            }
            fn collect(&self) -> Vec<MetricFamily> {
                merge(self.shards.iter().flat_map(Collector::collect))
            }
        }
    };
}
collector!(Counter);
collector!(Histogram);
collector!(CounterVec);
collector!(HistogramVec);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_producers_export_one_complete_labelled_distribution() {
        let count =
            CounterVec::new(Opts::new("requests_total", "requests"), &["status"], 9).unwrap();
        let duration = HistogramVec::new(
            HistogramOpts::new("duration_seconds", "duration").buckets(vec![0.01, 0.1]),
            &["route"],
            9,
        )
        .unwrap();
        std::thread::scope(|scope| {
            for slot in 0..9 {
                let count = count.with_label_values(&["200"]);
                let duration = duration.with_label_values(&["root"]);
                scope.spawn(move || {
                    register_worker(slot);
                    for _ in 0..1000 {
                        count.inc();
                        duration.observe(0.05);
                    }
                });
            }
            for _ in 0..20 {
                let _ = duration.collect();
            }
        });
        std::thread::scope(|scope| {
            scope.spawn(|| {
                register_worker(7);
                count.with_label_values(&["200"]).inc_by(100);
                register_worker(3);
                count.with_label_values(&["200"]).inc_by(10);
            });
        });
        for (slot, shard) in count.shards.iter().enumerate() {
            let extra = match slot {
                7 => 100,
                3 => 10,
                _ => 0,
            };
            assert_eq!(shard.with_label_values(&["200"]).get(), 1000 + extra);
        }
        for shard in duration.shards.iter() {
            assert_eq!(shard.with_label_values(&["root"]).get_sample_count(), 1000);
        }
        let c = count.collect();
        let h = duration.collect();
        assert_eq!(c[0].get_metric().len(), 1);
        assert_eq!(
            c[0].get_metric()[0].counter.as_ref().unwrap().value(),
            9110.0
        );
        let m = &h[0].get_metric()[0];
        assert_eq!(m.get_label().len(), 1);
        assert_eq!(m.get_label()[0].name(), "route");
        assert_eq!(m.get_label()[0].value(), "root");
        let h = m.histogram.as_ref().unwrap();
        assert_eq!(h.get_sample_count(), 9000);
        assert!((h.get_sample_sum() - 450.0).abs() < 1e-8);
        assert_eq!(h.get_bucket()[0].cumulative_count(), 0);
        assert_eq!(h.get_bucket()[1].cumulative_count(), 9000);
    }
}
