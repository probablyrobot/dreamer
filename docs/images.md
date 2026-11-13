# Images

Dreamer shows images that programs send with the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Ghostty interprets the protocol and keeps the images. Dreamer reads them
through the public render-state and graphics interfaces of the pinned
release (spec 9.4).

## What is supported

| Feature | State |
| --- | --- |
| Inline transmission (`t=d`) of raw RGB, RGBA, gray, or gray+alpha pixels | Supported (RGB and RGBA are covered by tests) |
| zlib-compressed payloads (`o=z`) | Inflated by Ghostty; not covered by Dreamer's tests yet |
| PNG (`f=100`) | Needs the PNG decoder (#130); until then PNG images are not stored |
| File, temporary-file, and shared-memory transmission (`t=f`, `t=t`, `t=s`) | Refused on every engine |
| Placements: position, cell offset, size, source crop, z-index and layer | In frames |
| Delete and replace | Supported |
| Virtual (Unicode placeholder) and relative placements | Placements are read; drawing is #131 |
| Animation | Frames show the current animation frame; playback is not driven yet |

File and shared-memory transmissions name something on the daemon's host.
A client can't read that, so every engine refuses them: daemon mirrors and
window replicas alike (spec 11.7).

## Limits

- **Storage:** each engine keeps at most 64 MiB of images, the spec's
  starting budget per pane. Older images are evicted first, as Kitty does.
- **Same limit everywhere:** eviction changes what a pane shows, so daemon
  mirrors and window replicas apply the same limit. Snapshots don't carry
  it, so it is applied again on restore. The limit is part of the engine
  build family's policy revision (`p2`), so peers configured differently
  never exchange snapshots.
- **Image size:** an image over 64 Mi pixels is not copied out of the
  engine. Neither is one whose data doesn't match its reported size and
  format. Its placements are skipped and counted
  (`Engine::images_skipped`).

## In frames

Each frame carries the viewport's image placements, ordered by layer and
then z-index:

| Layer | Kitty z-index |
| --- | --- |
| Below cell backgrounds | Below -1,073,741,824 |
| Below text | Negative |
| Above text | Zero or more |

Pixels are copied out of the engine once per image ID and generation, as
straight RGBA, and shared between frames. While Ghostty reports the same
placements generation, a frame shares the previous frame's placement
list.

Pixel values (offsets and draw size) are in canonical cell pixels, 10×20
per cell. A view scales them to its own cell size.

Image generations are local to the process that made them. A replica
restored from a checkpoint has the same images and placements as the
mirror, but different generation numbers.
