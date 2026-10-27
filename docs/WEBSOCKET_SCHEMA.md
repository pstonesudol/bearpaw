# WebSocket Message Schema

Endpoint: `/ws`

The connection is **server-push only**. The backend does not read message
content from clients — there is no `subscribe`, `ping`/`pong`, or any other
client→server protocol. A client connects and receives the stream; every
message below is `type`-tagged and flows Server → Client.

Source of truth is what the code broadcasts:
[`ws.rs`](../crates/bearpaw-api/src/api/ws.rs),
[`poll.rs`](../crates/bearpaw-api/src/api/poll.rs),
[`memory_sync.rs`](../crates/bearpaw-api/src/api/memory_sync.rs). Where this doc
disagrees with the code, the code wins.

## Message types

The server broadcasts five `type`s: `state_update`, `event`, `progress`,
`device_info`, and `banks_update`.

### state_update

Partial `LiveState` diff — only the fields that changed since the last poll.
Emitted up to ~5×/sec.

```json
{
  "type": "state_update",
  "timestamp": 1704412800.123,
  "sequence": 12345,
  "data": {
    "frequency": 151.2500,
    "modulation": "FM",
    "squelch_open": true,
    "rssi": 75,
    "tone_squelch_kind": "ctcss",
    "tone_squelch": 123.0,
    "tone_dcs_code": null,
    "tone_dcs_label": null
  }
}
```

`sequence` is monotonically increasing; clients MUST drop any message whose
`sequence` is not greater than the last one seen (out-of-order guard).

The tone fields (`tone_squelch_kind`, `tone_squelch`, `tone_dcs_code`,
`tone_dcs_label`) are only populated while `squelch_open === true` (during a
hit); they are omitted or null otherwise.

### event

Event subtypes are carried under `type: "event"`, distinguished by `event`.

**`scan_hit`** — squelch just opened on a signal:

```json
{
  "type": "event",
  "timestamp": 1704412800.456,
  "event": "scan_hit",
  "data": {
    "frequency": 151.2500,
    "channel": 25,
    "alpha_tag": "Police Dispatch",
    "rssi": 75,
    "hit_uid": "1760000000000-3"
  }
}
```

`hit_uid` is allocated when the squelch opens and is the durable id written on
the hit row, the audio file name, and every transcript event below. A client
that wants to attach a later recording or transcript to the row it just rendered
must keep this value.

**`recording_ready`** — a hit's audio finished (or was discarded/failed):

```json
{
  "type": "event",
  "timestamp": 1760000005.2,
  "event": "recording_ready",
  "data": {
    "hit_uid": "1760000000000-3",
    "recording_status": "ready",
    "recording_duration": 4.9,
    "link_confidence": "exact",
    "recording_error": null
  }
}
```

`recording_status` is `ready`, `partial`, `too_short` or `failed`.
`link_confidence` is `exact` or `uncertain`; `uncertain` means the audio is real
but the scanner changed frequency or the device dropped out mid-transmission, so
the file may not be the whole transmission. Both fields are on the hit row too,
so a client that misses this event loses nothing permanent.

**`transcript_ready`** — a transcription job changed state:

```json
{
  "type": "event",
  "timestamp": 1760000010.0,
  "event": "transcript_ready",
  "data": {
    "hit_uid": "1760000000000-3",
    "transcript_status": "completed",
    "transcript": "Amtrak 215 clear at Zoo",
    "transcript_engine": "whisper-cli",
    "transcript_error": null
  }
}
```

Sent for `processing`, `completed` and `failed`. `transcript` is only present on
`completed`; `transcript_error` only on `failed`.

**`state_stale`** — the backend stopped receiving fresh polls (scanner went
quiet or disconnected):

```json
{
  "type": "event",
  "event": "state_stale",
  "timestamp": 1704412800.789
}
```

### progress

Long-running task updates (memory sync, import). Completion is signaled by a
`progress` message with `percent: 100`, not a separate `complete` message.

```json
{
  "type": "progress",
  "task_id": "sync-abc123",
  "percent": 45,
  "message": "Syncing channel 225/500"
}
```

### device_info

Full `DeviceInfo` snapshot, pushed when model / port / connection status
changes. Disconnects surface here (and via a `state_stale` event) rather than
through any `error` message.

```json
{
  "type": "device_info",
  "data": {
    "model": "BC125AT",
    "firmware": "1.00.05",
    "connection_status": "connected",
    "port": "/dev/ttyACM0",
    "capabilities": {
      "channel_count": 500,
      "channels_per_bank": 50,
      "bank_count": 10,
      "has_alpha_tags": true,
      "has_per_channel_modulation": true,
      "has_tone_squelch": true,
      "has_backlight_control": true,
      "valid_delays": [-10, -5, 0, 1, 2, 3, 4, 5],
      "cleared_delay": 2,
      "default_baud": 115200
    }
  }
}
```

`capabilities` describes the connected scanner's memory model and feature set.
It arrives with the first `device_info` at connect, ahead of memory sync, so
capability-dependent UI can render correctly on first paint.

See `DeviceInfo` in [`API_SPEC.md`](API_SPEC.md#23-deviceinfo) for the full field
set.

### banks_update

The bank-enable mask changed server-side; the UI mirrors it instead of holding
a stale local copy. `banks` is a 10-element boolean array (index 0 = bank 1).

```json
{
  "type": "banks_update",
  "timestamp": 1704412800.123,
  "data": {
    "banks": [true, true, false, true, true, true, true, true, true, true]
  }
}
```
