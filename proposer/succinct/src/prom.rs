use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use sysinfo::System;

pub struct Metric {
    pub name: &'static str,
    description: &'static str,
}

pub const GAUGES: [Metric; 2] = [CPU_USAGE, MEMORY_USAGE];

pub const CPU_USAGE: Metric = Metric {
    name: "cpu_usage",
    description: "Current CPU usage percentage",
};

pub const MEMORY_USAGE: Metric = Metric {
    name: "memory_usage",
    description: "Current memory usage in bytes",
};

pub fn init_metrics(port: &u16) {
    println!("initializing metrics exporter");

    PrometheusBuilder::new()
        .with_http_listener(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            port.to_owned(),
        ))
        .install()
        .expect("failed to install Prometheus recorder");

    for name in GAUGES {
        register_gauge(name)
    }
}

/******** Utils ********/

/// Registers a counter with the given name.
fn register_counter(metric: Metric) {
    metrics::describe_counter!(metric.name, metric.description);
    let _counter = metrics::counter!(metric.name);
}

/// Registers a gauge with the given name.
fn register_gauge(metric: Metric) {
    metrics::describe_gauge!(metric.name, metric.description);
    let _gauge = ::metrics::gauge!(metric.name);
}

/// Registers a histogram with the given name.
fn register_histogram(metric: Metric) {
    metrics::describe_histogram!(metric.name, metric.description);
    let _histogram = ::metrics::histogram!(metric.name);
}

/// Call this periodically to update CPU and memory usage metrics.
pub fn update_cpu_and_memory() {
    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu = sys.global_cpu_usage();
    let mem = sys.used_memory();

    let cpu_usage = metrics::gauge!("cpu_usage");
    let memory_usage = metrics::gauge!("memory_usage");
    cpu_usage.set(cpu as f64);
    memory_usage.set(mem as f64);
}
