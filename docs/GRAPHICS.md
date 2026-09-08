# Images in Ködade CLI

Images emitted by a pane stay with its terminal screen. The daemon retains
image data, clients fetch each revision separately, and placements are clipped
to pane interiors. Opening an overlay hides images; closing it restores them
without uploading the same pixels again.

Rendering turns on automatically in Kitty and Ghostty. Set
`KODADE_GRAPHICS=kitty` for another terminal configured to support the Kitty
graphics protocol, or `KODADE_GRAPHICS=off` to disable it. Other terminals show
an image hint. Image paste works independently of rendering.

## Paste an image

Press `prefix I` or choose **paste clipboard image** from the command center.
Ködade uploads the PNG to the selected pane's daemon and inserts a quoted path
without pressing Enter. Over SSH, the agent receives a path on its own machine.

```sh
kodade-cli pane paste-image 3 ./screenshot.png
kodade-cli --remote buildbox pane paste-image 3 ./screenshot.png
kodade-cli pane paste-image 3  # host clipboard
```

Clipboard readers are `wl-paste` on Wayland, `xclip` on X11, and `pngpaste` on
macOS. Missing readers produce an actionable message; specifying a file needs
none of them. Paths must name regular files. PNG pixels are validated as well
as headers. Each session retains at most 64 attachments / 64 MiB in a private
temporary directory, with files mode `0600`. Orderly shutdown removes them.
A forced process kill can leave them for the OS's temporary-file cleanup.

## Protocol support and limits

Supported Kitty actions include RGB (`f=24`), RGBA (`f=32`), and PNG (`f=100`)
transmission (`a=t`), transmit and place (`a=T`), place (`a=p`), query (`a=q`),
and deletion (`a=d`). Explicit image IDs (`i`) and terminal-assigned image IDs
requested through an image number (`I`) are supported; repeated numbers identify
the newest transmission. Image revisions, chunking, quiet responses, cell-sized
placements, source cropping, z order, and cursor movement policy are retained.
Automatic sizes use an 8×16 cell-pixel estimate; an explicit single cell
dimension preserves the crop aspect ratio. Images follow full-screen scrolling
and scrolling margins. Clear screen and alternate-buffer entry clear the
appropriate placements.

Send large transfers in Kitty's 4096-byte base64 chunks. Limits are 8 MiB
decoded per image, 16 megapixels / 64 MiB decoded PNG pixels, 16 stored images
and 64 placement definitions per pane, 65,536 retained virtual cells, and
32 MiB image data per pane. Host caches have image and byte limits too. Layouts
carry placement metadata; pixel data travels through `Query(Image)`. Daemon JSON
requests are capped at 16 MiB before deserialization, while partial uploads
survive concurrent screen updates.

Images can arrive directly, through ordinary files or protocol temporary files,
or through POSIX shared memory on Unix daemons. File ranges use `O/S`; symlinks
are followed, special files are rejected, and only named protocol files inside
temporary directories are deleted. POSIX shared memory is unlinked after
opening. All sources stay on the pane's daemon machine; remote clients receive
normalized pixel data. Zlib (`o=z`) compression is bounded before image
validation. Both compressed and uncompressed data count against the per-image
limits.

Virtual Unicode placements, relative placements (up to eight parents), pixel
placement offsets, and visible-cell, column, row, z-index, image-range,
image-number (`d=n/N`) and image-id delete selectors are retained for local and
remote clients. Uppercase deletion removes only the selected, unused image data;
unrelated transmitted images remain available. On resize, visible virtual cells
follow the terminal grid and clipped cells are discarded. Hidden-buffer virtual
cells need an application redraw after resize; their image data remains
available. Animation is unsupported and returns an error. This remains bounded
Kitty support, not the full protocol.

## Verification

Kitty 0.48.2 on Linux arm64 was exercised in a real graphical terminal window.
`scripts/graphics-media-smoke.py` verifies exact pixels and ownership cleanup
for direct, zlib, file/range, temporary-file, and POSIX shared-memory transfers.
`scripts/graphics-smoke-test.py` also uses a real daemon and controlling PTY to
check image emission/fetch, modal hide/restore, resize, detach cleanup, PNG
paste, and session attachment cleanup. The isolated localhost OpenSSH smoke also
verifies PNG upload, image metadata, rename, and attachment cleanup through real
forwarded sockets. Unit tests cover chunk boundaries, failed transfers, quotas,
atomic replacement, scrolling, clipping, PNG validation, FIFO rejection, and
interrupted partial socket reads. Ghostty support follows its documented Kitty
implementation; it has not been visually tested in this environment. Final
v0.3.0 release-run evidence remains pending.

References: [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
and [Ghostty graphics support](https://ghostty.org/docs/features).
