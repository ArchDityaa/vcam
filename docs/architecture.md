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
                     \-> audioconvert -> audioresample -> capsfilter -> queue -> volume -> pulsesink
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

## The virtual microphone

With `[audio] enabled` (the default), `vcamd` creates two session modules with
`pactl` while it starts:

```
module-null-sink    sink_name=vcam_sink  sink_properties=device.description="Virtual Microphone"
module-remap-source master=vcam_sink.monitor source_name=vcam_mic
                    source_properties=device.description="Virtual Microphone"
```

Applications record from `vcam_mic`; the daemon plays into `vcam_sink` and the
remapped source forwards the sink's monitor. On shutdown the source is unloaded
before the sink, because the reverse order would leave a source pointing at a
monitor that no longer exists. Modules with the configured names are *adopted*
rather than created again, so a crash followed by `Restart=on-failure`, or a
repeated `vcam audio on`, never stacks `vcam_sink.2`, `vcam_sink.3`, ... copies.

The audio branch is part of the decoder pipeline and is attached lazily, when
`decodebin3` exposes the first audio pad:

```
decodebin3 -> audioconvert -> audioresample -> capsfilter (S16LE, rate, channels)
           -> queue -> volume -> pulsesink device=vcam_sink
```

Laziness is not cosmetic: a sink that never receives a buffer never receives
`EOS` either, so keeping an unlinked audio sink in the pipeline would make the
pipeline wait for an EOS that never comes and break looping for video-only
sources. The branch therefore exists only when the file actually has audio.

`pulsesink` runs with `provide-clock=false`. Adding audio must not switch the
pipeline clock, because the video `appsink` paces frames against that clock;
`sync=true` aligns audio to the same clock and the queue absorbs device jitter.
Since the branch belongs to the decoder, pause, seek, loop and stop apply to
sound and video together. The one special case is the paused-seek preview: the
controller briefly runs the pipeline to render a frame and mutes the `volume`
element for that window, so seeking while paused is not audible.

Audio failures are isolated from the camera. If the sink cannot be created, or
errors while streaming, the controller drops the audio branch, rebuilds the
decoder without it and keeps the device running; the reason is published in
`Status::audio.message` and shown by `vcam status` and the GUI. A missing
`pactl` or sound server at startup is treated the same way: the modules are not
created, the camera starts normally, and `vcam audio status` explains why. The
`vcam doctor` checks for the backend and the elements are informational, so a
machine without audio does not look like a broken camera.

`vcam-core::audio` also contains the module-argument parser and the description
escaper. The escaper is needed because PipeWire's PulseAudio compatibility
server splits module arguments on spaces and does not honour plain quotes:
`Virtual Microphone` would be stored as `Virtual`. Escaping spaces and quotes
is why `pactl list short modules` shows
`device.description="Virtual\ Microphone"`.

## IPC protocol

The socket lives at `$XDG_RUNTIME_DIR/vcam.sock` (configurable). Messages are
newline-delimited JSON.

Request:

```json
{"id": 1, "command": "start", "source": "/home/you/movie.mp4"}
```

The daemon accepts `start`, `stop`, `pause`, `resume`, `seek`, `set_source`,
`set_loop`, `set_output`, `set_profile`, `set_profile_auto`,
`set_compatibility`, `set_audio`, `status`, `profiles`, `doctor` and
`shutdown`. Toggling the virtual microphone uses the same envelope:

```json
{"id": 2, "command": "set_audio", "enabled": false}
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

## Chromium/Electron compatibility

Profiles react to who is *holding* the camera, which is too late for the problem
they are most needed for: an application that never lists the camera cannot
start holding it. Chromium-based applications (Discord, Chrome, Brave, Slack,
Signal, ...) do not enumerate V4L2 devices through GStreamer, and they
routinely fail to list a loopback device that only advertises `NV12`; the same
device advertising `YUYV` appears immediately.

The daemon therefore also scans the process table — `/proc/<pid>/comm`, one
small file per process, same user only, every two seconds, deliberately cheaper
than the consumer scan. When a Chromium-family process appears, the output
switches to `YUYV`; when the last one exits, the configured pixel format comes
back. This runs through the same `apply_output` path as everything else, so the
change is still deferred while an application is streaming.

Precedence in `desired_output`: an explicitly pinned profile wins, then an
automatic profile matching a current consumer, then the Chromium shim, then the
base `[output]`. The shim is on by default (`[compatibility] chromium_yuyv`)
because the failure it prevents is silent, and is toggled at runtime with
`vcam compat on|off`. The reason for the current format is published in
`Status::compatibility`, so `vcam status` and the GUI show *why* the format
differs from the configuration instead of leaving the user to guess.

## Device access

`vcam-core::access` answers "can this process open the device, and if not, why"
without privileges, and it distinguishes a reason from a symptom:

- `Missing` — the node does not exist (module not loaded);
- `NotInGroup` — the account is not in the group that owns the node;
- `StaleSession` — the account *is* in the group per `/etc/group`, but the
  process's own supplementary groups, read from `/proc/self/status`, do not
  contain it. The session started before `usermod -aG` ran. `id` in a new shell
  will disagree, which is exactly why this costs people hours;
- `Denied` — mode or owner, e.g. `root:root 0600`.

`vcam doctor` reports the classification plus the commands that fix it, and
`vcamd` refuses to start with that explanation rather than letting GStreamer
fail with `Element failed to change its state`, which is what a permission
problem looks like from the pipeline's point of view. The same diagnosis is
appended to output pipeline errors if access is lost while running.

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
