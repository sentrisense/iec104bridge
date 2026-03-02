#!/usr/bin/env python3
"""
DLR (Dynamic Line Rating) demo publisher.

Simulates monitoring of 2 power lines, each divided into 10 spans.
For every span it publishes:
  - a temperature value   (M_ME_NC_1, float, °C)   IOA 1001–1010
  - an ampacity value     (M_ME_NB_1, scaled, A)    IOA 2001–2010

For each line it also publishes:
  - the line ampacity     (M_ME_NB_1, scaled, A)    IOA 3001
    = the minimum span ampacity; quality inherits from that span.

Each power line maps to a separate IEC-104 Common Address (CA):
  Line 1 → CA 1
  Line 2 → CA 2

Quality simulation
──────────────────
Most spans are "good".  A small random walk occasionally pushes a span into
a degraded quality state (not_topical → substituted → back to good) to
exercise the state-timeline panels in Grafana.

Environment variables
─────────────────────
NATS_URL          NATS server URL         (default: nats://nats1:4222)
NATS_SUBJECT      Target subject          (default: sensors.measurements)
NATS_STREAM       JetStream stream name   (default: SENSORS)
PUBLISH_INTERVAL  Seconds between full
                  cycle publications      (default: 5.0)
"""

import asyncio
import json
import logging
import math
import os
import random
import time

import nats
from nats.js.api import RetentionPolicy, StorageType, StreamConfig

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger(__name__)

NATS_URL         = os.getenv("NATS_URL",         "nats://nats1:4222")
NATS_SUBJECT     = os.getenv("NATS_SUBJECT",     "sensors.measurements")
NATS_STREAM      = os.getenv("NATS_STREAM",      "SENSORS")
PUBLISH_INTERVAL = float(os.getenv("PUBLISH_INTERVAL", "5.0"))

N_LINES = 2
N_SPANS = 10

# IOA layout (per CA / line):
IOA_TEMP_BASE     = 1001   # 1001..1010  → span 1..10 temperature
IOA_AMPACITY_BASE = 2001   # 2001..2010  → span 1..10 ampacity
IOA_LINE_AMPACITY = 3001   # 3001        → line ampacity (min of spans)

# Quality states ordered by severity (index 0 = best)
QUALITY_STATES = ["good", "good", "good", "not_topical", "substituted", "invalid"]

# Per-line, per-span base temperatures (°C) and ampacities (A).
# Line 2 runs slightly hotter (different conductor / exposure).
BASE_TEMP = {
    1: [62.0, 64.5, 67.0, 65.3, 63.8, 66.2, 68.1, 64.0, 65.9, 63.1],
    2: [70.0, 72.5, 75.0, 73.3, 71.8, 74.2, 76.1, 72.0, 73.9, 71.1],
}
BASE_AMPACITY = {
    1: [520, 510, 495, 505, 515, 500, 490, 508, 512, 518],
    2: [480, 470, 455, 465, 475, 460, 450, 468, 472, 478],
}

# Temperature oscillation: slow sinusoidal drift (simulates diurnal cycle).
TEMP_AMPLITUDE = 8.0    # °C peak swing
TEMP_PERIOD    = 300.0  # seconds for a full cycle

# Ampacity noise (Gaussian, σ in A).
AMP_SIGMA = 12.0

# Quality random walk: each span independently drifts toward a degraded state
# with low probability each cycle, and recovers with higher probability.
# State is an index into QUALITY_STATES.
_quality_state: dict[tuple[int, int], int] = {
    (line, span): 0
    for line in range(1, N_LINES + 1)
    for span in range(1, N_SPANS + 1)
}


def _step_quality(line: int, span: int) -> str:
    """Advance the quality random walk for one span and return the quality string."""
    state = _quality_state[(line, span)]
    r = random.random()
    if state == 0:
        # Healthy: small chance of degrading
        if r < 0.03:
            state = 1  # move toward not_topical
    elif state < len(QUALITY_STATES) - 1:
        if r < 0.4:
            state -= 1  # recover
        elif r > 0.85:
            state += 1  # degrade further
    else:
        # At worst state: likely to recover
        if r < 0.6:
            state -= 1
    _quality_state[(line, span)] = state
    return QUALITY_STATES[state]


async def ensure_stream(js) -> None:
    try:
        await js.stream_info(NATS_STREAM)
        log.info("Stream '%s' already exists", NATS_STREAM)
    except Exception:
        # Stream not found or not yet ready — try to create it.
        # nats-setup should have already created it; this is a fallback.
        cfg = StreamConfig(
            name=NATS_STREAM,
            subjects=["sensors.>"],
            retention=RetentionPolicy.LIMITS,
            storage=StorageType.FILE,
            max_msgs=500_000,
            max_bytes=-1,
            num_replicas=1,
        )
        try:
            await js.add_stream(config=cfg)
            log.info("Created stream '%s'", NATS_STREAM)
        except Exception as add_err:
            log.warning("Stream '%s' add_stream: %s — assuming it already exists", NATS_STREAM, add_err)


async def publish_all(js, t: float) -> None:
    """Publish one complete snapshot of all lines and spans."""
    for line in range(1, N_LINES + 1):
        ca = line   # one CA per line

        min_amp     = None
        min_quality = "good"

        for span in range(1, N_SPANS + 1):
            quality = _step_quality(line, span)

            # ── Temperature ───────────────────────────────────────────────
            temp_base  = BASE_TEMP[line][span - 1]
            drift      = TEMP_AMPLITUDE * math.sin(2 * math.pi * t / TEMP_PERIOD)
            noise_temp = random.gauss(0.0, 1.5)
            temp       = round(temp_base + drift + noise_temp, 2)

            # ── Span ampacity ─────────────────────────────────────────────
            amp_base  = BASE_AMPACITY[line][span - 1]
            # Ampacity inversely tracks temperature: hotter → lower rating
            amp_adj   = -drift * 3.0
            noise_amp = random.gauss(0.0, AMP_SIGMA)
            amp       = int(round(amp_base + amp_adj + noise_amp))

            # Track minimum for line ampacity
            if quality not in ("invalid", "not_topical"):
                if min_amp is None or amp < min_amp:
                    min_amp     = amp
                    min_quality = quality

            ioa_temp = IOA_TEMP_BASE     + (span - 1)
            ioa_amp  = IOA_AMPACITY_BASE + (span - 1)

            for ioa, value, typ in [
                (ioa_temp, temp,  "float"),
                (ioa_amp,  amp,   "scaled"),
            ]:
                msg = {
                    "ioa":     ioa,
                    "value":   value,
                    "type":    typ,
                    "ca":      ca,
                    "quality": quality,
                    "cot":     "spontaneous",
                }
                await js.publish(NATS_SUBJECT, json.dumps(msg).encode())

            log.debug(
                "L%d S%02d  temp=%-6.1f°C  amp=%-5d A  quality=%s",
                line, span, temp, amp, quality,
            )

        # ── Line ampacity (min of all valid spans) ─────────────────────────
        line_amp     = min_amp if min_amp is not None else 0
        line_quality = min_quality if min_amp is not None else "invalid"

        msg = {
            "ioa":     IOA_LINE_AMPACITY,
            "value":   line_amp,
            "type":    "scaled",
            "ca":      ca,
            "quality": line_quality,
            "cot":     "spontaneous",
        }
        await js.publish(NATS_SUBJECT, json.dumps(msg).encode())

        log.info(
            "Line %d published: line_amp=%d A  line_quality=%s",
            line, line_amp, line_quality,
        )


async def run() -> None:
    servers = [s.strip() for s in NATS_URL.split(",")]
    log.info("Connecting to NATS at %s", servers)
    nc = await nats.connect(servers=servers)
    js = nc.jetstream()

    await ensure_stream(js)

    n_points = N_LINES * (N_SPANS * 2 + 1)
    log.info(
        "DLR publisher ready: %d lines × %d spans = %d IOAs per cycle, interval=%.1f s",
        N_LINES, N_SPANS, n_points, PUBLISH_INTERVAL,
    )

    t0 = time.monotonic()
    while True:
        t = time.monotonic() - t0
        await publish_all(js, t)
        await asyncio.sleep(PUBLISH_INTERVAL)


if __name__ == "__main__":
    asyncio.run(run())
