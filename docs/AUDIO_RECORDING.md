# Audio Recording & Transcription

Bearpaw can record the audio of every scanner hit and transcribe it locally.
This document is the design, the setup guide, and the honest list of what the
feature cannot do.

> **Status:** the capture, storage, transcription and UI code ships together.
> Nothing in this document has been verified against a BC125AT and a Cubilux
> adapter yet — see [Hardware test plan](#hardware-test-plan). Everything below
> that touches real hardware is a plan, not a measurement.

## Why it is built this way

### A second receiver would be a different product

The BC125AT has **one** receiver. It cannot listen to two channels at once, and
Bearpaw cannot make it. So the feature models exactly what the hardware does:
one continuous audio stream, and one transmission at a time, delimited by the
scanner's own squelch transitions.

Squelch state comes from `STS`/`GLG` polling at **5 Hz**. That is the accuracy
floor for every timestamp in this feature. A transmission that opens and closes
inside one 200 ms poll window may be missed entirely (this is why
`hit_min_duration` exists), and a recording can start up to 200 ms after the
squelch actually opened. The pre-roll buffer exists to hide that delay, not to
pretend it does not exist.

### Audio capture lives in the Rust backend

The frontend is replaceable; the backend owns state and hardware. Recording in
the browser would stop when the tab is hidden, tie the audio lifetime to a React
component, and make "did we capture this hit?" depend on the UI. Capture is
therefore a backend service, like the poll loop.

### Transcription shells out to `whisper-cli`

`whisper.cpp` is the right engine for an offline Apple Silicon app: quantised
models run faster than real time on CPU and use the GPU through Metal when the
build supports it. What is *not* right is linking a C++ build into the Tauri
sidecar on four platforms. Bearpaw instead runs a user-installed `whisper-cli`
against a user-installed GGML model.

That keeps the Rust build dependency-free, makes a missing model a clean,
recoverable error instead of a startup failure, and lets a user upgrade the
engine without waiting for a Bearpaw release. The cost is one install step
(below) and no bundled model.

Transcripts are **not authoritative**. Radio audio is narrowband, often clipped,
and full of proper nouns a general model has never seen. Treat every transcript
as a hint. Editing one stores the machine text separately
(`transcript_original`), so a correction never destroys what the engine said.

## Hardware

```
 BC125AT headphone jack (3.5 mm)
            │
            │  3.5 mm stereo cable
            ▼
 Cubilux USB-C audio capture adapter  ──USB-C──▶  Mac (Apple Silicon)
            │                                          │
            │  appears as a CoreAudio input            │
            ▼                                          ▼
      Bearpaw audio capture  ◀──────────────  CoreAudio / cpal
                                                       │
 BC125AT USB (data + control) ──USB-A/C──▶  Bearpaw scanner transport
```

The two connections are independent. Unplugging the audio adapter does not stop
the scanner, and unplugging the scanner does not stop audio capture. Bearpaw
treats them as two independent failures, which is also how it must be tested.

**Cabling:** the scanner's headphone output is stereo but the radio is mono. A
mono-to-stereo 3.5 mm cable works; a stereo cable also works because Bearpaw
downmixes to mono. Do **not** use the scanner's speaker output (there isn't
one) or a cable with a built-in attenuator unless the adapter's input clips.

## macOS setup

1. **Connect the adapter** and select it in *System Settings → Sound → Input*.
   Confirm the level meter moves when the scanner receives a signal.
2. **Grant microphone access.** The first capture attempt triggers macOS's
   prompt. Bearpaw's bundle declares `NSMicrophoneUsageDescription`; if the
   prompt was dismissed, re-enable Bearpaw under *System Settings → Privacy &
   Security → Microphone*.
3. **Running the backend from a terminal** (`cargo run -p bearpaw-api`) makes
   *Terminal* (or iTerm) the responsible process, not Bearpaw. Grant that app
   microphone access, or the stream opens and delivers silence — macOS does not
   always return an error for a denied permission.
4. **Avoid Aggregate/BlackHole devices** for the scanner input. They work, but
   their clock drift makes hit alignment worse than a direct USB input.

## Transcription install

```bash
brew install whisper-cpp          # provides whisper-cli
mkdir -p "$HOME/Library/Application Support/Bearpaw/models"
curl -L -o "$HOME/Library/Application Support/Bearpaw/models/ggml-base.en.bin" \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
```

`base.en` is the default recommendation: roughly 150 MB, comfortably faster than
real time on an M-series CPU, and noticeably better than `tiny.en` on radio
audio. `small.en` is more accurate and about three times slower. Quantised
(`q5_1`) models are a good trade if disk or CPU is tight.

Bearpaw finds `whisper-cli` on `PATH` and in `/opt/homebrew/bin` and
`/usr/local/bin`. Both the binary and the model can be overridden in
*Device → Audio & Transcription*. Models in the app-data `models/` directory
are listed by name.

### Vocabulary

Radio traffic is full of local proper nouns. The **Vocabulary** field is passed
to whisper as an initial prompt, biased toward spelling those terms correctly:

> SEPTA, Norfolk Southern, Amtrak, NORAC, Harrisburg Line, Keystone Corridor,
> Zoo Interlocking, Phoenix, Paoli, Thorndale, Norristown, Powelton Yard, clear,
> approach, restricted speed, dispatcher, maintenance of way

The prompt is truncated to roughly 400 characters because whisper's prompt
window is much smaller than a full glossary. A long list does not help; a short
list of the words that actually appear does.

## Storage

```
<app data>/recordings/<hit-uid>.wav     audio, 16 kHz mono 16-bit PCM
<app data>/models/*.bin                 whisper models
```

The database stores a **relative** filename, not an absolute path. The
recordings root is a preference, so moving the library is a copy plus a setting:
old references keep working as long as the files move with the root. If the root
changes without the files, rows are marked `missing` — the hit and its transcript
stay, and the UI says the audio is gone rather than silently showing nothing.

Each file is written to `<uid>.wav.part` and renamed only after the WAV trailer
is finalized. A crash leaves `.part` files, which are swept on startup and never
mistaken for a recording.

## Recording semantics

| Setting | Default | Meaning |
| --- | --- | --- |
| Enable automatic recording | off | Master switch |
| Input device | system default | CoreAudio input by name |
| Pre-roll | 500 ms | Audio kept from before the squelch opened |
| Post-roll | 750 ms | Audio kept after it closed |
| Minimum recording | 500 ms | Shorter segments are discarded |
| Retention | 30 days | Audio older than this is deleted; hits stay |

* A **rolling buffer** holds the pre-roll continuously while monitoring is on.
  It is bounded by `pre_roll × sample_rate` samples, so memory does not grow.
* The **hit start** is the backend's `scan_hit` transition, which is the same
  event the Recent Hits list uses. Audio and the hit row cannot disagree about
  *whether* a hit happened.
* A **new hit while the previous post-roll is still running** finishes the
  previous file immediately. Its post-roll is truncated; nothing else is lost.
  This is the normal shape of back-to-back radio traffic.
* A **hit that changes frequency without a squelch close** cannot be split by a
  one-receiver scanner. The segment is finished as `partial` with
  `link_confidence = uncertain` rather than silently assigned to one frequency.
* **Device loss** finishes the active segment as `partial`. The scanner keeps
  scanning; only recording is affected.
* **No audio** (device denied, unplugged before the hit, or recording off)
  leaves the hit row with `recording_status = none`. A segment that *started*
  and then lost the device is kept as `partial`; if it finishes before the
  hit row exists (the squelch was still open), the outcome is held until the
  close handler reports whether a row was written, so the bytes are never
  deleted and the row is never left claiming `recording`.

## Transcription semantics

| Status | Meaning |
| --- | --- |
| `none` | Transcription not requested (disabled, or no audio) |
| `pending` | Queued |
| `processing` | Running |
| `completed` | Text stored |
| `failed` | Error stored; retry is available |

* Jobs run **one at a time**, off the poll thread. A slow model cannot delay
  `STS` polling or a command.
* **Interrupted jobs are requeued on restart.** Any `processing` row found at
  startup becomes `pending` again; nothing is left stuck.
* A **missing binary or model** fails the job with a named error
  (`transcription_binary_missing`, `transcription_model_missing`) and an
  actionable message. It never retries in a loop.
* **Audio is never uploaded.** The engine is a local process with a local file.
  Bearpaw has no network transcription path.
* **Editing** stores the machine output in `transcript_original` the first time
  a user edits, so retranscription is always possible.

## Resource cost

Measured nowhere yet (no hardware in the development environment); these are
engineering estimates to be replaced with measurements from the hardware test
plan:

| Item | Estimate |
| --- | --- |
| Audio capture CPU | < 2% of one M-series core at 48 kHz |
| Audio storage | 32 KB/s at 16 kHz mono 16-bit → ~2.7 MB/hour of *audio*, not wall time |
| Pre-roll memory | `pre_roll × 16000 × 4` bytes (500 ms ≈ 32 KB) |
| Transcription | `base.en` roughly 0.2–0.5× real time on an M1 CPU |
| Model disk | 150 MB (`base.en`), 500 MB (`small.en`) |

## Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| Recording indicator never appears | Recording disabled, or no hit longer than the minimum | Enable it; lower *minimum recording* |
| `audio_device_not_found` | Adapter unplugged or renamed | Re-select the device; capture retries every 3 s |
| Files are silent | macOS mic permission denied for the responsible app | Privacy & Security → Microphone |
| `transcription_binary_missing` | `whisper-cli` not installed or not on `PATH` | `brew install whisper-cpp`, or set the path |
| `transcription_model_missing` | No `.bin` in the model directory | Point the model selector at a GGML file |
| `engine_failed` with a load error | Model is not a whisper.cpp GGML file | Re-download the model |
| Transcript is nonsense | Noise, clipping, or unusual proper nouns | Edit it; add the words to the vocabulary |
| Audio out of sync at the start | Pre-roll shorter than the poll delay | Raise pre-roll above 500 ms |

## Limitations

1. **One receiver.** No simultaneous recordings, ever.
2. **5 Hz alignment.** Hit boundaries are within one poll interval, not
   sample-accurate. The pre-roll mitigates the leading edge; the trailing edge
   is a poll interval late.
3. **macOS-first.** Capture uses `cpal`, so it compiles on Linux and Windows,
   but only macOS has been designed for and documented. Linux needs ALSA and a
   device selection that has not been exercised; Windows likewise.
4. **External engine.** Bearpaw does not bundle whisper.cpp or a model. No
   install means no transcription (recording still works).
5. **No diarisation or channel separation.** One audio stream, one transcript.
6. **Transcripts are not evidence.** Do not use them for anything where an
   error matters without listening to the audio.

## Hardware test plan

Not yet executed. Run this on the Mac with the BC125AT and Cubilux attached and
record the outcome in the PR.

1. **Device discovery** — adapter appears in the settings selector with its
   CoreAudio name; selecting it persists across a restart.
2. **Permission** — first test recording triggers the macOS prompt; denying it
   produces a visible error, not a silent empty file.
3. **Level** — scanning Norfolk Southern produces a moving input level; idle
   squelch sits near the noise floor.
4. **One transmission** — a single clear transmission produces one hit row, one
   `.wav`, and audible speech at the start (pre-roll working).
5. **Back-to-back** — three replies in under two seconds produce three files,
   none truncated mid-word by more than the configured post-roll.
6. **Short burst** — a sub-500 ms kerchunk is discarded as `too_short` (or kept
   if the minimum is lowered); the hit row remains.
7. **Scanner unplug** — yanking the scanner mid-hit leaves a `partial` file and
   a hit row; scan control recovers when replugged.
8. **Audio unplug** — yanking the adapter mid-hit leaves a `partial` file, the
   scanner keeps scanning, and replugging resumes capture within ~3 s.
9. **Transcription** — the file transcribes offline; `whisper-cli` shows in
   Activity Monitor and no network traffic is generated.
10. **Restart** — hit, audio, and transcript survive a full quit and relaunch;
    a job killed mid-flight is `pending` and completes after relaunch.
11. **Retention** — set retention to 1 day, age a file past it (or delete one
    recording by hand) and confirm the audio goes while the hit and transcript
    remain. `0` means "keep until the hit ages out", not "delete now".
12. **CPU** — record a 10-minute monitoring session and note CPU and disk in
    Activity Monitor.
