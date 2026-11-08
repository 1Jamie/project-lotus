(function() {
    let updateTimeout = null;
    function updateDragRegions() {
        if (updateTimeout) clearTimeout(updateTimeout);
        updateTimeout = setTimeout(() => {
            const dragRects = [];
            const noDragRects = [];
            const dpr = window.devicePixelRatio || 1;
            
            // Support both data attributes (Lotus standard) and computed CSS (Electron compatibility)
            // We iterate over everything that might be a region to ensure CSS support.
            const candidates = document.querySelectorAll('*');
            candidates.forEach(el => {
                const style = getComputedStyle(el);
                const region = style.webkitAppRegion;
                const isDragAttr = el.getAttribute('data-lotus-drag') === 'true';
                const isNoDragAttr = el.getAttribute('data-lotus-drag') === 'false';

                if (region === 'drag' || isDragAttr) {
                    const rect = el.getBoundingClientRect();
                    const dpr = window.devicePixelRatio || 1;
                    dragRects.push({ x: rect.x * dpr, y: rect.y * dpr, width: rect.width * dpr, height: rect.height * dpr });
                } else if (region === 'no-drag' || isNoDragAttr) {
                    const rect = el.getBoundingClientRect();
                    const dpr = window.devicePixelRatio || 1;
                    noDragRects.push({ x: rect.x * dpr, y: rect.y * dpr, width: rect.width * dpr, height: rect.height * dpr });
                }
            });
            
            if (window.lotus && window.lotus.send) {
                window.lotus.send('lotus:set-drag-regions', { 
                    paneId: window.lotus.paneId, 
                    drag: dragRects, 
                    noDrag: noDragRects 
                });
            }
        }, 32); // Slightly higher debounce for full-tree scan
    }

    function initObservers() {
        if (!document.body) return;
        
        const observer = new MutationObserver((mutations) => {
            let shouldUpdate = false;
            for (const mutation of mutations) {
                if (mutation.type === 'childList') {
                    shouldUpdate = true;
                    break;
                }
                if (mutation.type === 'attributes') {
                    shouldUpdate = true; // Any attribute change might affect style/drag
                    break;
                }
            }
            if (shouldUpdate) updateDragRegions();
        });

        observer.observe(document.body, { 
            childList: true, 
            subtree: true, 
            attributes: true
        });

        const resizeObserver = new ResizeObserver(updateDragRegions);
        resizeObserver.observe(document.body);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            updateDragRegions();
            initObservers();
        });
    } else {
        updateDragRegions();
        initObservers();
    }
})();
