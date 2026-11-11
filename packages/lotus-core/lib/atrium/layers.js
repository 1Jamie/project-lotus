'use strict';

/** Semantic layer → zIndex band (plugins pick a layer; shell assigns concrete z). */
const LAYER_BANDS = {
  game: { min: 0, max: 0 },
  hud: { min: 100, max: 299 },
  drawer: { min: 300, max: 699 },
  shell: { min: 900, max: 999 },
};

const _nextInBand = {
  hud: 100,
  drawer: 300,
  shell: 900,
};

function resetLayerCounters() {
  _nextInBand.hud = 100;
  _nextInBand.drawer = 300;
  _nextInBand.shell = 900;
}

/**
 * Allocate a zIndex inside the semantic band.
 * @param {'game'|'hud'|'drawer'|'shell'} layer
 * @param {number} [preferred] optional fixed z inside band
 */
function zIndexForLayer(layer, preferred) {
  if (layer === 'game') return 0;
  const band = LAYER_BANDS[layer] || LAYER_BANDS.hud;
  if (Number.isFinite(preferred)) {
    return Math.min(band.max, Math.max(band.min, preferred));
  }
  const key = LAYER_BANDS[layer] ? layer : 'hud';
  let z = _nextInBand[key];
  if (z > band.max) z = band.min;
  _nextInBand[key] = z + 1;
  return z;
}

module.exports = {
  LAYER_BANDS,
  resetLayerCounters,
  zIndexForLayer,
};
