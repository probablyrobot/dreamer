# Images

Dreamer shows images that programs send with the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).
Ghostty interprets the protocol and keeps the images. Dreamer reads them
through the public render-state and graphics interfaces of the pinned
release.

## What is supported

| Feature | State |
| --- | --- |
| Inline transmission (`t=d`) of raw RGB, RGBA, gray, or gray+alpha pixels | Supported (RGB and RGBA are covered by tests) |
| zlib-compressed payloads (`o=z`) | Inflated by Ghostty; not covered by Dreamer's tests yet |
| PNG (`f=100`) | Decoded by Dreamer's callback (below) |
| File, temporary-file, and shared-memory transmission (`t=f`, `t=t`, `t=s`) | Refused on every engine |
| Placements: position, cell offset, size, source crop, z-index and layer | In frames |
| Delete and replace | Supported |
| Virtual (Unicode placeholder) and relative placements | Read and drawn like other placements; not covered by fixtures yet |
| Animation | Frames show the current animation frame; playback is not driven yet |

File and shared-memory transmissions name something on the daemon's host.
A client can't read that, so every engine refuses them: daemon mirrors and
window replicas alike.

## PNG decoding

Ghostty asks the embedder to decode PNGs. Dreamer's callback uses the
pure-Rust `png` crate:
- **Registration:** registered once per process, before any engine is
  created or the library is queried on any thread.
- **Size checks:** the declared size is checked (at most 64 Mi pixels)
  before the pixel buffer is allocated, and decoding is capped at 320 MiB
  of working memory.
- **Output:** written into memory from Ghostty's allocator and handed over
  only on success.
- **Failures:** malformed, truncated, and oversized PNGs are rejected and
  the program's other output carries on. A panic inside the decoder
  rejects the image; it never unwinds into C.

## Limits

- **Storage:** each engine keeps at most 64 MiB of images per pane.
  Older images are evicted first, as Kitty does.
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

## Drawing

The renderer paints each frame in this order:
1. the default background;
2. images below cell backgrounds;
3. cell backgrounds;
4. images below text;
5. the selection;
6. text;
7. images above text;
8. the cursor.

Each placement is clipped to the grid. Pixel values are scaled from
canonical cells to the view's cells. A source crop is drawn by placing the
whole image so the cropped region lands on the destination, then clipping
to the destination. One texture per image serves every crop of it.

**Texture cache.** GPU-ready images are kept per window, keyed by engine
instance, image ID, and generation:
- **Reuse:** an unchanged image is never converted or uploaded again.
  `PaintStats.image_uploads` counts uploads, and the repaint smoke requires
  zero on a repaint.
- **Drop:** an image the current frame no longer uses is dropped after the
  paint and its atlas tiles are released. That covers deleted and replaced
  images, and everything from a replaced engine. A restored checkpoint
  is a new engine instance, so it never reuses the old textures.
- **Size:** the cache holds only what the visible frame uses, so it is
  bounded by the engines' image storage (64 MiB per pane).

**Fixtures.** `crates/terminal-render/fixtures` has these image fixtures:
`image-place`, `image-crop`, `image-delete`, `image-clip`, and
`image-below-text`. `expected.txt` lists the images drawn and clipped for
each. The macOS gate's matrix and `gate linux-desktop` both check them.
