(function() {
    if (window.__lotus_scrollbar_initialized) return;
    window.__lotus_scrollbar_initialized = true;

    const config = window.__LOTUS_SCROLLBAR_CONFIG__ || {
        theme: 'auto',
        autoHide: true,
        width: 10
    };

    const width = Math.max(2, config.width || 10);
    const autoHide = config.autoHide !== false;
    const theme = config.theme || 'auto';

    let trackY = null;
    let thumbY = null;
    let trackX = null;
    let thumbX = null;
    let dragBlocker = null;

    let isDraggingY = false;
    let isDraggingX = false;
    let fadeTimeout = null;
    let resizeObserver = null;

    function enableDragBlocker() {
        if (!dragBlocker) {
            dragBlocker = document.createElement('div');
            dragBlocker.id = '__lotus_scroll_drag_blocker';
            dragBlocker.style.cssText = 'position:fixed!important;top:0!important;left:0!important;width:100%!important;height:100%!important;z-index:2147483647!important;background:transparent!important;cursor:default!important;-webkit-app-region:no-drag!important;pointer-events:auto!important;';
            dragBlocker.setAttribute('data-lotus-drag', 'false');
        }
        if (!dragBlocker.parentNode && document.documentElement) {
            document.documentElement.appendChild(dragBlocker);
        }
        document.documentElement.classList.add('__lotus_scrolling_active');
    }

    function disableDragBlocker() {
        if (dragBlocker && dragBlocker.parentNode) {
            dragBlocker.parentNode.removeChild(dragBlocker);
        }
        if (document.documentElement) {
            document.documentElement.classList.remove('__lotus_scrolling_active');
        }
    }

    function showScrollbars() {
        if (trackY && trackY.style.display !== 'none') trackY.style.opacity = '1';
        if (trackX && trackX.style.display !== 'none') trackX.style.opacity = '1';
    }

    function scheduleFade() {
        if (!autoHide || isDraggingY || isDraggingX) return;
        clearTimeout(fadeTimeout);
        fadeTimeout = setTimeout(() => {
            if (!isDraggingY && !isDraggingX) {
                if (trackY) trackY.style.opacity = '0';
                if (trackX) trackX.style.opacity = '0';
            }
        }, 800);
    }

    function updateGeometry() {
        if (!trackY || !trackX) return;
        const scrollEl = document.scrollingElement || document.documentElement;
        if (!scrollEl) return;

        const clientH = window.innerHeight;
        const clientW = window.innerWidth;
        const scrollH = scrollEl.scrollHeight;
        const scrollW = scrollEl.scrollWidth;

        const hasOverflowY = scrollH > clientH + 1;
        const hasOverflowX = scrollW > clientW + 1;

        // Collision avoidance when both scrollbars appear
        if (hasOverflowY && hasOverflowX) {
            trackY.style.height = `calc(100% - ${width}px)`;
            trackX.style.width = `calc(100% - ${width}px)`;
        } else {
            trackY.style.height = '100%';
            trackX.style.width = '100%';
        }

        // Vertical scrollbar geometry
        if (!hasOverflowY) {
            trackY.style.display = 'none';
        } else {
            trackY.style.display = 'block';
            const availableTrackH = hasOverflowX ? clientH - width : clientH;
            const thumbH = Math.max(24, Math.round((availableTrackH / scrollH) * availableTrackH));
            const maxScrollTop = scrollH - clientH;
            const maxThumbTop = Math.max(0, availableTrackH - thumbH);
            const thumbTop = maxScrollTop > 0 ? Math.round((scrollEl.scrollTop / maxScrollTop) * maxThumbTop) : 0;

            thumbY.dataset.top = String(thumbTop);
            thumbY.dataset.height = String(thumbH);
            thumbY.style.height = `${thumbH}px`;
            thumbY.style.transform = `translate3d(0, ${thumbTop}px, 0)`;
        }

        // Horizontal scrollbar geometry
        if (!hasOverflowX) {
            trackX.style.display = 'none';
        } else {
            trackX.style.display = 'block';
            const availableTrackW = hasOverflowY ? clientW - width : clientW;
            const thumbW = Math.max(24, Math.round((availableTrackW / scrollW) * availableTrackW));
            const maxScrollLeft = scrollW - clientW;
            const maxThumbLeft = Math.max(0, availableTrackW - thumbW);
            const thumbLeft = maxScrollLeft > 0 ? Math.round((scrollEl.scrollLeft / maxScrollLeft) * maxThumbLeft) : 0;

            thumbX.dataset.left = String(thumbLeft);
            thumbX.dataset.width = String(thumbW);
            thumbX.style.width = `${thumbW}px`;
            thumbX.style.transform = `translate3d(${thumbLeft}px, 0, 0)`;
        }
    }

    function initScrollbars() {
        const root = document.documentElement;
        if (!root) {
            document.addEventListener('DOMContentLoaded', initScrollbars, { once: true });
            return;
        }

        if (document.getElementById('__lotus_scroll_track_y')) return;

        // Scoped Overlay Scrollbar Styles
        const style = document.createElement('style');
        style.id = '__lotus_scrollbar_styles';
        style.textContent = `
            #__lotus_scroll_track_y, #__lotus_scroll_track_x {
                position: fixed !important;
                z-index: 2147483646 !important;
                pointer-events: auto !important;
                user-select: none !important;
                -webkit-user-select: none !important;
                -webkit-app-region: no-drag !important;
                background: transparent !important;
                box-sizing: border-box !important;
                opacity: 0;
                transition: opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_y {
                top: 0 !important;
                right: 0 !important;
                width: ${width}px !important;
                height: 100% !important;
                transition: width 0.15s ease, opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_y:hover,
            #__lotus_scroll_track_y.active {
                width: ${Math.max(width, 12)}px !important;
            }
            #__lotus_scroll_track_x {
                bottom: 0 !important;
                left: 0 !important;
                height: ${width}px !important;
                width: 100% !important;
                transition: height 0.15s ease, opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_x:hover,
            #__lotus_scroll_track_x.active {
                height: ${Math.max(width, 12)}px !important;
            }
            #__lotus_scroll_thumb_y, #__lotus_scroll_thumb_x {
                position: absolute !important;
                border-radius: ${width}px !important;
                box-sizing: border-box !important;
                -webkit-app-region: no-drag !important;
                pointer-events: auto !important;
                cursor: default !important;
                will-change: transform;
                transition: width 0.15s ease, height 0.15s ease, background 0.15s ease;
            }
            #__lotus_scroll_thumb_y {
                top: 0 !important;
                right: 2px !important;
                left: auto !important;
                width: ${Math.max(4, width - 4)}px !important;
            }
            #__lotus_scroll_track_y:hover #__lotus_scroll_thumb_y,
            #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y {
                width: ${Math.max(width - 2, 8)}px !important;
            }
            #__lotus_scroll_thumb_x {
                left: 0 !important;
                bottom: 2px !important;
                top: auto !important;
                height: ${Math.max(4, width - 4)}px !important;
            }
            #__lotus_scroll_track_x:hover #__lotus_scroll_thumb_x,
            #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                height: ${Math.max(width - 2, 8)}px !important;
            }

            /* Disable iframes pointer events while dragging scrollbars */
            .__lotus_scrolling_active iframe {
                pointer-events: none !important;
            }

            /* Light theme */
            [data-theme="light"] #__lotus_scroll_thumb_y,
            [data-theme="light"] #__lotus_scroll_thumb_x,
            .__lotus_theme_light #__lotus_scroll_thumb_y,
            .__lotus_theme_light #__lotus_scroll_thumb_x {
                background: rgba(0, 0, 0, 0.35) !important;
            }
            [data-theme="light"] #__lotus_scroll_thumb_y:hover,
            [data-theme="light"] #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            [data-theme="light"] #__lotus_scroll_thumb_x:hover,
            [data-theme="light"] #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(0, 0, 0, 0.65) !important;
            }

            /* Dark theme */
            [data-theme="dark"] #__lotus_scroll_thumb_y,
            [data-theme="dark"] #__lotus_scroll_thumb_x,
            .__lotus_theme_dark #__lotus_scroll_thumb_y,
            .__lotus_theme_dark #__lotus_scroll_thumb_x {
                background: rgba(255, 255, 255, 0.35) !important;
            }
            [data-theme="dark"] #__lotus_scroll_thumb_y:hover,
            [data-theme="dark"] #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            [data-theme="dark"] #__lotus_scroll_thumb_x:hover,
            [data-theme="dark"] #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(255, 255, 255, 0.65) !important;
            }

            /* Neutral fallback */
            #__lotus_scroll_thumb_y, #__lotus_scroll_thumb_x {
                background: rgba(127, 127, 127, 0.45) !important;
            }
            #__lotus_scroll_thumb_y:hover,
            #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            #__lotus_scroll_thumb_x:hover,
            #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(127, 127, 127, 0.8) !important;
            }
        `;
        root.appendChild(style);

        // Vertical Track & Thumb
        trackY = document.createElement('div');
        trackY.id = '__lotus_scroll_track_y';
        trackY.setAttribute('data-lotus-drag', 'false');

        thumbY = document.createElement('div');
        thumbY.id = '__lotus_scroll_thumb_y';
        thumbY.setAttribute('data-lotus-drag', 'false');
        trackY.appendChild(thumbY);

        // Horizontal Track & Thumb
        trackX = document.createElement('div');
        trackX.id = '__lotus_scroll_track_x';
        trackX.setAttribute('data-lotus-drag', 'false');

        thumbX = document.createElement('div');
        thumbX.id = '__lotus_scroll_thumb_x';
        thumbX.setAttribute('data-lotus-drag', 'false');
        trackX.appendChild(thumbX);

        if (theme === 'light') {
            trackY.classList.add('__lotus_theme_light');
            trackX.classList.add('__lotus_theme_light');
        } else if (theme === 'dark') {
            trackY.classList.add('__lotus_theme_dark');
            trackX.classList.add('__lotus_theme_dark');
        }

        root.appendChild(trackY);
        root.appendChild(trackX);

        // Thumb Y Drag Handling
        thumbY.addEventListener('mousedown', (e) => {
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientH = window.innerHeight;
            const scrollH = scrollEl.scrollHeight;
            const hasOverflowX = scrollEl.scrollWidth > window.innerWidth + 1;
            const availableTrackH = hasOverflowX ? clientH - width : clientH;
            const thumbH = Math.max(24, Math.round((availableTrackH / scrollH) * availableTrackH));
            const maxThumbTop = Math.max(0, availableTrackH - thumbH);
            const maxScrollTop = scrollH - clientH;
            if (maxThumbTop <= 0 || maxScrollTop <= 0) return;

            const startMouseY = e.clientY;
            const startScrollTop = scrollEl.scrollTop;
            const ratio = maxScrollTop / maxThumbTop;

            isDraggingY = true;
            clearTimeout(fadeTimeout);
            trackY.classList.add('active');
            showScrollbars();
            enableDragBlocker();

            function onMouseMove(moveEvent) {
                const deltaY = moveEvent.clientY - startMouseY;
                scrollEl.scrollTop = startScrollTop + deltaY * ratio;
            }

            function onMouseUp() {
                isDraggingY = false;
                trackY.classList.remove('active');
                disableDragBlocker();
                window.removeEventListener('mousemove', onMouseMove, true);
                window.removeEventListener('mouseup', onMouseUp, true);
                scheduleFade();
            }

            window.addEventListener('mousemove', onMouseMove, true);
            window.addEventListener('mouseup', onMouseUp, true);
        });

        // Track Y Click (Deadzone checked jump)
        trackY.addEventListener('mousedown', (e) => {
            if (e.target === thumbY) return;
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientH = window.innerHeight;
            const clickY = e.clientY;
            const currentThumbTop = parseFloat(thumbY.dataset.top || '0');
            const thumbH = parseFloat(thumbY.dataset.height || '24');

            if (clickY < currentThumbTop - 2) {
                scrollEl.scrollTop -= clientH * 0.9;
            } else if (clickY > currentThumbTop + thumbH + 2) {
                scrollEl.scrollTop += clientH * 0.9;
            }
        });

        // Thumb X Drag Handling
        thumbX.addEventListener('mousedown', (e) => {
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientW = window.innerWidth;
            const scrollW = scrollEl.scrollWidth;
            const hasOverflowY = scrollEl.scrollHeight > window.innerHeight + 1;
            const availableTrackW = hasOverflowY ? clientW - width : clientW;
            const thumbW = Math.max(24, Math.round((availableTrackW / scrollW) * availableTrackW));
            const maxThumbLeft = Math.max(0, availableTrackW - thumbW);
            const maxScrollLeft = scrollW - clientW;
            if (maxThumbLeft <= 0 || maxScrollLeft <= 0) return;

            const startMouseX = e.clientX;
            const startScrollLeft = scrollEl.scrollLeft;
            const ratio = maxScrollLeft / maxThumbLeft;

            isDraggingX = true;
            clearTimeout(fadeTimeout);
            trackX.classList.add('active');
            showScrollbars();
            enableDragBlocker();

            function onMouseMove(moveEvent) {
                const deltaX = moveEvent.clientX - startMouseX;
                scrollEl.scrollLeft = startScrollLeft + deltaX * ratio;
            }

            function onMouseUp() {
                isDraggingX = false;
                trackX.classList.remove('active');
                disableDragBlocker();
                window.removeEventListener('mousemove', onMouseMove, true);
                window.removeEventListener('mouseup', onMouseUp, true);
                scheduleFade();
            }

            window.addEventListener('mousemove', onMouseMove, true);
            window.addEventListener('mouseup', onMouseUp, true);
        });

        // Track X Click (Deadzone checked jump)
        trackX.addEventListener('mousedown', (e) => {
            if (e.target === thumbX) return;
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientW = window.innerWidth;
            const clickX = e.clientX;
            const currentThumbLeft = parseFloat(thumbX.dataset.left || '0');
            const thumbW = parseFloat(thumbX.dataset.width || '24');

            if (clickX < currentThumbLeft - 2) {
                scrollEl.scrollLeft -= clientW * 0.9;
            } else if (clickX > currentThumbLeft + thumbW + 2) {
                scrollEl.scrollLeft += clientW * 0.9;
            }
        });

        // Hover & Edge detection
        trackY.addEventListener('mouseenter', showScrollbars);
        trackY.addEventListener('mouseleave', scheduleFade);
        trackX.addEventListener('mouseenter', showScrollbars);
        trackX.addEventListener('mouseleave', scheduleFade);

        window.addEventListener('mousemove', (e) => {
            if (window.innerWidth - e.clientX <= Math.max(20, width + 10) ||
                window.innerHeight - e.clientY <= Math.max(20, width + 10)) {
                showScrollbars();
                scheduleFade();
            }
        }, { passive: true });

        // Scroll listener
        window.addEventListener('scroll', () => {
            updateGeometry();
            showScrollbars();
            scheduleFade();
        }, { passive: true });

        // Window resize
        window.addEventListener('resize', () => {
            updateGeometry();
            showScrollbars();
            scheduleFade();
        }, { passive: true });

        // Observers
        if (window.ResizeObserver) {
            resizeObserver = new ResizeObserver(() => {
                updateGeometry();
            });
            resizeObserver.observe(root);
            if (document.body) {
                resizeObserver.observe(document.body);
            }
        }

        updateGeometry();
        if (!autoHide) {
            showScrollbars();
        } else {
            showScrollbars();
            scheduleFade();
        }
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            initScrollbars();
            if (document.body && resizeObserver) {
                resizeObserver.observe(document.body);
            }
            updateGeometry();
        }, { once: true });
    } else {
        initScrollbars();
    }
})();
