use winit::window::Window;

#[cfg(target_os = "linux")]
pub fn set_always_on_top(window: &Window, always_on_top: bool) {
    use winit::window::WindowLevel;
    window.set_window_level(if always_on_top {
        WindowLevel::AlwaysOnTop
    } else {
        WindowLevel::Normal
    });
}

#[cfg(target_os = "windows")]
pub fn set_always_on_top(_window: &Window, _always_on_top: bool) {
    // Windows doesn't support this via winit directly in 0.29
    // Would need raw Win32 API calls with SetWindowPos
    eprintln!("Warning: set_always_on_top not fully supported on Windows via winit 0.29");
}

#[cfg(target_os = "macos")]
pub fn set_always_on_top(window: &Window, always_on_top: bool) {
    use winit::window::WindowLevel;
    window.set_window_level(if always_on_top {
        WindowLevel::AlwaysOnTop
    } else {
        WindowLevel::Normal
    });
}

#[cfg(target_os = "linux")]
pub fn request_attention(window: &Window) {
    use winit::window::UserAttentionType;
    window.request_user_attention(Some(UserAttentionType::Critical));
}

#[cfg(target_os = "windows")]
pub fn request_attention(window: &Window) {
    use winit::window::UserAttentionType;
    window.request_user_attention(Some(UserAttentionType::Critical));
}

#[cfg(target_os = "macos")]
pub fn request_attention(window: &Window) {
    use winit::window::UserAttentionType;
    window.request_user_attention(Some(UserAttentionType::Critical));
}
