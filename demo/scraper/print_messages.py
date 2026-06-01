#!/usr/bin/env python3

"""Print IEC-104 measurement ASDUs to stdout, including CP56Time2a timestamps.

This is a lightweight manual verification client for the bridge. It connects as
an IEC-104 client, sends STARTDT, optionally issues General Interrogation for
one or more CAs, and prints decoded measurement messages as JSON lines.
"""

import json
import os
import socket
import struct
import time


BRIDGE_HOST = os.getenv("BRIDGE_HOST", "127.0.0.1")
BRIDGE_PORT = int(os.getenv("BRIDGE_PORT", "2404"))
GI_INTERVAL = float(os.getenv("GI_INTERVAL", "0"))
GI_CAS = [int(value) for value in os.getenv("GI_CAS", "1").split(",") if value.strip()]

TYPE_M_SP_NA_1 = 1
TYPE_M_ME_NA_1 = 9
TYPE_M_ME_NB_1 = 11
TYPE_M_ME_NC_1 = 13
TYPE_M_SP_TB_1 = 30
TYPE_M_ME_TD_1 = 34
TYPE_M_ME_TE_1 = 35
TYPE_M_ME_TF_1 = 36
TYPE_C_IC_NA_1 = 100

COT_ACTIVATION = 0x06

STARTDT_ACT = bytes([0x68, 0x04, 0x07, 0x00, 0x00, 0x00])
TESTFR_CON = bytes([0x68, 0x04, 0x83, 0x00, 0x00, 0x00])


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
    length = recv_exact(sock, 1)[0]
    return start + bytes([length]) + recv_exact(sock, length)


def make_s_frame(rr: int) -> bytes:
    return bytes([0x68, 0x04, 0x01, 0x00, (rr << 1) & 0xFF, (rr >> 7) & 0xFF])


def make_gi_frame(ss: int, rr: int, ca: int) -> bytes:
    cf1 = (ss << 1) & 0xFF
    cf2 = (ss >> 7) & 0xFF
    cf3 = (rr << 1) & 0xFF
    cf4 = (rr >> 7) & 0xFF
    asdu = bytes([
        TYPE_C_IC_NA_1,
        0x01,
        COT_ACTIVATION,
        0x00,
        ca & 0xFF,
        (ca >> 8) & 0xFF,
        0x00,
        0x00,
        0x00,
        0x14,
    ])
    apdu = bytes([cf1, cf2, cf3, cf4]) + asdu
    return bytes([0x68, len(apdu)]) + apdu


def is_iframe(cf: bytes) -> bool:
    return (cf[0] & 0x01) == 0


def is_uframe(cf: bytes) -> bool:
    return (cf[0] & 0x03) == 0x03


def parse_send_seq(cf: bytes) -> int:
    return (cf[0] >> 1) | (cf[1] << 7)


def decode_cp56_time(raw: bytes) -> str:
    millis = raw[0] | (raw[1] << 8)
    minute = raw[2] & 0x3F
    hour = raw[3] & 0x1F
    day = raw[4] & 0x1F
    month = raw[5] & 0x0F
    year = 2000 + (raw[6] & 0x7F)
    second = millis // 1000
    millisecond = millis % 1000
    return f"{year:04d}-{month:02d}-{day:02d}T{hour:02d}:{minute:02d}:{second:02d}.{millisecond:03d}Z"


def decode_value(asdu_type: int, payload: bytes):
    if asdu_type in (TYPE_M_SP_NA_1, TYPE_M_SP_TB_1):
        return bool(payload[0] & 0x01), 1
    if asdu_type in (TYPE_M_ME_NA_1, TYPE_M_ME_TD_1):
        raw = struct.unpack_from("<h", payload, 0)[0]
        return raw / 32767.0, 3
    if asdu_type in (TYPE_M_ME_NB_1, TYPE_M_ME_TE_1):
        return struct.unpack_from("<h", payload, 0)[0], 3
    if asdu_type in (TYPE_M_ME_NC_1, TYPE_M_ME_TF_1):
        return struct.unpack_from("<f", payload, 0)[0], 5
    return None, len(payload)


def decode_asdu(asdu: bytes):
    if len(asdu) < 6:
        return []

    type_id = asdu[0]
    vsq = asdu[1]
    sq = bool(vsq & 0x80)
    count = vsq & 0x7F
    cot = asdu[2] & 0x3F
    ca = asdu[4] | (asdu[5] << 8)
    offset = 6
    ioa = 0
    decoded = []

    timed_types = {TYPE_M_SP_TB_1, TYPE_M_ME_TD_1, TYPE_M_ME_TE_1, TYPE_M_ME_TF_1}

    for index in range(count):
        if offset + 3 > len(asdu):
            break
        if index == 0 or not sq:
            ioa = asdu[offset] | (asdu[offset + 1] << 8) | (asdu[offset + 2] << 16)
            offset += 3
        else:
            ioa += 1

        value, consumed = decode_value(type_id, asdu[offset:])
        if value is None or offset + consumed > len(asdu):
            break

        qds = asdu[offset + consumed - 1]
        offset += consumed
        timestamp = None
        if type_id in timed_types:
            if offset + 7 > len(asdu):
                break
            timestamp = decode_cp56_time(asdu[offset : offset + 7])
            offset += 7

        decoded.append(
            {
                "ca": ca,
                "ioa": ioa,
                "type_id": type_id,
                "cot": cot,
                "value": value,
                "qds": qds,
                "timestamp": timestamp,
            }
        )

    return decoded


def main() -> None:
    sock = socket.create_connection((BRIDGE_HOST, BRIDGE_PORT), timeout=10)
    sock.settimeout(None)
    sock.sendall(STARTDT_ACT)
    apdu = recv_apdu(sock)
    if apdu[2:6][0] != 0x0B:
        raise RuntimeError(f"Expected STARTDT_CON, got {apdu[2:6].hex()}")

    ss = 0
    rr = 0
    last_gi = 0.0

    while True:
        now = time.monotonic()
        if GI_INTERVAL == 0 and last_gi == 0.0:
            for ca in GI_CAS:
                sock.sendall(make_gi_frame(ss, rr, ca))
                ss = (ss + 1) & 0x7FFF
            last_gi = now
        elif GI_INTERVAL > 0 and now - last_gi >= GI_INTERVAL:
            for ca in GI_CAS:
                sock.sendall(make_gi_frame(ss, rr, ca))
                ss = (ss + 1) & 0x7FFF
            last_gi = now

        apdu = recv_apdu(sock)
        cf = apdu[2:6]

        if is_uframe(cf):
            if cf[0] == 0x43:
                sock.sendall(TESTFR_CON)
            continue

        if not is_iframe(cf):
            continue

        rr = (parse_send_seq(cf) + 1) & 0x7FFF
        for message in decode_asdu(apdu[6:]):
            print(json.dumps(message), flush=True)

        # Ack every received I-frame so the bridge never waits on a partial
        # batch of unacknowledged GI responses.
        sock.sendall(make_s_frame(rr))


if __name__ == "__main__":
    main()
