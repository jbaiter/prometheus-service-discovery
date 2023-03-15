# Prometheus Service Discovery

This tool implements Service Discovery for Prometheus via a shared Redis instance.
Every distributed instance registers itself via the `register` subcommand. The discovery
then happens on the machine the Prometheus server is running on via the `discover`
subcommand, which will monitor for changes to the service registry and write out the
discovered services to a JSON file, where it can then be picked up by Prometheus'
[file-based discovery mechanism](https://prometheus.io/docs/guides/file-sd/).


**Installation**:

Download the latest release from the [Releases Page](https://github.com/jbaiter/prometheus-service-discovery/releases).

The tarball contains a static binary without any dependencies and can be installed in the location of your choice.


## How it works at a glance
- Configure a Redis instance that is accessible by all participants
- Run the `register` subcommand whenever a service is started on some node
  (best done via `ExecStartPost` in systemd)
- Run the `discover` subcommand on the Prometheus node to monitor for new
  services or service changes and write the changes to a file.
- Instruct Prometheus to monitor the file written by the `discovery`
  process


## Common options and available subcommands
```
$ prometheus-sd --help
Johannes Baiter <johannes.baiter@gmail.com>
Simple redis-based service discovery for Prometheus

USAGE:
    prometheus-sd [OPTIONS] [SUBCOMMAND]

OPTIONS:
    -h, --help                         Print help information
    -r, --redis-url [<TEXT>...]        URL for Redis server (default 'redis://localhost:6379') [env:
                                       PROMETHEUS_SD_REDIS_URL=]
    -t, --max-timeout [<NUMBER>...]    Maximum timeout in seconds to try initially connecting to
                                       Redis (default 28800 = 8 hours) [env:
                                       PROMETHEUS_SD_REDIS_TIMEOUT=]
    -V, --version                      Print version information

SUBCOMMANDS:
    register    Register a new service instance in the environment.
    discover    Discover services in the environment.
    help        Print this message or the help of the given subcommand(s)
```

The URL that can be passed in via the `-r`/`--redis-url` option follows the
following format: `redis://[:<passwd>@]<hostname>[:port][/<db>]`.
Alternatively you can also use a Unix socket: `redis+unix:///[:<passwd>@]<path>[?db=<db>]`.
Instead of the command-line flags, you can also use the following environment variables:
- `PROMETHEUS_SD_REDIS_URL`
- `PROMETHEUS_SD_REDIS_TIMEOUT`
Please note that passwords must be URL-escaped if you're using special characters.


## Service Registration
```
$ prometheus-sd register --help
Register a new service instance in the environment.

USAGE:
    prometheus-sd register [OPTIONS] --host <TEXT> --port <INTEGER> <SERVICE_KEY>

ARGS:
    <SERVICE_KEY>    Sets the service key to use

OPTIONS:
    -?, --help                        Print help information
    -h, --host <TEXT>                 Hostname for the service.
    -j, --job-name [<TEXT>...]        Job name for the given service. Defaults to the service key.
    -l <KEY> <VALUE>                  Labels to add to the service instance. Can be specified
                                      multiple times.
    -m, --metrics-path [<TEXT>...]    Metrics path for the service. Defaults to /metrics.
    -p, --port <INTEGER>              Port the metrics are exported at.
```

**Example:**
```sh
# The simplest case, the job name is identical to the service key, no custom labels
$ prometheus-sd register some_service -h example.com -p 9000
# A service with a custom job name and two custom labels
$ prometheus-sd register some_service_blue -j some_service -l color blue -l is_discovered 1 -h example.com -p 1337
```

Service registration should happen on service startup. If you are using systemd, a good
place to put it is in the [`ExecStartPre` or `ExitStartPost` options](https://www.freedesktop.org/software/systemd/man/systemd.service.html#ExecStartPre=),
either directly in the service file or via the "drop-in" mechanism to alter existing distribution or
third-party services.


## Service Discovery
```
$ prometheus-sd discover --help
prometheus-sd-discover
Discover services in the environment.

This is a long-running process that will continously monitor Redis for the
registration of new services and, upon any modifications to the service
registry, write the services as JSON to an output path where it can be picked
up by Prometheus' file-based discovery process.

USAGE:
    prometheus-sd discover --output <FILE>

OPTIONS:
    -h, --help
            Print help information

    -o, --output <FILE>
            File to write the service definitions to
```

**Example:**
```sh
# Will update /etc/prometheus/services.json every time the service registry is modified
$ ./prometheus-sd discover -o /etc/prometheus/services.json
```

The `discover` subcommand is intended to be run as a background service that takes care
of writing out the service definitions any time the service registry changes.

## Prometheus Configuration
The service discovery is based on Prometheus'  [file-based service discovery](https://prometheus.io/docs/guides/file-sd/).
To enable it for use with this tool, add the following section to your `scrape_configs` section in the Prometheus configuration:

```yaml
scrape_configs:
- job_name: 'will-be-overriden'  # ...by the job label from the discovered services, needs to be present, tho!
  file_sd_configs:
  - files:
    - '/etc/prometheus/services.json'
  # This is neccessary to allow for dynamic metric paths per service instance, don't leave this out!
  relabel_configs:
  - source_labels: [__address__]  # Extract metrics path from address
    regex: '[^/]+(/.*)'
    target_label: __metrics_path__
  - source_labels: [__address__]  # Strip metrics path from address
    regex:  '([^/]+)/.*'
    target_label: __address__
```
