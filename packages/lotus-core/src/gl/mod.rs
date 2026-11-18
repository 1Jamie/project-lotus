pub mod compositor;
#[cfg(target_os = "linux")]
pub mod fsr1;
#[cfg(target_os = "linux")]
pub mod anime4k;
pub mod pane_context;
#[cfg(target_os = "linux")]
pub mod shm_consumer;
#[cfg(not(target_os = "linux"))]
pub mod shm_consumer_stub;
#[cfg(target_os = "linux")]
pub mod webgl_broker;

pub use compositor::{
    ack_presented_scene, atrium_host_present, blit_scene_to_window, composite_panes_to_scene,
    ensure_textures, scene_needs_present, CompositeRemoteResult,
};
#[cfg(target_os = "linux")]
pub use compositor::wait_retired_sample_fence;
pub use pane_context::LotusPaneContext;
#[cfg(target_os = "linux")]
pub use shm_consumer::RemotePaneConsumer;
#[cfg(not(target_os = "linux"))]
pub use shm_consumer_stub::RemotePaneConsumer;
