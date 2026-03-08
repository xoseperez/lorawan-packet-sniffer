# TSDB Comparison: QuestDB vs TimescaleDB vs InfluxDB vs TDengine vs VictoriaMetrics vs DuckDB vs SQLite

## Language

| TSDB | Language |
|------|----------|
| **QuestDB** | Zero-GC **Java** + **C++** (Enterprise adds Rust) |
| **TimescaleDB** | **C** (PostgreSQL extension) |
| **InfluxDB v2** | **Go** |
| **InfluxDB v3 Core** | **Rust** (built on Apache Arrow, DataFusion, Parquet) |
| **TDengine** | **C** |
| **VictoriaMetrics** | **Go** |
| **DuckDB** | **C++** (C++11) |
| **SQLite** | **C** (ANSI C) |

## Memory & CPU Footprint

| TSDB | Min RAM | Notes |
|------|---------|-------|
| **QuestDB** | ~8 GB recommended (production: 4 vCPU + 8 GiB minimum, 1:4 vCPU/RAM ratio ideal) | JVM-based, so inherently heavier. Memory-maps files aggressively for performance. **Heaviest.** |
| **TimescaleDB** | ~1-2 GB usable for small workloads (it's PostgreSQL underneath) | Inherits PG's tunable `shared_buffers`, `work_mem`. `timescaledb-tune` auto-configures based on available resources. **Medium footprint.** |
| **InfluxDB v2** | ~400 MB idle, but balloons quickly under load (OOM reports are common in the community) | Single Go binary, but memory management has been a pain point. |
| **InfluxDB v3 Core** | Not formally documented yet; Rust should be leaner than v2's Go runtime | New architecture (April 2025), still maturing. |
| **TDengine** | ~3 GB for typical deployments (formula-based: depends on vgroups, replicas, buffer config) | Written in C, designed for IoT. Claims 1/5 the hardware of OpenTSDB. 10K+ writes/sec per CPU core. **Lean for a full-featured TSDB.** |
| **VictoriaMetrics** | ~4.3 GB RSS in benchmarks (10x less than InfluxDB, 7x less than Prometheus) | Single Go binary. 7x lower CPU than Prometheus. Very efficient compression (0.4-1 byte per data point). **Leanest standalone TSDB.** |
| **DuckDB** | ~125 MB per thread (1 GB for 8 threads) | Embeddable library, not a server. Supports out-of-core (larger-than-memory) processing. **Lightest option if embedded in your app.** |
| **SQLite** | ~1 MB library, ~190-325 KB runtime heap | 590-750 KB compiled binary. Zero external deps. Designed for embedded systems. **Absolute lightest.** |

**Leanest to heaviest**: SQLite (embedded, <1 MB) > DuckDB (embedded) > VictoriaMetrics > InfluxDB v3 Core > TDengine > TimescaleDB > InfluxDB v2 > QuestDB

## Architecture Support

| TSDB | amd64 | arm64 | mips |
|------|-------|-------|------|
| **QuestDB** | Yes | Yes (Docker multi-arch) | **No** |
| **TimescaleDB** | Yes | Yes (Docker multi-arch) | **No** |
| **InfluxDB v2** | Yes | Yes | **No** |
| **InfluxDB v3 Core** | Yes | Yes (caveat: requires 4KB page size, RPi5 defaults to 16KB) | **No** |
| **TDengine** | Yes | Yes (Docker: amd64, arm64, arm32) | **Client only** (server: no; connectors: MIPS supported, MIPS64 planned) |
| **VictoriaMetrics** | Yes | Yes (optimized arm64 builds since v1.73.0) | **No** |
| **DuckDB** | Yes | Yes (Linux, macOS, Windows beta) | **No** |
| **SQLite** | Yes | Yes | **Yes** (runs on virtually everything: MIPS, RISC-V, PPC, SPARC, etc.) |

**Only SQLite supports MIPS.** TDengine has MIPS client/connector support but not for the server engine.

## Ease of Deployment

| TSDB | Single binary? | Docker? | Complexity |
|------|----------------|---------|------------|
| **QuestDB** | Yes (fat JAR, needs JVM) | Yes (`questdb/questdb`, amd64+arm64) | Medium — needs JVM runtime, but Docker makes it trivial |
| **TimescaleDB** | **No** — it's a PG extension | Yes (`timescale/timescaledb`, amd64+arm64) | **Highest** — requires a running PostgreSQL instance + extension installed. Docker hides this but it's still PG under the hood |
| **InfluxDB v2** | **Yes** — single Go binary | Yes (official `influxdb` image) | **Easiest** — download binary, run. No external deps |
| **InfluxDB v3 Core** | **Yes** — single Rust binary | Yes (official image) | **Easiest** — same single-binary model |
| **TDengine** | **No** — modular (taosd + taosAdapter + CLI) | Yes (`tdengine/tdengine`, amd64+arm64+arm32) | **Low** — package install or Docker. Designed for IoT rapid deployment. No external DB/cache deps needed |
| **VictoriaMetrics** | **Yes** — single Go binary | Yes (`victoriametrics/victoria-metrics`) | **Easiest** — download binary, run. Zero external deps |
| **DuckDB** | **Embeddable library** (no server needed) | Yes (`duckdb/duckdb`, but rarely needed) | **Trivial** — `pip install duckdb` or link the C++ library. Zero deps, no daemon, no ports |
| **SQLite** | **Embeddable library** (no server needed) | Not needed (but available) | **Trivial** — link 590 KB C library or use built-in language bindings (Python, Go, Rust, etc.). Zero deps, no daemon, no ports. Single-file database. |

## Storage: In-Memory vs Persistence

| TSDB | Model | Details |
|------|-------|---------|
| **QuestDB** | **Disk-persistent** with heavy mmap | Uses memory-mapped files for fast reads. WAL for crash recovery. Data lives on disk but performance depends on OS page cache (wants lots of RAM). |
| **TimescaleDB** | **Disk-persistent** (PostgreSQL storage) | Full ACID, WAL, hypertable partitioning. Supports compression + tiered storage. Most mature persistence story. |
| **InfluxDB v2** | **Disk-persistent** (TSM engine) | WAL + in-memory cache that spills to TSM files on disk. Retention policies + continuous downsampling built-in. |
| **InfluxDB v3 Core** | **Disk-persistent** (columnar/Parquet) | WAL + Parquet files. Can decouple compute from storage (object store). Most modern storage design. |
| **TDengine** | **Disk-persistent** (hybrid memory+disk) | MemTable (Red-Black Tree + SkipList) in RAM, flushes to disk on threshold. WAL with 2 levels (with/without fsync). 2-stage compression (delta-delta, simple8B, LZ4). |
| **VictoriaMetrics** | **Disk-persistent** (LSM-tree variant) | No traditional WAL — batches small writes before flushing. May lose 1-5 seconds of data on hard crash. 30x-75x compression ratio. |
| **DuckDB** | **Both** (persistent or in-memory mode) | Persistent mode: single compressed columnar file + WAL. In-memory mode: pass `:memory:` as path. Supports out-of-core disk spilling for large queries. |
| **SQLite** | **Disk-persistent** (also supports in-memory) | Single-file database. WAL mode for concurrent reads + writes. Full ACID. Configurable durability via `PRAGMA synchronous` (OFF/NORMAL/FULL). In-memory mode with `:memory:`. |

All options support **disk persistence**. DuckDB and SQLite also offer pure in-memory modes. VictoriaMetrics trades a few seconds of durability for write performance (no WAL).

## Write Throughput Suitability

For a LoRaWAN packet sniffer, expected throughput is low: a gateway typically handles tens to low hundreds of packets per second at most. This puts the requirement at **~1-1000 inserts/sec** — well within reach of every option, including SQLite.

| TSDB | Write Throughput | Notes |
|------|-----------------|-------|
| **QuestDB** | Millions of rows/sec | Massively over-provisioned for this use case. |
| **TimescaleDB** | Hundreds of thousands of rows/sec | Over-provisioned. Full SQL is a plus for complex queries. |
| **InfluxDB v2** | Hundreds of thousands of rows/sec | Over-provisioned. Well-suited for IoT metrics. |
| **InfluxDB v3 Core** | Expected to match or exceed v2 | Over-provisioned. |
| **TDengine** | 10K+ writes/sec per core; 1M rows/sec batched | Over-provisioned. Purpose-built for IoT ingest. |
| **VictoriaMetrics** | Hundreds of thousands of samples/sec | Over-provisioned. Optimized for metrics ingestion. |
| **DuckDB** | ~1.6M rows/sec (optimized batch inserts) | Over-provisioned. Better for analytics than continuous ingest. |
| **SQLite** | ~40 rows/sec (naive) / ~260K rows/sec (transactional) / ~1.6M rows/sec (batched+tuned) | **More than sufficient** for LoRaWAN throughput. Naive unbatched inserts (40/sec) could be tight at scale; use WAL mode + transactions for headroom. |

**Key insight**: At LoRaWAN packet rates (~1-1000 pkt/sec), **every option here is overkill** — including SQLite in WAL mode with basic transactions. The bottleneck will never be DB write throughput. This makes the decision purely about **footprint, deployment simplicity, and query capabilities**.

## Summary for LoRaWAN Packet Sniffer Use Case

Given this is likely targeting resource-constrained or edge deployments:

| Criterion | Winner |
|-----------|--------|
| Leanest footprint | **SQLite** (<1 MB library) then **DuckDB** (embedded, ~125 MB) |
| Easiest deployment | **SQLite** or **DuckDB** (embedded library, no server) |
| Most mature/stable | **SQLite** (25+ years, billions of deployments) or **TimescaleDB** (PostgreSQL) |
| Best for constrained ARM devices | **SQLite** > **DuckDB** > **VictoriaMetrics** > **TDengine** > **InfluxDB v2** |
| Purpose-built for IoT | **TDengine** (designed specifically for IoT/edge, C, built-in retention/downsampling) |
| Best analytics on captured data | **DuckDB** (columnar OLAP engine, great for batch analysis of Parquet/CSV) |
| MIPS support | **SQLite** (runs on virtually anything) |
| Sufficient throughput for LoRaWAN | **All of them** (even SQLite with WAL mode + transactions) |

### Recommendations

- **If you want absolute simplicity and minimal footprint**: **SQLite** — <1 MB, runs everywhere (including MIPS), full ACID, SQL, single-file DB. No time-series features (retention, downsampling) but trivial to implement manually. The most battle-tested DB in existence (billions of deployments). For LoRaWAN packet rates, WAL mode + batched inserts provides more than enough headroom.
- **If embedding in your app with analytics focus**: **DuckDB** — columnar engine for fast analytical queries on captured data. Heavier than SQLite (~125 MB) but much better at aggregate/window queries over large datasets.
- **If you want a dedicated TSDB server for IoT**: **TDengine** — purpose-built for IoT, written in C, supports ARM32/64, built-in stream processing and retention policies.
- **If you want the safest mainstream TSDB choice**: **InfluxDB v2** — proven on Raspberry Pi, huge ecosystem, single binary.
- **If monitoring/metrics is the primary use case**: **VictoriaMetrics** — incredibly lean, Prometheus-compatible, excellent compression.
- **If you need full SQL and ACID with time-series extensions**: **TimescaleDB** — but heaviest deployment (requires PostgreSQL).
- **QuestDB**: Best suited for high-throughput analytics on beefy servers. Not ideal for edge/IoT.

### SQLite Caveats for Time-Series

SQLite is not a purpose-built TSDB. You lose:
- Native retention policies (must implement `DELETE WHERE timestamp < X` via cron or app logic)
- Built-in downsampling/aggregation pipelines
- Time-series-specific compression (delta-of-delta, gorilla encoding, etc.)
- Native time-series query functions (gap filling, interpolation)

You gain:
- The smallest possible footprint (<1 MB)
- The widest architecture support (including MIPS)
- Zero deployment complexity (link a library, open a file)
- Full SQL with ACID guarantees
- 25+ years of battle-tested stability
- No network stack, no auth, no ports — minimal attack surface

For a packet sniffer writing ~1-1000 rows/sec with basic queries, these trade-offs strongly favor SQLite unless you need built-in dashboarding or streaming aggregation.

## Sources

- [QuestDB Memory Management](https://questdb.com/docs/guides/architecture/memory-management/)
- [QuestDB Capacity Planning](https://questdb.com/docs/operations/capacity-planning/)
- [QuestDB Docker Hub](https://hub.docker.com/r/questdb/questdb)
- [QuestDB ARM64 Community Discussion](https://community.questdb.com/t/arm64-support-and-optimization/915/1)
- [QuestDB GitHub](https://github.com/questdb/questdb)
- [TimescaleDB Docker Hub](https://hub.docker.com/r/timescale/timescaledb)
- [TimescaleDB GitHub](https://github.com/timescale/timescaledb)
- [timescaledb-tune](https://github.com/timescale/timescaledb-tune)
- [InfluxDB v3: Why Rust Beat Go (The New Stack)](https://thenewstack.io/influxdb-v3-why-rust-beat-go-for-time-series-database/)
- [InfluxDB v2 High RAM Usage (Community)](https://community.influxdata.com/t/influxdb-v2-high-ram-usage-and-leading-into-oom-and-constant-restart/32820)
- [InfluxDB 3 Core Install Docs](https://docs.influxdata.com/influxdb3/core/install/)
- [InfluxDB 3 Core RPi5 ARM64 Issue](https://github.com/influxdata/influxdb/issues/26066)
- [InfluxDB Docker Hub](https://hub.docker.com/_/influxdb/)
- [TDengine Documentation](https://docs.tdengine.com/)
- [TDengine System Requirements](https://docs.tdengine.com/operations-and-maintenance/system-requirements/)
- [TDengine Storage Engine](https://docs.tdengine.com/inside-tdengine/storage-engine/)
- [TDengine Supported Platforms](https://docs.tdengine.com/tdengine-reference/supported-platforms/)
- [TDengine GitHub](https://github.com/taosdata/TDengine)
- [VictoriaMetrics Single-node Docs](https://docs.victoriametrics.com/victoriametrics/single-server-victoriametrics/)
- [VictoriaMetrics Quick Start](https://docs.victoriametrics.com/victoriametrics/quick-start/)
- [VictoriaMetrics on ARM](https://victoriametrics.com/blog/vm-on-arm/index.html)
- [VictoriaMetrics Docker Hub](https://hub.docker.com/r/victoriametrics/victoria-metrics)
- [VictoriaMetrics GitHub](https://github.com/VictoriaMetrics/VictoriaMetrics)
- [Prometheus vs VictoriaMetrics Benchmark](https://valyala.medium.com/prometheus-vs-victoriametrics-benchmark-on-node_exporter-metrics-4ca29c75590f)
- [DuckDB Performance Environment Guide](https://duckdb.org/docs/stable/guides/performance/environment)
- [DuckDB Memory Management](https://duckdb.org/2024/07/09/memory-management)
- [DuckDB Installation](https://duckdb.org/install/)
- [DuckDB FAQ](https://duckdb.org/faq)
- [DuckDB Docker Docs](https://duckdb.org/docs/stable/operations_manual/duckdb_docker)
- [DuckDB GitHub](https://github.com/duckdb/duckdb)
- [SQLite Official Homepage](https://www.sqlite.org/)
- [SQLite Library Footprint](https://www.sqlite.org/footprint.html)
- [SQLite Architecture](https://www.sqlite.org/arch.html)
- [SQLite Write-Ahead Logging (WAL)](https://www.sqlite.org/wal.html)
- [SQLite Performance Benchmarks](https://www.sqlite.org/speed.html)
- [Fast SQLite Bulk Inserts](https://avi.im/blag/2021/fast-sqlite-inserts/)
- [Maximum Speed SQLite Inserts](https://blog.julik.nl/2025/01/maximum-speed-sqlite-inserts)
