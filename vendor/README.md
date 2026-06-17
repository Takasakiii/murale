# Vendored Dependencies

## libmpv2 (v6.0.0)

**Source:** https://crates.io/crates/libmpv2/6.0.0

**Patch:** `RenderContext.ctx` field changed from private to `pub` in
`src/mpv/render.rs`.

**Why:** murale needs to call `mpv_render_context_render()` directly via
`libmpv2_sys` FFI to pass `MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME = 0`.
The upstream `RenderContext::render()` method omits this parameter, causing
mpv to block the render thread until PTS display time. On high-refresh
displays (240Hz) this blocks for up to 40ms per frame, causing severe stutter.

With the `ctx` field public, murale can access the raw
`*mut mpv_render_context` pointer and call the C API directly while still
using the safe wrapper for context creation, update callbacks, and teardown.

**Updating:** To update to a newer libmpv2 release:

1. Copy the new crate source into `vendor/libmpv2/`
2. Change `ctx: *mut libmpv2_sys::mpv_render_context` to `pub ctx:` in
   `src/mpv/render.rs` (line 12)
3. Run `cargo build` to verify compatibility
