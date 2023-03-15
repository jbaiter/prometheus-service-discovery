extern crate backoff;
#[macro_use]
extern crate clap;
extern crate custom_error;
extern crate redis;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate log;

use backoff::ExponentialBackoffBuilder;
use clap::{App, AppSettings, Arg, ArgMatches};
use custom_error::custom_error;
use env_logger::Env;
use redis::{Commands, RedisError, RedisResult};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::num::ParseIntError;
use std::path::Path;
use std::process;
use std::time::Duration;

/// Redis key under which the set of all registered service keys resides.
static SERVICE_KEY: &str = "prometheus_sd_service_keys";
static DISCOVER_HELP: &str = "Discover services in the environment.

This is a long-running process that will continously monitor Redis for the
registration of new services and, upon any modifications to the service
registry, write the services as JSON to an output path where it can be picked
up by Prometheus' file-based discovery process.
";

// Custom error and result types that wrap various errors that can arise during
// service registration and monitoring
custom_error! {CliError
    RedisError{source: RedisError}       = "Problem connecting to Redis: {source}",
    ExportError{source: io::Error}       = "Problem exporting service file: {source}",
    JsonError{source: serde_json::Error} = "Prolem exporting service JSON: {source}",
    InvalidPort{source: ParseIntError}   = "Invalid port number: {source}",
    NoSuchService{service: String}       = "No such service registered: '{service}'",
    NoSuchHost{service: String, host: String} = "No host starting with '{host}' registered for {service}",
}
type Result<T> = std::result::Result<T, CliError>;

/// Instance of a service on a single host, used for registering new instances.
#[derive(Debug)]
struct ServiceInstance {
    service_name: String,
    job_name: String,
    labels: Vec<(String, String)>,
    metrics_path: String,
    host: String,
    port: u16,
}

/// Service definition, used for telling Prometheus about the discovered service
#[derive(Serialize, Debug)]
struct RegisteredService {
    labels: HashMap<String, String>,
    targets: HashSet<String>,
}

/// Register a new service instance
fn register_instance(con: &mut redis::Connection, inst: &ServiceInstance) -> Result<()> {
    let labels_key = format!("prometheus_sd:{}:labels", inst.service_name);
    let targets_key = format!("prometheus_sd:{}:targets", inst.service_name);
    let mut pipe = redis::pipe();
    pipe.atomic()
        .sadd(SERVICE_KEY, &inst.service_name)
        .hset(&labels_key, "job", &inst.job_name);
    if !inst.labels.is_empty() {
        pipe.hset_multiple(&labels_key, &inst.labels);
    }
    pipe.sadd(
        targets_key,
        format!("{}:{}{}", inst.host, inst.port, inst.metrics_path),
    )
    .query(con)?;
    Ok(())
}

/// Unregister a service instance, either wholesale or for a single target only
fn unregister_instance(
    con: &mut redis::Connection,
    service_name: &str,
    host: Option<&str>,
) -> Result<()> {
    let labels_key = format!("prometheus_sd:{}:labels", service_name);
    let targets_key = format!("prometheus_sd:{}:targets", service_name);

    let service_is_registered: bool = con.sismember(SERVICE_KEY, service_name).unwrap_or(false);
    if !service_is_registered {
        return Err(CliError::NoSuchService {
            service: service_name.to_string(),
        });
    }

    let mut pipe = redis::pipe();
    pipe.atomic();

    let service_targets: HashSet<String> = con.smembers(&targets_key)?;
    match host {
        Some(host) => {
            // Only remove the target for a single host
            let found = service_targets
                .iter()
                .find(|t| t.starts_with(&host))
                .map(|t| {
                    pipe.srem(&targets_key, &t);
                })
                .is_some();
            if !found {
                return Err(CliError::NoSuchHost {
                    service: service_name.to_string(),
                    host: host.to_string(),
                });
            }
            if service_targets.len() == 1 && found {
                pipe.del(&targets_key);
                pipe.del(&labels_key);
                pipe.srem(SERVICE_KEY, &service_name);
            }
        }
        None => {
            // Remove everything relating to the service, i.e. all labels and all targets
            pipe.del(&targets_key);
            pipe.del(&labels_key);
            pipe.srem(&SERVICE_KEY, &service_name);
        }
    }
    pipe.query(con)?;
    Ok(())
}

/// Discover all services with their hosts and labels in the registry.
fn discover_services(con: &mut redis::Connection) -> Result<Vec<RegisteredService>> {
    let mut service_keys: Vec<String> = con.smembers(SERVICE_KEY)?;
    service_keys.sort();
    service_keys
        .iter()
        .map(|key| {
            Ok(RegisteredService {
                labels: con.hgetall(format!("prometheus_sd:{}:labels", key))?,
                targets: con.smembers(format!("prometheus_sd:{}:targets", key))?,
            })
        })
        .collect()
}

/// Monitor Redis for changes to the service registry and dump the new service definition on a change.
fn monitor_registry(
    con: &mut redis::Connection,
    client: &redis::Client,
    out_path: &Path,
) -> Result<()> {
    // We need a dedicated connection for the pubsub, since the pubsub struct
    // has a mutable reference to the connection as a member and we need to run
    // commands on the connection as well. No retries for this connection, we
    // assume that this will succeed, given that we already have established
    // a live connection.
    let mut pubsub_con = client.get_connection()?;
    // Enable keyspace event notifications
    redis::cmd("CONFIG")
        .arg("SET")
        .arg("notify-keyspace-events")
        .arg("Ksh") // Only get notified for keyspace events on set and hash keys
        .query(&mut pubsub_con)?;
    let mut pubsub = pubsub_con.as_pubsub();
    pubsub.psubscribe("__keyspace@0__:prometheus_sd*")?;
    loop {
        let _ = pubsub.get_message()?;
        let services = discover_services(con)?;
        serde_json::to_writer_pretty(
            io::BufWriter::with_capacity(256 * 1024, File::create(out_path)?),
            &services,
        )?;
    }
}

/// Try connecting to Redis and retry until a given maximum timeout is reached.
fn try_redis_connect(
    redis_client: &redis::Client,
    max_timeout_sec: u64,
) -> RedisResult<redis::Connection> {
    // Will wait between 500ms and 15min, with a maximum backoff time of 1min, up to 8 hours
    let backoff = ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(500))
        .with_max_interval(Duration::from_secs(15 * 60))
        .with_multiplier(1.5)
        .with_max_elapsed_time(Some(Duration::from_secs(max_timeout_sec)))
        .build();
    let res = backoff::retry_notify(
        backoff,
        || {
            let mut con = redis_client.get_connection_with_timeout(Duration::from_secs(5 * 60))?;
            if redis::cmd("PING").query::<String>(&mut con)? != "PONG" {
                Err(backoff::Error::transient(RedisError::from((
                    redis::ErrorKind::ResponseError,
                    "Ping failed",
                ))))
            } else {
                Ok(con)
            }
        },
        |err, duration| {
            warn!(
                "Failed to connect to Redis: '{:?}' retrying in {:.0?}",
                err, duration
            );
        },
    );
    match res {
        Ok(con) => Ok(con),
        Err(backoff::Error::Permanent(e)) => Err(e),
        Err(backoff::Error::Transient {
            err,
            retry_after: _,
        }) => Err(err),
    }
}

fn run_app(matches: &ArgMatches) -> Result<()> {
    let redis_url = matches
        .value_of("redis-url")
        .unwrap_or("redis://localhost:6379");
    let max_timeout_sec: u64 = matches.value_of("max-timeout").unwrap_or("28800").parse()?;
    info!("Connecting to {}", redis_url);
    let redis_client = match redis::Client::open(redis_url) {
        Ok(client) => client,
        Err(e) => {
            error!("Error connecting to Redis: {}", e);
            return Err(CliError::RedisError { source: e });
        }
    };
    match matches.subcommand() {
        Some(("register", sub_matches)) => {
            let service_key = sub_matches.value_of("SERVICE_KEY").unwrap();
            let labels = match sub_matches.values_of("label") {
                Some(ls) => ls.collect(),
                None => Vec::new(),
            };
            let service = ServiceInstance {
                service_name: service_key.to_owned(),
                job_name: sub_matches
                    .value_of("job-name")
                    .unwrap_or(service_key)
                    .to_owned(),
                labels: labels
                    .chunks_exact(2)
                    .map(|lv| (lv[0].to_owned(), lv[1].to_owned()))
                    .collect(),
                host: sub_matches.value_of("host").unwrap().to_owned(),
                port: sub_matches.value_of("port").unwrap().parse()?,
                metrics_path: sub_matches
                    .value_of("metrics-path")
                    .unwrap_or("/metrics")
                    .to_owned(),
            };
            let mut redis_conn = try_redis_connect(&redis_client, max_timeout_sec)?;
            register_instance(&mut redis_conn, &service)?;
            Ok(())
        }
        Some(("unregister", sub_matches)) => {
            let service_key = sub_matches.value_of("SERVICE_KEY").unwrap();
            let host: Option<&str> = sub_matches.value_of("host");
            let mut redis_conn = try_redis_connect(&redis_client, max_timeout_sec)?;
            unregister_instance(&mut redis_conn, service_key, host)?;
            Ok(())
        }
        Some(("discover", sub_matches)) => {
            let out_path = Path::new(sub_matches.value_of("output").unwrap());
            // Write out the initial service definitions before starting to watch
            // for changes. This is so that we have a valid set of discovered targets
            // at all times, even when initially deploying the app.
            let mut con = try_redis_connect(&redis_client, max_timeout_sec)?;
            let services = discover_services(&mut con)?;
            serde_json::to_writer_pretty(File::create(out_path)?, &services)?;
            monitor_registry(&mut con, &redis_client, out_path)?;
            Ok(())
        }
        _ => Ok(()), // Should not happen, since clap will exit before
    }
}

fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();
    // FIXME: It would be really great to make the hostname optional, since it's most often
    //        equivalent to the running machines' hostname. Unfortunately Rust does not seem
    //        to have an equivalent of Python's `socket.getfqdn()`, so this is going to be a
    //        lot more complicated.
    let app = App::new("prometheus-sd")
        .author("Johannes Baiter <johannes.baiter@gmail.com>")
        .about("Simple redis-based service discovery for Prometheus")
        .version(env!("CARGO_PKG_VERSION"))
        .setting(AppSettings::ArgRequiredElseHelp)
        .args(&[
            arg!(-r --"redis-url" [TEXT] "URL for Redis server (default 'redis://localhost:6379')")
                .env("PROMETHEUS_SD_REDIS_URL")
                .takes_value(true),
            arg!(-t --"max-timeout" [NUMBER] "Maximum timeout in seconds to try initially connecting to Redis (default 28800 = 8 hours)")
                .env("PROMETHEUS_SD_REDIS_TIMEOUT")
                .takes_value(true)
        ])
        .subcommand(App::new("register")
                    .display_order(1)
                    .about("Register a new service instance in the environment.")
                    .mut_arg("help", |h| h.short('?'))
                    .args(&[
                        arg!(<SERVICE_KEY>                 "Sets the service key to use"),
                        arg!(-j --"job-name" [TEXT]        "Job name for the given service. Defaults to the service key."),
                        Arg::new("label")
                            .short('l')
                            .help("Labels to add to the service instance. Can be specified multiple times.")
                            .takes_value(true)
                            .number_of_values(2)
                            .multiple_occurrences(true)
                            .value_names(&["KEY", "VALUE"]),
                        arg!(-m --"metrics-path" [TEXT]    "Metrics path for the service. Defaults to /metrics."),
                        arg!(-h --host <TEXT>              "Hostname for the service."),
                        arg!(-p --port <INTEGER>           "Port the metrics are exported at.")

                    ]))
        .subcommand(App::new("unregister")
                    .display_order(2)
                    .about("Remove a service or a target for a service in the environment.")
                    .mut_arg("help", |h| h.short('?'))
                    .args(&[
                        arg!(<SERVICE_KEY>                 "Name of the service"),
                        arg!(-h --host [TEXT]              "Hostname for the service."),
                    ]))
        .subcommand(App::new("discover")
                    .display_order(3)
                    .about("Discover services in the environment.")
                    .long_about(DISCOVER_HELP)
                    .args(&[arg!(-o --output <FILE> "File to write the service definitions to")]));
    let matches = app.get_matches();
    match run_app(&matches) {
        Ok(_) => {}
        Err(e) => {
            error!("{}", e);
            process::exit(1);
        }
    }
}
