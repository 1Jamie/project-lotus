//! Windows stub: sandboxed remote panes / SHM consumers are Linux-only.
//! Keeps shared host code compiling without pulling lotus-supervisor.

pub struct RemotePaneConsumer {
    pub last_rendered_seq: u64,
    pub tex_width: u32,
    pub tex_height: u32,
    pub max_width: u32,
    pub max_height: u32,
}

impl RemotePaneConsumer {
    pub fn new(
        _ro_fd: i32,
        _mapped_size: usize,
        _ack_rw_fd: i32,
        _max_width: u32,
        _max_height: u32,
    ) -> Result<Self, &'static str> {
        Err("remote pane SHM is only supported on Linux")
    }

    pub fn update_active_resize_gen(&mut self, _gen: u64) {}

    pub fn begin_plane_handoff(
        &mut self,
        _ro_fd: i32,
        _mapped_size: usize,
        _max_width: u32,
        _max_height: u32,
    ) -> Result<(), &'static str> {
        Err("remote pane SHM is only supported on Linux")
    }

    pub fn try_promote_pending_plane(&mut self) -> bool {
        false
    }

    pub fn has_pending_plane(&self) -> bool {
        false
    }

    pub fn staging_frames_since_resize(&self) -> u32 {
        0
    }

    pub fn has_unread_frame(&self) -> bool {
        false
    }

    pub fn latest_upload_size(&self) -> (u32, u32) {
        (0, 0)
    }

    pub fn consume_latest_frame(
        &mut self,
        _gl: &glow::Context,
        _preserve_display: bool,
    ) -> Result<bool, &'static str> {
        Err("remote pane SHM is only supported on Linux")
    }

    pub fn promote_staging_to_display(&mut self, _gl: &glow::Context) {}

    pub fn promote_painted_or_discard_staging(&mut self, _gl: &glow::Context) {}

    pub fn discard_staging(&mut self, _gl: &glow::Context) {}

    pub fn get_texture(&self) -> Option<glow::Texture> {
        None
    }

    pub fn invalidate_cached_frame(&mut self, _gl: &glow::Context) {}
}
