# Architecture

`vcam` has three pieces:

```
+-----------+     Unix socket      +-----------------------+
|  vcam     | <-----------------> |                       |
|  (CLI)    |     JSON lines      |        vcamd          |
+-----------+                     |   (control thread)    |
                                  |                       |
+-----------+                     |  decoder  ->  output   |
| vcam-gui  | <-----------------> |  pipeline     pipeline |
| (GTK4)    |                     +-----------+-----------+
+-----------+                                 |
                                              | writes frames
                                              v
                                   /dev/video10 (v4l2loopback)
                                              ^
                                              | reads frames
                                   +----------+-----------+
                                   | Chrome, Zoom, OBS,   |
                                   | OpenCV, vcam-gui     |
                                   +----------------------+
```

## Components

- **`vcamd`** owns everything. It holds the loopback device open for its whole
  lifetime and runs the GStreamer pipelines.
- **`vcam`** (CLI) and **`vcam-gui`** are pure clients. They connect to the
  daemon socket, send one request per connection and read the response. The GUI
  additionally keeps one long-lived connection open to receive status events.

## Threading model

`vcamd` is a Tokio application, but the media pipelines run on a dedicated
control thread:

- the **Tokio runtime** runs the Unix socket server and signal handling;
- the **controller thread** owns the GStreamer objects, drains decoder frames,
  pushes output frames, and applies commands. Commands arrive over an unbounded
  channel, and each one carries a `oneshot` reply channel.

This keeps all GStreamer calls on a single thread and avoids sharing
non-`Send` pipeline objects across tasks.

## The output pipeline

The output pipeline is created once and kept alive:

```
appsrc name=vcam-source  ->  capsfilter  ->  queue  ->  v4l2sink device=/dev/video10
```

The controller pushes one frame every `1/fps` seconds, regardless of playback
state. When nothing is playing it pushes a black placeholder, so the device is
always streaming. This solves the `exclusive_caps=1` enumeration race: the
device only switches to *capture* mode after the writer completes `STREAMON`,
and because the writer never stops, applications always see a capture device.

### Why the caps spell everything out

`v4l2sink` constrains its sink caps with `pixel-aspect-ratio=1/1`,
`interlace-mode` and `colorimetry`. A plain fixed caps structure such as

```
video/x-raw,format=NV12,width=1280,height=720,framerate=30/1
```

leaves those fields open, which makes it *broader* than the sink caps. Caps
negotiation therefore fails with `not-negotiated`, and `appsrc` stops its
streaming task. The daemon adds the missing fields explicitly:

```
video/x-raw,format=NV12,width=1280,height=720,framerate=30/1,
pixel-aspect-ratio=1/1,interlace-mode=progressive,colorimetry=bt709
```

`colorimetry` is `bt709` for heights of 720 and above and `bt601` for smaller
frames. Both are accepted by `v4l2sink` and give downstream consumers sensible
colour metadata.

## The decoder pipeline

Each source builds a pipeline that produces tightly packed frames in the output
format:

```
filesrc -> decodebin3 -> videoconvert -> videoscale -> videorate -> capsfilter -> appsink
```

- `appsink` uses `sync=true`, so frames become available at their presentation
  time. This is what makes playback run at real time rather than as fast as the
  file can be decoded.
- The controller calls `try_pull_sample` once per output tick and keeps only the
  newest frame, which keeps latency low.
- The frame's PTS is used as the reported playback position. The pipeline
  position query returns zero while paused on this `decodebin3` pipeline, so the
  position is derived from the frame that is actually shown.

### Seek

Seeks use `FLUSH | ACCURATE`. `KEY_UNIT` was rejected because many camera and
phone clips only contain a keyframe at the beginning, which makes a key-unit
seek snap back to zero.

Seeking while paused is handled with a short preview: the decoder is set to
`PLAYING`, the controller waits until a frame at or after the target arrives
(or a deadline expires), then pauses again. The visible status stays `Paused`
the whole time.

### Looping and end of stream

`decodebin3` posts an `EOS` message at the end of a file. If looping is enabled
the controller seeks back to zero and keeps playing; otherwise it stops the
decoder, reports `playback finished`, and shows the black placeholder.

## IPC protocol

The socket lives at `$XDG_RUNTIME_DIR/vcam.sock` (configurable). Messages are
newline-delimited JSON.

Request:

```json
{"id": 1, "command": "start", "source": "/home/you/movie.mp4"}
```

Response:

```json
{"type": "response", "id": 1, "ok": true, "data": { ... }}
```

Events are pushed to every connected client, including a `status` event roughly
every 500 ms:

```json
{"type": "event", "event": "status", "data": { ... }}
```

The GUI relies on these events for its live state, and opens a fresh connection
for each command so that a slow command can never block the event stream.

## Preview

The GUI does not open the camera. While another application (Discord, Chrome,
Zoom, ...) is already streaming the loopback device, v4l2loopback rejects
`VIDIOC_REQBUFS` with `EBUSY`, so a reader that starts afterwards never
attaches — the preview would stay empty whenever an application is using the
camera. Instead the daemon downscales every output frame to a small RGB image
(at most 640 px wide, about 10 fps) and publishes it atomically in
`$XDG_RUNTIME_DIR/vcam-preview.raw`, next to the control socket. The GUI polls
that file and renders it as a `gdk::MemoryTexture`. The preview therefore keeps
working no matter which applications use the camera, and it always matches what
the device is streaming (including the black placeholder while idle).

## Camera users

v4l2loopback has no API to ask who is reading the device, so the daemon scans
`/proc`: for each process (same user only, which is all a user daemon can see) it
follows `/proc/<pid>/fd/*` symlinks looking for the device node, and reads the
access mode (`O_RDONLY` / `O_WRONLY` / `O_RDWR`) from `/proc/<pid>/fdinfo/<fd>`.
The scan runs about once a second and the result is published in `Status`, listed
by `vcam consumers`, and shown in the GUI.

The scan matters because of a device quirk: while one application is streaming
the loopback device, a *new* opener is rejected with `EBUSY` at `VIDIOC_REQBUFS`.
Worse, renegotiating the device caps while an application holds it can wedge it.
The daemon therefore treats a non-empty consumer list as a lock:

- A change that renegotiates the device caps (resolution, frame rate, pixel
  format) is **deferred**: it is stored as a pending change and applied once the
  list becomes empty. `--force` bypasses this.
- A change that only affects decoding (scaling, mirroring, rotation) is applied
  immediately, because the decoder can be rebuilt without touching the output
  pipeline. The caps negotiated with the device stay identical, so the device is
  never disturbed.

`apply_output` rebuilds the output pipeline only when the caps actually change;
otherwise it keeps the device open and just restarts the decoder.

## Profiles

A `[[profiles]]` entry is a partial output configuration plus a list of match
patterns. When `daemon.auto_profiles` is on, each time the consumer list changes
the daemon picks the first profile whose pattern matches a consumer's executable
name, resolves it against the base `[output]`, and applies the result — deferred
if it would change the caps. When no profile matches it restores the base output.
An explicit `vcam profile use` pins a profile and turns automatic selection off
until `vcam profile auto on`.

## Watchdog and self-healing

A small state machine runs every 250 ms on the controller thread and evaluates
cheap signals:

- the current level of the output `appsrc` (a level pinned at the watermark means
  the device stopped draining);
- the number of consecutive failed frame pushes;
- the time since the last successful push while playing;
- the pipeline bus, for fatal errors such as the device disappearing;
- the resident set size from `/proc/self/statm`, both an absolute cap and growth
  over a 45 s window.

If a signal trips, the watchdog asks the controller to rebuild the output
pipeline. Rebuilds are rate-limited with exponential backoff (500 ms doubling to
30 s), and after `max_recoveries` failed attempts the daemon logs an error and
exits, so the user service (which has `Restart=on-failure`) brings it back with a
clean process. The failure counters reset after a period of clean operation.

The output pipeline itself is non-blocking by construction: `appsrc` runs with
`block=false` and `leaky-type=downstream`, and the queue leaks downstream, so a
stalled device drops frames instead of growing an unbounded queue. The watchdog
detects and repairs the stall rather than letting memory grow.
