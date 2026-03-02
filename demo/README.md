# IEC-104 Bridge demo – Dynamic Line Rating (DLR)

Spins up a complete, NATS-driven observability stack with a single command.
The simulated scenario is a transmission-system operator monitoring
**Dynamic Line Rating** on two overhead power lines, each divided into
**10 spans**.  Every span reports conductor temperature and ampacity;
the per-line ampacity is the minimum of all valid span values.

```
 publisher (Python)
  │ 2 lines × 10 spans: sinusoidal temperature drift
  │ + Gaussian noise + quality random-walk
  │ NATS JetStream  sensors.measurements
  ▼
 nats1 ─── nats2 ─── nats3  (3-node JetStream cluster)
  ▲
  │ async_nats pull consumer
  ▼
iec104bridge [IEC-104 server :2404]   ──── Prometheus metrics :9091
  ▲                                              │
  │  IEC-104 protocol (GI every 5 s)             ▼
 scraper (Python)                       Prometheus [:9090]
  │                                              │
  │ influxdb-client  →  measurement: dlr         │
  ▼                                              │
  InfluxDB v2 [:8086]                            │
  │                                              │
  └──────────────────┬───────────────────────────┘
                     ▼
              Grafana [:3000]
         (auto-provisioned DLR dashboard)
```

## Quick start

```bash
cd demo
docker compose up --build
```

Open **http://localhost:3000** → dashboards → **DLR – Dynamic Line Rating Demo**.
No login required (anonymous viewer access is enabled).

| Service             | URL                             | Notes                              |
|---------------------|---------------------------------|------------------------------------|
| Grafana             | http://localhost:3000           | admin / admin for edit access      |
| InfluxDB            | http://localhost:8086           | admin / password123                |
| Prometheus          | http://localhost:9090           | —                                  |
| Bridge metrics      | http://localhost:9091/metrics   | raw Prometheus text                |
| NATS monitoring     | http://localhost:8222           | nats1 only; /healthz, /jsz, /varz  |
| IEC-104             | tcp://localhost:2404            | connect any IEC-104 client         |

## IOA addressing scheme

Each power line maps to a separate IEC-104 **Common Address (CA)**:

| CA | Power line |
|----|------------|
| 1  | Line 1     |
| 2  | Line 2     |

Within each CA the IOAs are:

| IOA range   | IEC-104 type  | Quantity                      | Unit |
|-------------|---------------|-------------------------------|------|
| 1001 – 1010 | M_ME_NC_1 (13) | Span 1–10 conductor temperature | °C  |
| 2001 – 2010 | M_ME_NB_1 (11) | Span 1–10 ampacity (capacity)   | A   |
| 3001        | M_ME_NB_1 (11) | Line ampacity (min of spans)    | A   |

IOA 3001 always inherits the quality of the worst contributing span.

## Simulation model

| Parameter            | Line 1          | Line 2          |
|----------------------|-----------------|-----------------|
| Base temperature     | 62 – 68 °C      | 70 – 76 °C      |
| Base ampacity        | 490 – 520 A     | 450 – 480 A     |
| Diurnal drift        | ±8 °C sinusoid, period 300 s | same |
| Temperature noise    | Gaussian σ = 1.5 °C         | same |
| Ampacity offset      | −drift × 3  A (inverse of temperature) | same |
| Ampacity noise       | Gaussian σ = 12 A           | same |
| Quality random walk  | 3 % chance to degrade per cycle; 40–60 % chance to recover | same |

Quality states (in degradation order): `good` → `not_topical` → `substituted` → `invalid`.

## How it works

1. **nats1/2/3** form a 3-node JetStream cluster.  A one-shot **nats-setup**
   container creates the `SENSORS` stream (subject filter `sensors.>`) and exits.
2. **publisher** connects to the NATS cluster and every `PUBLISH_INTERVAL` seconds
   publishes a full snapshot: 42 messages per cycle (2 lines × (10 temps + 10
   ampacities + 1 line ampacity)).  Quality drifts via a per-span random walk.
3. **bridge** consumes the `SENSORS` stream as a JetStream pull consumer, caches
   the latest value per IOA per CA, and dispatches spontaneous IEC-104 ASDUs to
   any connected client.  It also serves Prometheus metrics on port 9091.
4. **scraper** connects as an IEC-104 client, sends a General Interrogation for
   **each** CA (line) every `GI_INTERVAL` seconds, decodes `M_ME_NC_1` and
   `M_ME_NB_1` ASDUs, maps IOAs to `line`, `span`, and `measurement_type` tags,
   and writes each point to InfluxDB measurement `dlr`.  A `quality_code` numeric
   field (0 = good … 5 = invalid) is stored alongside `value` for state-timeline
   panels.
5. **InfluxDB** persists the time-series data in the `iec104` bucket.
6. **Prometheus** scrapes the bridge metrics endpoint every 15 s.
7. **Grafana** is pre-provisioned with both datasources and a ready-to-use DLR
   dashboard.

## What to expect in Grafana

Use the **Power Line** variable at the top of the dashboard to switch between
Line 1 and Line 2.  All panels update automatically.

### Line-level status row

| Panel               | What it shows                                                      |
|---------------------|--------------------------------------------------------------------|
| **Line Ampacity**   | Current line rating in amperes (stat, colour green ≥ 480 A / yellow ≥ 400 A / red below). Because ampacity is inversely correlated with temperature, it oscillates in anti-phase to the sinusoidal temperature drift. |
| **Line Quality**    | Quality inherited from the span with the lowest ampacity.  Normally "good" (green); turns yellow / red when the quality random walk degrades one or more spans. |

### Span time-series row

| Panel                       | What it shows                                                 |
|-----------------------------|---------------------------------------------------------------|
| **Span Temperatures (°C)**  | One series per span (10 traces).  All lines follow a shared sinusoidal envelope with per-span Gaussian noise on top. |
| **Span Ampacities (A)**     | One series per span.  Mirror image of temperatures: as temperature rises, ampacity falls and vice versa. |

### Quality state-timeline row

Each row shows one coloured band per span across time.

| Panel                          | States shown                                              |
|--------------------------------|-----------------------------------------------------------|
| **Span Temperature Quality**   | green = good, yellow = not topical, orange = substituted, red = invalid |
| **Span Ampacity Quality**      | same colour mapping                                       |

Most spans stay green.  Occasionally the random walk moves a span to a
degraded state before it recovers.  This mimics real-world communication
or sensor faults that appear and clear without operator intervention.

## Environment variables

### Bridge

| Variable              | Default | Description                          |
|-----------------------|---------|--------------------------------------|
| `NATS_URL`            | —       | NATS server URL(s), comma-separated  |
| `NATS_STREAM`         | —       | JetStream stream name                |
| `NATS_CONSUMER`       | —       | Durable consumer name                |
| `NATS_SUBJECT_FILTER` | —       | Optional subject filter              |
| `IEC104_PORT`         | 2404    | IEC-104 TCP listen port              |
| `IEC104_CA`           | 1       | Default Common Address               |
| `METRICS_PORT`        | 9091    | Prometheus metrics HTTP port         |

### Publisher

| Variable           | Default              | Description                          |
|--------------------|----------------------|--------------------------------------|
| `NATS_URL`         | nats://nats1:4222    | NATS server URL(s), comma-separated  |
| `NATS_SUBJECT`     | sensors.measurements | Subject to publish to                |
| `NATS_STREAM`      | SENSORS              | JetStream stream name                |
| `PUBLISH_INTERVAL` | 5.0                  | Seconds between full-cycle snapshots |

### Scraper

| Variable          | Default              | Description              |
|-------------------|----------------------|--------------------------|
| `BRIDGE_HOST`     | bridge               | Bridge hostname          |
| `BRIDGE_PORT`     | 2404                 | IEC-104 TCP port         |
| `GI_INTERVAL`     | 5                    | Seconds between GIs      |
| `INFLUXDB_URL`    | http://influxdb:8086 | InfluxDB base URL        |
| `INFLUXDB_TOKEN`  | demo-token           | InfluxDB API token       |
| `INFLUXDB_ORG`    | demo                 | InfluxDB organisation    |
| `INFLUXDB_BUCKET` | iec104               | InfluxDB bucket          |
