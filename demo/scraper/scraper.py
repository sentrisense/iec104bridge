#!/usr/bin/env python3
"""
DLR demo scraper.

Connects as an IEC-104 client to the bridge, sends a General Interrogation
every GI_INTERVAL seconds, decodes incoming ASDUs, and writes data points to
InfluxDB v2 with DLR-aware tagging.

IOA → semantic mapping (matches publisher.py):
  CA = line number (1 or 2)
  IOA 1001–1010  → span 1–10 temperature   (M_ME_NC_1, float, °C)
  IOA 2001–2010  → span 1–10 ampacity      (M_ME_NB_1, scaled, A)
  IOA 3001       → line ampacity (min)      (M_ME_NB_1, scaled, A)

InfluxDB schema
───────────────
measurement: "dlr"
tags:
  line             – "1" or "2"
  span             – "1".."10" (absent for line-level IOAs)
  measurement_type – "temperature" | "span_ampacity" | "line_ampacity"
  quality          – "good" | "not_topical" | "substituted" | "invalid" | …
fields:
  value            – float
  quality_code     – int  (0=good 1=not_topical 2=substituted 3=blocked
                           4=overflow 5=invalid) for state-timeline panels

Environment variables
─────────────────────
BRIDGE_HOST     bridge hostname / IP  (default: bridge)
BRIDGE_PORT     IEC-104 TCP port      (default: 2404)
GI_INTERVAL     seconds between GIs   (default: 5)
INFLUXDB_URL    InfluxDB base URL      (default: http://influxdb:8086)
INFLUXDB_TOKEN  API token              (default: demo-token)
INFLUXDB_ORG    organisation           (default: demo)
INFLUXDB_BUCKET target bucket          (default: iec104)
"""

import os
import socket
import struct
import time
import logging
from influxdb_client import InfluxDBClient, Point
from influxdb_client.client.write_api import SYNCHRONOUS

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger(__name__)

# ── Config ────────────────────────────────────────────────────────────────────
BRIDGE_HOST  = os.getenv("BRIDGE_HOST",   "bridge")
BRIDGE_PORT  = int(os.getenv("BRIDGE_PORT", "2404"))
GI_INTERVAL  = int(os.getenv("GI_INTERVAL", "5"))

INFLUXDB_URL    = os.getenv("INFLUXDB_URL",    "http://influxdb:8086")
INFLUXDB_TOKEN  = os.getenv("INFLUXDB_TOKEN",  "demo-token")
INFLUXDB_ORG    = os.getenv("INFLUXDB_ORG",    "demo")
INFLUXDB_BUCKET = os.getenv("INFLUXDB_BUCKET", "iec104")

# ── IOA layout ───────────────────────────────────────────────────────────────
IOA_TEMP_FIRST     = 1001
IOA_TEMP_LAST      = 1010
IOA_AMP_SPAN_FIRST = 2001
IOA_AMP_SPAN_LAST  = 2010
IOA_LINE_AMP       = 3001

# ── IEC-104 constants ─────────────────────────────────────────────────────────
TYPE_M_ME_NB_1 = 11
TYPE_M_ME_NC_1 = 13
TYPE_C_IC_NA_1 = 100

COT_ACTIVATION = 0x06

QDS_OVERFLOW    = 0x01
QDS_BLOCKED     = 0x10
QDS_SUBSTITUTED = 0x20
QDS_NOT_TOPICAL = 0x40
QDS_INVALID     = 0x80

QUALITY_CODE = {
    "good":        0,
    "not_topical": 1,
    "substituted": 2,
    "blocked":     3,
    "overflow":    4,
    "invalid":     5,
}

STARTDT_ACT = bytes([0x68, 0x04, 0x07, 0x00, 0x00, 0x00])
TESTFR_CON  = bytes([0x68, 0x04, 0x83, 0x00, 0x00, 0x00])


def quality_string(qds: int) -> str:
    if qds & QDS_INVALID:     return "invalid"
    if qds & QDS_NOT_TOPICAL: return "not_topical"
    if qds & QDS_SUBSTITUTED: return "substituted"
    if qds & QDS_BLOCKED:     return "blocked"
    if qds & QDS_OVERFLOW:    return "overflow"
    return "good"


def ioa_to_tags(ioa: int, ca: int) -> dict:
    """Derive DLR semantic tags from an IOA and CA."""
    line = str(ca)
    if IOA_TEMP_FIRST <= ioa <= IOA_TEMP_LAST:
        return {"line": line, "span": str(ioa - IOA_TEMP_FIRST + 1), "measurement_type": "temperature"}
    if IOA_AMP_SPAN_FIRST <= ioa <= IOA_AMP_SPAN_LAST:
        return {"line": line, "span": str(ioa - IOA_AMP_SPAN_FIRST + 1), "measurement_type": "span_ampacity"}
    if ioa == IOA_LINE_AMP:
        return {"line": line, "measurement_type": "line_ampacity"}
    # Unknown IOA – keep generic tags
    return {"line": line, "ioa": str(ioa), "measurement_type": "unknown"}


# ── IEC-104 frame helpers ─────────────────────────────────────────────────────

def recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("Connection closed by peer")
        buf += chunk
    return buf


def recv_apdu(sock: socket.socket) -> bytes:
    start = recv_exact(sock, 1)
    if start[0] != 0x68:
        raise ValueError(f"Expected 0x68, got 0x{start[0]:02x}")
    length_b = recv_exact(sock, 1)
    return start + length_b + recv_exact(sock, length_b[0])


def make_s_frame(rr: int) -> bytes:
    return bytes([0x68, 0x04, 0x01, 0x00, (rr << 1) & 0xFF, (rr >> 7) & 0xFF])


def make_gi_frame(ss: int, rr: int, ca: int = 1) -> bytes:
    cf1 = (ss << 1) & 0xFF; cf2 = (ss >> 7) & 0xFF
    cf3 = (rr << 1) & 0xFF; cf4 = (rr >> 7) & 0xFF
    asdu = bytes([TYPE_C_IC_NA_1, 0x01, COT_ACTIVATION, 0x00,
                  ca & 0xFF, (ca >> 8) & 0xFF, 0x00, 0x00, 0x00, 0x14])
    apdu = bytes([cf1, cf2, cf3, cf4]) + asdu
    return bytes([0x68, len(apdu)]) + apdu


def is_iframe(cf: bytes) -> bool: return (cf[0] & 0x01) == 0
def is_uframe(cf: bytes) -> bool: return (cf[0] & 0x03) == 0x03
def parse_send_seq(cf: bytes) -> int: return (cf[0] >> 1) | (cf[1] << 7)


# ── ASDU decoder ──────────────────────────────────────────────────────────────

def decode_asdu(asdu: bytes) -> list[dict]:
    if len(asdu) < 6:
        return []
    type_id = asdu[0]
    vsq     = asdu[1]
    sq      = bool(vsq & 0x80)
    count   = vsq & 0x7F
    ca      = asdu[4] | (asdu[5] << 8)

    if type_id not in (TYPE_M_ME_NB_1, TYPE_M_ME_NC_1):
        return []

    obj_offset = 6
    ioa        = 0
    points     = []

    for i in range(count):
        if obj_offset + 3 > len(asdu):
            break
        if i == 0 or not sq:
            ioa = asdu[obj_offset] | (asdu[obj_offset+1] << 8) | (asdu[obj_offset+2] << 16)
            obj_offset += 3
        else:
            ioa += 1

        if type_id == TYPE_M_ME_NC_1:
            if obj_offset + 5 > len(asdu): break
            value = float(struct.unpack_from("<f", asdu, obj_offset)[0])
            qds   = asdu[obj_offset + 4]
            obj_offset += 5
        else:
            if obj_offset + 3 > len(asdu): break
            value = float(struct.unpack_from("<h", asdu, obj_offset)[0])
            qds   = asdu[obj_offset + 2]
            obj_offset += 3

        quality = quality_string(qds)
        tags    = ioa_to_tags(ioa, ca)
        points.append({"value": value, "quality": quality, **tags})

    return points


# ── InfluxDB writer ───────────────────────────────────────────────────────────

def write_points(write_api, points: list[dict]) -> None:
    records = []
    for p in points:
        rec = Point("dlr").field("value", p["value"])
        # Semantic tags
        for tag in ("line", "span", "measurement_type", "quality", "ioa"):
            if tag in p:
                rec = rec.tag(tag, p[tag])
        # Numeric quality code for state-timeline panels
        rec = rec.field("quality_code", QUALITY_CODE.get(p["quality"], 0))
        records.append(rec)
    if records:
        write_api.write(bucket=INFLUXDB_BUCKET, org=INFLUXDB_ORG, record=records)
        log.info("Wrote %d point(s) to InfluxDB", len(records))


# ── Main loop ─────────────────────────────────────────────────────────────────

def run():
    influx_client = InfluxDBClient(url=INFLUXDB_URL, token=INFLUXDB_TOKEN, org=INFLUXDB_ORG)
    write_api = influx_client.write_api(write_options=SYNCHRONOUS)
    while True:
        try:
            _session(write_api)
        except (ConnectionError, OSError, ValueError) as exc:
            log.warning("Connection error: %s – reconnecting in 5 s", exc)
            time.sleep(5)


def _session(write_api):
    log.info("Connecting to %s:%d …", BRIDGE_HOST, BRIDGE_PORT)
    sock = socket.create_connection((BRIDGE_HOST, BRIDGE_PORT), timeout=10)
    sock.settimeout(None)
    log.info("Connected")

    sock.sendall(STARTDT_ACT)
    apdu = recv_apdu(sock)
    cf = apdu[2:6]
    if not is_uframe(cf) or cf[0] != 0x0B:
        raise ValueError(f"Expected STARTDT_CON, got CF={cf.hex()}")
    log.info("STARTDT confirmed")

    ss = 0
    rr = 0

    def send_gi(ca: int):
        nonlocal ss
        sock.sendall(make_gi_frame(ss, rr, ca))
        log.info("Sent GI CA=%d (ss=%d)", ca, ss)
        ss = (ss + 1) & 0x7FFF

    last_gi = 0.0

    while True:
        now = time.monotonic()
        if now - last_gi >= GI_INTERVAL:
            # Send GI for each line (CA)
            for ca in range(1, 3):
                send_gi(ca)
            last_gi = now

        sock.settimeout(GI_INTERVAL)
        try:
            apdu = recv_apdu(sock)
        except socket.timeout:
            continue

        cf = apdu[2:6]

        if is_uframe(cf):
            if cf[0] == 0x43:
                sock.sendall(TESTFR_CON)
            continue

        if is_iframe(cf):
            rr = (parse_send_seq(cf) + 1) & 0x7FFF
            points = decode_asdu(apdu[6:])
            if points:
                write_points(write_api, points)
            if rr % 8 == 0:
                sock.sendall(make_s_frame(rr))


if __name__ == "__main__":
    run()

