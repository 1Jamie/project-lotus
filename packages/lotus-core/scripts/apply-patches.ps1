# PowerShell script to patch servo for Windows
$ErrorActionPreference = "Stop"

# The script is in 'packages/lotus-core/scripts/'. The target file is relative to that.
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$TargetFile = Join-Path $ScriptDir ".."
$TargetFile = Join-Path $TargetFile "servo"
$TargetFile = Join-Path $TargetFile "components"
$TargetFile = Join-Path $TargetFile "shared"
$TargetFile = Join-Path $TargetFile "paint"
$TargetFile = Join-Path $TargetFile "rendering_context.rs"

Write-Host "[Lotus] Applying Servo engine patches (Windows PowerShell)..."

if (-not (Test-Path $TargetFile)) {
    Write-Host "[Error] rendering_context.rs not found at $TargetFile"
    exit 1
}

$content = Get-Content $TargetFile -Raw

# Normalize line endings to LF for reliable replacement
$content = $content.Replace("`r`n", "`n")

# --- Define Old and New Strings ---

$old_type = "type RenderToParentCallback = Box<dyn Fn(&glow::Context, Rect<i32>) + Send + Sync>;"
$new_type = "type RenderToParentCallback = Box<dyn Fn(&glow::Context, Rect<i32>, u32) + Send + Sync>;"

$old_function_block = @"
        let parent_context_framebuffer_id = self.parent_context.surfman_context.framebuffer();
        let size = self.size.get();
        let size = Size2D::new(size.width as i32, size.height as i32);
        Some(Box::new(move |gl, target_rect| {
            Self::blit_framebuffer(
                gl,
                Rect::new(Point2D::origin(), size.to_i32()),
                front_framebuffer_id,
                target_rect,
                parent_context_framebuffer_id,
            );
        }))
"@

$new_function_block = @"
        let size = self.size.get();
        let size = Size2D::new(size.width as i32, size.height as i32);
        Some(Box::new(move |gl, target_rect, target_fbo| {
            let target_framebuffer_id = NonZeroU32::new(target_fbo).map(NativeFramebuffer);
            Self::blit_framebuffer(
                gl,
                Rect::new(Point2D::origin(), size.to_i32()),
                front_framebuffer_id,
                target_rect,
                target_framebuffer_id,
            );
        }))
"@

# Normalize the multi-line string blocks to LF
$old_function_block = $old_function_block.Replace("`r`n", "`n")
$new_function_block = $new_function_block.Replace("`r`n", "`n")

# --- Check if Patch is Already Applied ---
if ($content.Contains($new_type)) {
    Write-Host "[Lotus] Servo patch seems to be already applied. Skipping."
    exit 0
}

# --- Apply Patches ---
$original_content_for_check = $content
$content = $content.Replace($old_type, $new_type)
$content = $content.Replace($old_function_block, $new_function_block)

if ($content -eq $original_content_for_check) {
     Write-Host "[Warning] Failed to apply PowerShell patch. The source code may have changed."
     exit 0
}

# Write the file back with LF endings, which rustc handles correctly.
Set-Content -Path $TargetFile -Value $content -NoNewline -Encoding UTF8

Write-Host "[Lotus] Servo patches applied successfully via PowerShell."
