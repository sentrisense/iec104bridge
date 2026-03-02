# IEC-104 Bridge

Reads JSON-encoded process values from a message source (NATS JetStream or a
local file) and forwards them to connected IEC-104 clients as spontaneous data
using [lib60870](https://github.com/mz-automation/lib60870).

---

## Architecture

```
Message source                Bridge                  IEC-104 clients
──────────────   JSON       ──────────────────     ──────────────────────
NATS JetStream ──────────►  parse + dispatch  ───► lib60870 server (TCP 2404)
  – or –                     data cache             any IEC-104 master
Local file
```

The bridge maintains a **data cache** keyed by `(ca, ioa)`.  When a client
sends a **General Interrogation** command the bridge replays the most recent
value for every cached IOA.

---

## JSON message format

Every message must be a valid JSON object.  Fields:

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `ioa` | integer | ✓ | – | Information Object Address (1 – 16 777 215) |
| `value` | bool or number | ✓ | – | Process value |
| `type` | string | – | inferred | ASDU type (see table below) |
| `ca` | integer | – | `IEC104_CA` | Common Address |
| `quality` | string | – | `"good"` | Quality descriptor |
| `cot` | string | – | `"spontaneous"` | Cause of Transmission |

### Type values

| `"type"` | IEC-104 type | lib60870 encoding |
|---|---|---|
| `"single_point"` | M_SP_NA_1 (1) | boolean ON/OFF |
| `"float"` | M_ME_NC_1 (13) | 32-bit float |
| `"scaled"` | M_ME_NB_1 (11) | signed 16-bit integer, clamped |
| `"normalized"` | M_ME_NA_1 (9) | encoded as float |
| `"double_point"` | *(fallback)* | sent as single point |

When `"type"` is omitted the type is **inferred**:

| JSON value | Inferred type |
|---|---|
| `true` / `false` | `single_point` |
| integer (`42`) | `scaled` |
| float (`3.14`) | `float` |

### Quality values

`"good"` (default), `"invalid"`, `"not_topical"`, `"substituted"`, `"blocked"`, `"overflow"`

### COT values

`"spontaneous"` (default), `"periodic"`, `"background_scan"`, `"interrogated"`, `"return_info_remote"`, `"return_info_local"`

---

## Configuration

All configuration is via environment variables.

| Variable | Default | Description |
|---|---|---|
| `INPUT_FILE` | *(unset)* | Path to a `.jsonl` file.  When set, NATS is not used. |
| `IEC104_PORT` | `2404` | TCP port for the IEC-104 server |
| `IEC104_CA` | `1` | Default Common Address when the message omits `"ca"` |
| `IEC104_BIND_ADDR` | `0.0.0.0` | Interface to bind on |
| `NATS_URL` | `nats://localhost:4222` | NATS server URL *(NATS mode only)* |
| `NATS_STREAM` | *(required)* | JetStream stream name *(NATS mode only)* |
| `NATS_CONSUMER` | *(required)* | Durable consumer name *(NATS mode only)* |
| `NATS_SUBJECT_FILTER` | *(unset)* | Optional subject filter *(NATS mode only)* |
| `RUST_LOG` | `iec104bridge=info` | Log level filter (uses `tracing-subscriber`) |

---

## Building

```bash
cargo build --release
# binary: target/release/iec104bridge
```

---

## Running in file mode

Set `INPUT_FILE` to point at a newline-delimited JSON file.  NATS credentials
are not required.

```bash
export INPUT_FILE=examples/messages.jsonl
export IEC104_CA=1          # optional, matches "ca" in the file
export IEC104_PORT=2404     # optional
cargo run
```

The bridge reads all messages, stores them in the data cache, then **keeps the
IEC-104 server running** until you press **Ctrl-C**.  Clients can connect at
any point and retrieve all values via a General Interrogation.

---

## Testing with the lib60870 example client

The [`simple_client`](https://github.com/mz-automation/lib60870/tree/master/lib60870-C/examples/cs104_client)
example from the lib60870-C library is a convenient IEC-104 master for manual
testing.  It connects, sends a **General Interrogation**, prints received ASDUs,
and disconnects.

### 1 – Build the C client

```bash
git clone https://github.com/mz-automation/lib60870.git
cd lib60870/lib60870-C
mkdir build && cd build
cmake ..
make
# binary: examples/cs104_client/simple_client
```

### 2 – Start the bridge (terminal 1)

```bash
cd /path/to/iec104bridge
export INPUT_FILE=examples/messages.jsonl
export IEC104_CA=1
RUST_LOG=iec104bridge=debug cargo run
```

Expected output:

```
INFO  iec104bridge: Starting IEC-104 bridge source="file" iec104_port=2404 iec104_ca=1 ...
INFO  iec104bridge: IEC-104 server started port=2404
INFO  iec104bridge: Using file source (INPUT_FILE mode) path="examples/messages.jsonl"
DEBUG iec104bridge::bridge: dispatching IEC-104 message ioa=1001 ca=1 ...
...
WARN  iec104bridge: Message stream exhausted – IEC-104 server still active, waiting for Ctrl-C
```

### 3 – Run the C client (terminal 2)

```bash
# Default: connects to localhost:2404, CA=1
./lib60870/lib60870-C/build/examples/cs104_client/simple_client

# Connect to a different host/port:
./simple_client 192.168.1.10 2404
```

Expected client output:

```
Connecting to: localhost:2404
Connected!
Received STARTDT_CON
RECVD ASDU type: M_SP_NA_1(1) elements: 1
  single point information:
    IOA: 1001 value: 1
RECVD ASDU type: M_SP_NA_1(1) elements: 1
  single point information:
    IOA: 1002 value: 0
...
RECVD ASDU type: M_ME_NC_1(13) elements: 1
RECVD ASDU type: M_ME_NB_1(11) elements: 1
...
```

> **Note:** `simple_client.c` fully decodes `M_SP_NA_1` (single-point) and
> `M_ME_TE_1` (scaled with timestamp).  For `M_ME_NC_1` (float) and
> `M_ME_NB_1` (scaled, no timestamp) it prints the type name and element
> count only — the values are still transmitted correctly and would be
> decoded by a full IEC-104 master such as
> [OpenMUC](https://www.openmuc.org/) or [ScadaBR](http://www.scadabr.com/).

### 4 – Workflow timeline

```
t=0s   Bridge starts, reads file, caches all 30 values, server listens on :2404
t=2s   Client connects, sends STARTDT, then General Interrogation (QOI=20, CA=1)
t=2s   Bridge receives GI, sends ACT_CON, replays all 30 cached values, sends ACT_TERM
t=7s   Client disconnects
       ^── bridge stays up for further connections until Ctrl-C
```

---

## Running with NATS

```bash
export NATS_URL="nats://localhost:4222"
export NATS_STREAM="sensors"
export NATS_CONSUMER="iec104bridge"
export NATS_SUBJECT_FILTER="plant.a.>"   # optional
export IEC104_PORT=2404
export IEC104_CA=1
cargo run
```

Publish a message:

```bash
nats pub plant.a.breaker '{"ioa":1001,"value":true,"type":"single_point"}'
nats pub plant.a.voltage '{"ioa":2001,"value":132.4,"type":"float"}'
```

---

## Example messages file

[`examples/messages.jsonl`](examples/messages.jsonl) contains a representative
set of all supported types:

| IOA range | Type | Description |
|---|---|---|
| 1001–1103 | `single_point` | Circuit breaker and relay status |
| 2001–2203 | `float` | Voltages, frequency, temperatures |
| 3001–3202 | `scaled` | Power and current measurements |
| 4001–4003 | *(inferred)* | Type-inference demonstration |

---

## Input sources

| Source | How to select | Use case |
|---|---|---|
| `NatsSource` | `INPUT_FILE` not set | Production |
| `FileSource` | `INPUT_FILE=/path/to/file.jsonl` | Local testing, log replay |
| `IterSource` | Programmatically (tests only) | Unit / integration tests |

New sources (e.g. Unix socket) can be added by implementing the
`MessageSource` trait in `src/source.rs`.
