import { convertFileSrc } from '@tauri-apps/api/core';

export const defaultBackgroundSettings = Object.freeze({
  enabled: false,
  resourceId: '',
  fileName: '',
  mediaType: 'image',
  recentAssets: [],
  fit: 'cover',
  blur: 0,
  opacity: 1,
  darkOverlay: 0.35,
  lightOverlay: 0.18,
});

const fits = new Set(['cover', 'contain', 'stretch', 'center', 'tile']);
const mediaTypes = new Set(['image', 'video']);
const clamp = (value, min, max, fallback) => Number.isFinite(Number(value))
  ? Math.min(max, Math.max(min, Number(value)))
  : fallback;

export function normalizeBackgroundAsset(value = {}) {
  const resourceId = String(value?.resourceId || '');
  if (!resourceId) return null;
  return {
    resourceId,
    fileName: String(value?.fileName || ''),
    mediaType: mediaTypes.has(value?.mediaType) ? value.mediaType : 'image',
  };
}

export function normalizeBackgroundSettings(value = {}) {
  const normalizedRecentAssets = Array.isArray(value?.recentAssets)
    ? value.recentAssets.map(normalizeBackgroundAsset).filter(Boolean)
    : [];
  const activeAsset = normalizeBackgroundAsset(value);
  if (activeAsset && !mediaTypes.has(value?.mediaType)) {
    const recentMatch = normalizedRecentAssets.find((asset) => asset.resourceId === activeAsset.resourceId);
    if (recentMatch) {
      activeAsset.mediaType = recentMatch.mediaType;
      activeAsset.fileName ||= recentMatch.fileName;
    }
  }
  const recentAssets = [];
  const seenResourceIds = new Set();
  const appendAsset = (asset) => {
    const normalized = normalizeBackgroundAsset(asset);
    if (!normalized || seenResourceIds.has(normalized.resourceId) || recentAssets.length >= 3) return;
    seenResourceIds.add(normalized.resourceId);
    recentAssets.push(normalized);
  };
  activeAsset && appendAsset(activeAsset);
  normalizedRecentAssets.forEach(appendAsset);

  return {
    enabled: value?.enabled === true,
    resourceId: activeAsset?.resourceId || '',
    fileName: activeAsset?.fileName || '',
    mediaType: activeAsset?.mediaType || 'image',
    recentAssets,
    fit: fits.has(value?.fit) ? value.fit : 'cover',
    blur: clamp(value?.blur, 0, 40, 0),
    opacity: clamp(value?.opacity, 0, 1, 1),
    darkOverlay: clamp(value?.darkOverlay, 0, 1, 0.35),
    lightOverlay: clamp(value?.lightOverlay, 0, 1, 0.18),
  };
}

export function resolveBackgroundUrl(path = '') {
  if (!path) return '';
  try { return convertFileSrc(path); } catch { return ''; }
}
