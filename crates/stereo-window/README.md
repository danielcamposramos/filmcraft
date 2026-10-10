# filmcraft-stereo-window

The Stereo Monitor window: a native window that shows a stereo pair and declares it to the
desktop. Layer L5, safe Rust, MIT OR Apache-2.0. Used by the desktop app only (`View ▸ Stereo
Monitor`, `docs/monitors.md`).

**One format, declared once.** FilmCraft delivers a stereo picture as full side by side, left eye
first, each eye at full size. The window tells the desktop that this is what its surface holds,
and only while it shows a pair:

| Windowing system | Declaration |
|---|---|
| Wayland | `kde_stereo_content_v1.set_content(side_by_side_full)` (value 3) on the window's surface. `wp_viewporter` gives the surface the size of one eye (a declared surface is placed at one view's size); the buffer is the whole pair. Global announced: `kde_stereo_content_manager_v1`. |
| X11 | the window property `_KDE_NET_WM_STEREO_CONTENT` (3), when the root window announces `_KDE_NET_WM_STEREO_CONTENT_SUPPORTED` version 2 or later. From version 3 the compositor doubles the window of a program that declares itself, so the window is created at one eye's size. |

The desktop decides what each screen shows (a display's 3D mode, a virtual 3D mode, or the left
eye on a screen that is 2D). The window has no output formats and no anaglyph: it never mixes the
eyes. Where the desktop announces no declaration, the window shows the left eye only, so two
squeezed pictures never appear. A picture that is not a pair is shown as it is and never
declared; the declaration is removed when a pair gives way to one.

`protocols/kde-stereo-content-v1.xml` is the protocol description of Sparky Stereo OS's
`plasma-wayland-protocols` fork, vendored byte for byte (SPDX MIT-CMU, Daniel Campos Ramos; a
permissive licence, kept as it is). It has one packing, `side_by_side_full` (3), and no class
request. The crate's own code is MIT OR Apache-2.0.

Everything runs on the window's own thread and display connection, with the Wayland client
libraries (`smithay-client-toolkit`, `wayland-client`, MIT) and `x11rb` (MIT OR Apache-2.0), no
`unsafe`.
